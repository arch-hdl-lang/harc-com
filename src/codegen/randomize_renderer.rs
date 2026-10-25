//! Backend-neutral C++ rendering for TBIR randomization.
//!
//! This renderer owns only the state needed to turn lowered TBIR constraint
//! sites and record schemas into C++ snippets.  The retiring native backend
//! keeps its compatibility renderer; TBIR must not construct its monolithic
//! emitter.
//!
//! The solver methods intentionally mirror the v1 implementation until that
//! backend is deleted.  `codegen::randomize` has an exact-output parity test
//! covering both solver sites and record helpers so the two copies cannot
//! drift silently during the retirement window.

use crate::ast::*;
use crate::codegen::emission::{EmitError, EmitOpts};
use crate::codegen::lifecycle_transform::desugar_impl_for_test_in_file;
use crate::codegen::native::ConstraintOperandSignedness;
use crate::codegen::randomize::{
    PerRunCell, TBIR_RANDOMIZE_CONTEXT_SENTINEL, TBIR_RANDOMIZE_SEQUENCER_SCOPE_SENTINEL,
    TBIR_RANDOMIZE_STATE_SENTINEL, TBIR_RANDOMIZE_TARGET_SENTINEL,
    TBIR_RANDOMIZE_TSEQ_SCOPE_SENTINEL,
};
use crate::codegen::render_support::{c_ident, cpp_sint_for_width, cpp_uint_for_width};
use crate::codegen::source_transform::*;
use crate::lexer::Span;
use std::fmt::Write;

const INDENT: &str = "    ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Legacy,
    Capsule,
}

fn stable_identity_hash(domain: &str, parts: &[&[u8]]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    };
    feed(domain.as_bytes());
    for part in parts {
        feed(&(part.len() as u64).to_le_bytes());
        feed(part);
    }
    hash
}

fn global_const_cpp_name(name: &str) -> String {
    format!(
        "harc_user_const_{}_{:016x}",
        c_ident(name),
        stable_identity_hash("harc-user-const-v1", &[name.as_bytes()])
    )
}

fn global_enum_variant_cpp_name(name: &str) -> String {
    format!(
        "harc_user_enum_{}_{:016x}",
        c_ident(name),
        stable_identity_hash("harc-user-enum-v1", &[name.as_bytes()])
    )
}

#[derive(Debug, Clone, Copy)]
struct StableSourceIdentity {
    tag: u64,
    site_seed: u64,
}

fn stable_source_identities(
    file: &SourceFile,
) -> std::collections::HashMap<SourceId, StableSourceIdentity> {
    file.sources
        .iter()
        .map(|source| {
            let parts = [source.name.as_bytes()];
            (
                source.id,
                StableSourceIdentity {
                    tag: stable_identity_hash("harc-source-tag-v2", &parts),
                    site_seed: stable_identity_hash("harc-source-site-v2", &parts),
                },
            )
        })
        .collect()
}

fn stable_component_randomize_site_id(
    source: StableSourceIdentity,
    owner: &str,
    span: Span,
) -> u64 {
    stable_identity_hash(
        "harc-component-randomize-site-v1",
        &[
            &source.site_seed.to_le_bytes(),
            owner.as_bytes(),
            &span.start.to_le_bytes(),
            &span.end.to_le_bytes(),
        ],
    )
}

fn component_randomize_site_owners(
    file: &SourceFile,
) -> std::collections::HashMap<SourceSite, String> {
    crate::solver::problem_table::build_component_scope_problem_table(file)
        .entries
        .into_iter()
        .filter_map(|entry| match entry.source {
            crate::solver::problem_table::TypedSolverProblemSource::RandomizeSite {
                context,
                source_id,
                span,
                ..
            } => Some((SourceSite::new(source_id, span), context)),
            _ => None,
        })
        .collect()
}

/// Width-typed transaction field info used by the Z3 solver lowering.
#[derive(Debug, Clone)]
pub(super) struct TxnFieldInfo {
    name: String,
    path: Vec<String>,
    width: u32,
    signed: bool,
    enum_variants: Option<usize>,
    enum_variant_labels: Option<Vec<String>>,
    list: Option<ListFieldInfo>,
    /// `!` prefix on a transaction field — pinned to the current value during
    /// solver-backed randomize and skipped during model assignment.
    non_random: bool,
    attrs: Vec<Attr>,
    when_guard: Option<Expr>,
}

#[derive(Debug, Clone)]
struct ListFieldInfo {
    declared_max_len: Option<usize>,
    elem_width: u32,
    elem_signed: bool,
}

fn txn_field_info_from_field_path(
    f: &Field,
    path: Vec<String>,
    when_guard: Option<Expr>,
    enums: &std::collections::HashMap<String, usize>,
    enum_domains: &std::collections::HashMap<String, Vec<String>>,
) -> TxnFieldInfo {
    let list = list_field_info(&f.ty);
    let (width, signed, enum_variants, enum_variant_labels) = match &f.ty {
        TypeExpr::Builtin { name, args, .. } => match name {
            BuiltinTy::UInt | BuiltinTy::Bits | BuiltinTy::UIntCap => {
                (type_arg_width(args).unwrap_or(64), false, None, None)
            }
            BuiltinTy::SInt | BuiltinTy::SIntCap => {
                (type_arg_width(args).unwrap_or(64), true, None, None)
            }
            BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => (1, false, None, None),
            BuiltinTy::Int => (32, true, None, None),
            _ => (64, false, None, None),
        },
        TypeExpr::Named { name, .. } => {
            let last = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
            if let Some(&n) = enums.get(last) {
                (
                    enum_width(n),
                    false,
                    Some(n),
                    enum_domains.get(last).cloned(),
                )
            } else {
                (0, false, None, None)
            }
        }
    };
    TxnFieldInfo {
        name: path.join("."),
        path,
        width,
        signed,
        enum_variants,
        enum_variant_labels,
        list,
        non_random: f.non_random,
        attrs: f.attrs.clone(),
        when_guard,
    }
}

fn flatten_txn_body_field_infos(
    items: &[TxnBodyItem],
    record_fields: &std::collections::HashMap<String, Vec<Field>>,
    enums: &std::collections::HashMap<String, usize>,
    enum_domains: &std::collections::HashMap<String, Vec<String>>,
) -> Vec<TxnFieldInfo> {
    let mut out = Vec::new();
    flatten_txn_body_field_infos_inner(
        items,
        Vec::new(),
        None,
        record_fields,
        enums,
        enum_domains,
        &mut out,
    );
    out
}

fn flatten_txn_body_field_infos_inner(
    items: &[TxnBodyItem],
    prefix: Vec<String>,
    when_guard: Option<Expr>,
    record_fields: &std::collections::HashMap<String, Vec<Field>>,
    enums: &std::collections::HashMap<String, usize>,
    enum_domains: &std::collections::HashMap<String, Vec<String>>,
    out: &mut Vec<TxnFieldInfo>,
) {
    for item in items {
        match item {
            TxnBodyItem::Field(f) => flatten_record_field_info_one(
                f,
                prefix.clone(),
                when_guard.clone(),
                record_fields,
                enums,
                enum_domains,
                out,
            ),
            TxnBodyItem::When(w) => {
                let next_guard = match &when_guard {
                    Some(g) => and_join(&[g.clone(), w.discriminant.clone()], w.span),
                    None => w.discriminant.clone(),
                };
                flatten_txn_body_field_infos_inner(
                    &w.items,
                    prefix.clone(),
                    Some(next_guard),
                    record_fields,
                    enums,
                    enum_domains,
                    out,
                );
            }
            TxnBodyItem::Keep(_) => {}
        }
    }
}

fn flatten_record_field_infos(
    fields: &[Field],
    record_fields: &std::collections::HashMap<String, Vec<Field>>,
    enums: &std::collections::HashMap<String, usize>,
    enum_domains: &std::collections::HashMap<String, Vec<String>>,
) -> Vec<TxnFieldInfo> {
    let mut out = Vec::new();
    flatten_record_field_infos_inner(
        fields,
        Vec::new(),
        record_fields,
        enums,
        enum_domains,
        &mut out,
    );
    out
}

fn flatten_record_field_infos_inner(
    fields: &[Field],
    prefix: Vec<String>,
    record_fields: &std::collections::HashMap<String, Vec<Field>>,
    enums: &std::collections::HashMap<String, usize>,
    enum_domains: &std::collections::HashMap<String, Vec<String>>,
    out: &mut Vec<TxnFieldInfo>,
) {
    for f in fields {
        flatten_record_field_info_one(
            f,
            prefix.clone(),
            None,
            record_fields,
            enums,
            enum_domains,
            out,
        );
    }
}

fn flatten_record_field_info_one(
    f: &Field,
    prefix: Vec<String>,
    when_guard: Option<Expr>,
    record_fields: &std::collections::HashMap<String, Vec<Field>>,
    enums: &std::collections::HashMap<String, usize>,
    enum_domains: &std::collections::HashMap<String, Vec<String>>,
    out: &mut Vec<TxnFieldInfo>,
) {
    let mut path = prefix;
    path.push(f.name.name.clone());
    if list_field_info(&f.ty).is_none() {
        if let Some(record) = record_type_name(&f.ty).and_then(|name| record_fields.get(name)) {
            if !enums.contains_key(record_type_name(&f.ty).unwrap_or_default()) {
                flatten_record_field_infos_inner(
                    record,
                    path,
                    record_fields,
                    enums,
                    enum_domains,
                    out,
                );
                return;
            }
        }
    }
    out.push(txn_field_info_from_field_path(
        f,
        path,
        when_guard,
        enums,
        enum_domains,
    ));
}

#[derive(Debug, Clone)]
struct AutoCoverageGoal {
    field: String,
    c_field: String,
    values: Vec<AutoCoverageValue>,
}

#[derive(Debug, Clone)]
struct AutoCoverageValue {
    label: String,
    c_expr: String,
    words: Vec<u32>,
}

impl AutoCoverageValue {
    fn unsigned(value: u64) -> Self {
        Self {
            label: value.to_string(),
            c_expr: format!("{value}ULL"),
            words: vec![value as u32, (value >> 32) as u32],
        }
    }

    fn signed(value: i64) -> Self {
        Self {
            label: value.to_string(),
            c_expr: if value == i64::MIN {
                "INT64_MIN".to_string()
            } else if value < 0 {
                format!("{value}LL")
            } else {
                format!("{value}LL")
            },
            words: vec![value as u32, ((value as u64) >> 32) as u32],
        }
    }

    fn from_words(label: String, words: Vec<u32>) -> Self {
        let c_expr = if words.len() <= 2 {
            let lo = words.first().copied().unwrap_or(0) as u64;
            let hi = words.get(1).copied().unwrap_or(0) as u64;
            format!("{}ULL", lo | (hi << 32))
        } else if words.len() <= 4 {
            let mut terms = Vec::new();
            for (idx, word) in words.iter().enumerate() {
                if *word == 0 {
                    continue;
                }
                terms.push(format!("((_harc_u128)0x{word:08x}ULL << {})", idx * 32));
            }
            if terms.is_empty() {
                "(_harc_u128)0".to_string()
            } else {
                format!("({})", terms.join(" | "))
            }
        } else {
            format!(
                "harc_rt::HarcWide<{}>({{{}}})",
                words.len(),
                words
                    .iter()
                    .map(|w| format!("0x{w:08x}u"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        Self {
            label,
            c_expr,
            words,
        }
    }
}

const AUTO_COVERAGE_WALKING_BIT_CAP: usize = 16;

fn field_attr_range(f: &TxnFieldInfo) -> Option<(&Expr, &Expr)> {
    f.attrs.iter().find_map(|a| {
        if a.name.name != "range" || a.args.len() < 2 {
            return None;
        }
        match (&a.args[0], &a.args[1]) {
            (AttrArg::Expr(lo), AttrArg::Expr(hi)) => Some((lo, hi)),
            _ => None,
        }
    })
}

fn field_attr_dist_entries(f: &TxnFieldInfo) -> Option<&Vec<DistEntry>> {
    f.attrs.iter().find_map(|a| {
        if a.name.name != "dist" {
            return None;
        }
        a.args.iter().find_map(|arg| match arg {
            AttrArg::Dist(entries) => Some(entries),
            _ => None,
        })
    })
}

fn field_attr_unique(f: &TxnFieldInfo) -> bool {
    f.attrs.iter().any(|a| a.name.name == "unique")
}

fn field_attr_unique_scope(f: &TxnFieldInfo) -> Option<&str> {
    f.attrs
        .iter()
        .find(|a| a.name.name == "unique")
        .and_then(|a| {
            a.args.iter().find_map(|arg| match arg {
                AttrArg::WithinScope(scope) => Some(scope.name.as_str()),
                _ => None,
            })
        })
}

fn push_auto_coverage_value_unique(values: &mut Vec<AutoCoverageValue>, value: AutoCoverageValue) {
    if !values.iter().any(|v| v.words == value.words) {
        values.push(value);
    }
}

fn walking_auto_coverage_bit_positions(width: u32) -> Vec<u32> {
    if width == 0 {
        return Vec::new();
    }
    let width_usize = width as usize;
    let count = width_usize.min(AUTO_COVERAGE_WALKING_BIT_CAP);
    if count == width_usize {
        return (0..width).collect();
    }

    let mut positions = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let max_pos = width_usize - 1;
    let max_idx = count - 1;
    for i in 0..count {
        let pos = (i * max_pos + (max_idx / 2)) / max_idx;
        if seen.insert(pos) {
            positions.push(pos as u32);
        }
    }
    positions
}

fn unsigned_max_words(width: u32) -> Vec<u32> {
    let word_count = (width as usize).div_ceil(32).max(1);
    let mut words = vec![0xffff_ffffu32; word_count];
    let rem = width % 32;
    if rem != 0 {
        if let Some(last) = words.last_mut() {
            *last = (1u32 << rem) - 1;
        }
    }
    words
}

fn solver_unsigned_mask_expr(width: u32) -> String {
    let words = unsigned_max_words(width);
    if words.len() <= 2 {
        let lo = words.first().copied().unwrap_or(0) as u64;
        let hi = words.get(1).copied().unwrap_or(0) as u64;
        format!("(uint64_t)0x{:016x}ULL", lo | (hi << 32))
    } else if words.len() <= 4 {
        let mut terms = Vec::new();
        for (idx, word) in words.iter().enumerate() {
            if *word != 0 {
                terms.push(format!("((_harc_u128)0x{word:08x}ULL << {})", idx * 32));
            }
        }
        if terms.is_empty() {
            "(_harc_u128)0".to_string()
        } else {
            format!("({})", terms.join(" | "))
        }
    } else {
        format!(
            "harc_rt::HarcWide<{}>({{{}}})",
            words.len(),
            words
                .iter()
                .map(|w| format!("0x{w:08x}u"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

fn signed_min_words(width: u32) -> Vec<u32> {
    let mut words = vec![0u32; (width as usize).div_ceil(32).max(1)];
    let sign_bit = width.saturating_sub(1);
    words[(sign_bit / 32) as usize] = 1u32 << (sign_bit % 32);
    words
}

fn signed_max_words(width: u32) -> Vec<u32> {
    let mut words = unsigned_max_words(width);
    let sign_bit = width.saturating_sub(1);
    words[(sign_bit / 32) as usize] &= !(1u32 << (sign_bit % 32));
    words
}

fn decimal_label_for_auto_words(width: u32, words: &[u32]) -> String {
    if width <= 64 {
        let lo = words.first().copied().unwrap_or(0) as u64;
        let hi = words.get(1).copied().unwrap_or(0) as u64;
        return (lo | (hi << 32)).to_string();
    }
    if words.iter().all(|w| *w == 0) {
        return "0".to_string();
    }
    if words.iter().enumerate().all(|(idx, w)| {
        let max = unsigned_max_words(width);
        max.get(idx).copied().unwrap_or(0) == *w
    }) {
        return format!("2^{}-1", width);
    }
    if let Some((idx, word)) = words
        .iter()
        .enumerate()
        .find(|(_, word)| **word != 0 && word.count_ones() == 1)
    {
        if words
            .iter()
            .enumerate()
            .all(|(other_idx, other)| other_idx == idx || *other == 0)
        {
            let bit = idx as u32 * 32 + word.trailing_zeros();
            return format!("2^{bit}");
        }
    }
    if words.iter().enumerate().all(|(idx, word)| {
        let mut max = unsigned_max_words(width);
        let diff = max.get_mut(idx).map(|m| {
            let d = *m ^ *word;
            d != 0 && d.count_ones() == 1
        });
        let this_is_diff = diff.unwrap_or(false);
        this_is_diff
            || max
                .get(idx)
                .copied()
                .map(|m| m == *word)
                .unwrap_or(*word == 0)
    }) {
        let max = unsigned_max_words(width);
        for (idx, (a, b)) in max.iter().zip(words.iter()).enumerate() {
            let diff = *a ^ *b;
            if diff != 0 && diff.count_ones() == 1 {
                let bit = idx as u32 * 32 + diff.trailing_zeros();
                return format!("2^{}-1-2^{bit}", width);
            }
        }
    }
    "wide".to_string()
}

fn auto_coverage_unsigned_word(value_words: Vec<u32>, width: u32) -> AutoCoverageValue {
    let label = decimal_label_for_auto_words(width, &value_words);
    AutoCoverageValue::from_words(label, value_words)
}

fn auto_coverage_signed_word(value_words: Vec<u32>, width: u32, is_min: bool) -> AutoCoverageValue {
    let label = if is_min {
        format!("-2^{}", width.saturating_sub(1))
    } else {
        format!("2^{}-1", width.saturating_sub(1))
    };
    AutoCoverageValue::from_words(label, value_words)
}

fn signed_domain_bound_exprs(width: u32) -> (String, String) {
    if width == 64 {
        ("INT64_MIN".to_string(), "INT64_MAX".to_string())
    } else if width < 64 {
        (
            format!("(int64_t)(-(1LL << {}))", width.saturating_sub(1)),
            format!("(int64_t)((1LL << {}) - 1)", width.saturating_sub(1)),
        )
    } else {
        (
            AutoCoverageValue::from_words(String::new(), signed_min_words(width)).c_expr,
            AutoCoverageValue::from_words(String::new(), signed_max_words(width)).c_expr,
        )
    }
}

fn solver_bv_value_call(value: &str, signed: bool, value_width: u32, solver_width: u32) -> String {
    if signed {
        format!("harc_z3_bv_signed_value(_ctx, {value}, {value_width}, {solver_width})")
    } else {
        format!("harc_z3_bv_value(_ctx, {value}, {solver_width})")
    }
}

fn natural_auto_coverage_endpoints(f: &TxnFieldInfo) -> Vec<AutoCoverageValue> {
    if f.width == 0 {
        return Vec::new();
    }

    if f.signed {
        if f.width > 64 {
            return vec![
                auto_coverage_signed_word(signed_min_words(f.width), f.width, true),
                auto_coverage_signed_word(signed_max_words(f.width), f.width, false),
            ];
        }
        let shift = f.width.saturating_sub(1);
        let lo = if shift >= 63 {
            i64::MIN
        } else {
            -(1i64 << shift)
        };
        let hi = if shift >= 63 {
            i64::MAX
        } else {
            (1i64 << shift) - 1
        };
        let mut values = vec![AutoCoverageValue::signed(lo)];
        if hi != lo {
            values.push(AutoCoverageValue::signed(hi));
        }
        values
    } else {
        let hi_words = unsigned_max_words(f.width);
        let mut values = Vec::new();
        push_auto_coverage_value_unique(
            &mut values,
            auto_coverage_unsigned_word(vec![0; hi_words.len()], f.width),
        );
        push_auto_coverage_value_unique(
            &mut values,
            auto_coverage_unsigned_word(hi_words.clone(), f.width),
        );
        for bit in walking_auto_coverage_bit_positions(f.width) {
            let mut one = vec![0u32; hi_words.len()];
            one[(bit / 32) as usize] = 1u32 << (bit % 32);
            push_auto_coverage_value_unique(&mut values, auto_coverage_unsigned_word(one, f.width));
            let mut inv = hi_words.clone();
            inv[(bit / 32) as usize] ^= 1u32 << (bit % 32);
            push_auto_coverage_value_unique(&mut values, auto_coverage_unsigned_word(inv, f.width));
        }
        values
    }
}

/// A mathematical literal interval whose values agree with the emitted
/// bit-vector comparisons. Do not reinterpret unsigned literals as negative
/// signed bounds, or optimize expressions whose constant value is unknown.
fn literal_range_preference(f: &TxnFieldInfo, salt: usize, solver_width: u32) -> Option<String> {
    fn bound(e: &Expr, solver_width: u32) -> Option<i128> {
        match &*e.kind {
            ExprKind::Int(s) => {
                let value = parse_int_str(s)?;
                // Wide constraint literals currently pass through an int64_t
                // carrier and sign-extend. Do not optimize a different interval.
                if solver_width > 64 && value > i64::MAX as u64 {
                    return None;
                }
                Some(i128::from(value))
            }
            ExprKind::Paren(inner) => bound(inner, solver_width),
            ExprKind::Unary {
                op: UnaryOp::Neg,
                expr,
            } => bound(expr, solver_width)?.checked_neg(),
            _ => None,
        }
    }
    fn signed_literal(v: i128) -> String {
        if v == i64::MIN as i128 {
            "(-9223372036854775807LL - 1)".to_string()
        } else {
            format!("{v}LL")
        }
    }
    let (lo, hi) = field_attr_range(f)?;
    let (lo, hi) = (bound(lo, solver_width)?, bound(hi, solver_width)?);
    if f.width == 0 {
        return None;
    }
    if f.signed {
        if f.width > 64 || lo < i64::MIN as i128 || hi > i64::MAX as i128 {
            return None;
        }
        let half = 1i128 << (f.width - 1);
        let (lo, hi) = (lo.max(-half), hi.min(half - 1));
        if lo > hi {
            return None;
        }
        Some(format!(
            "harc_rt::random::harc_prefer_range_i64(_harc_rt_seed, {salt}, {}, {})",
            signed_literal(lo),
            signed_literal(hi)
        ))
    } else {
        if lo < 0 || hi > u64::MAX as i128 {
            return None;
        }
        let max = if f.width >= 64 {
            u64::MAX as i128
        } else {
            (1i128 << f.width) - 1
        };
        let hi = hi.min(max);
        if lo > hi {
            return None;
        }
        Some(format!(
            "harc_rt::random::harc_prefer_range_u64(_harc_rt_seed, {salt}, {lo}ULL, {hi}ULL)"
        ))
    }
}

fn auto_coverage_values(f: &TxnFieldInfo) -> Vec<AutoCoverageValue> {
    let mut values = Vec::new();
    if let Some(n) = f.enum_variants {
        let labels = f.enum_variant_labels.as_deref();
        for i in 0..n {
            if let Some(label) = labels.and_then(|labels| labels.get(i)) {
                values.push(AutoCoverageValue {
                    label: label.clone(),
                    c_expr: format!("{i}ULL"),
                    words: vec![i as u32, 0],
                });
            } else {
                values.push(AutoCoverageValue::unsigned(i as u64));
            }
        }
    } else if f.width == 1 && !f.signed {
        values.extend([0, 1].map(AutoCoverageValue::unsigned));
    } else if let Some((lo, hi)) = field_attr_range(f) {
        if f.signed {
            if let (Some(lo), Some(hi)) = (fold_signed_int_literal(lo), fold_signed_int_literal(hi))
            {
                values.push(AutoCoverageValue::signed(lo));
                if hi != lo {
                    values.push(AutoCoverageValue::signed(hi));
                }
            }
        } else if let (Some(lo), Some(hi)) = (fold_int_literal(lo), fold_int_literal(hi)) {
            values.push(AutoCoverageValue::unsigned(lo));
            if hi != lo {
                values.push(AutoCoverageValue::unsigned(hi));
            }
        }
    } else {
        values.extend(natural_auto_coverage_endpoints(f));
    }
    values
}

/// Bits needed to hold `v` — a literal is self-sized, and `0` is one bit.
fn value_bit_width(v: u64) -> u32 {
    if v == 0 {
        1
    } else {
        64 - v.leading_zeros()
    }
}

/// Self-width of a literal operand in a constraint, for the §2.4 wrap
/// mask. Everything unsized is sized by value through the statement
/// path's own `parse_int_literal`, so the same literal sizes identically
/// in both positions.
///
/// An ARCH sized literal states its width outright, and that width is
/// what §2.4 masks at — the same declared-width rule a `const` follows,
/// so the two agree about a token like `8'd300` that can be spelled
/// either way. The digits are not consulted, and since harc#565 they no
/// longer can EXCEED the declared width: the parser rejects a literal
/// whose value does not fit. The two are not equal — `8'h0F` declares 8
/// and needs 4, and the mask follows the 8 — but the declared width is
/// now guaranteed to be wide enough for the value, which is the property
/// that was missing. Before that fix this function had to choose,
/// and either choice was wrong somewhere — masking at the declared width
/// turned `keep (len +% 4'hFF) == 4'hFF` on a `uint<4>` field from
/// solvable into unsatisfiable, and masking at the value's width did the
/// same to `keep len +% 4'hFF == 15`.
///
/// Not parsing the digits also keeps a literal whose value overflows
/// `u64` (`128'hFFFF...`) resolvable, where its declared width is all
/// the mask ever needed.
fn literal_operand_bit_width(s: &str) -> Option<u32> {
    let Some(idx) = s.find('\'') else {
        return crate::ir::lower::parse_int_literal(s).map(value_bit_width);
    };
    // A zero declared width no longer parses (harc#565), so the old
    // `.max(1)` clamp is gone. It was load-bearing, not cosmetic: a zero
    // reaching `solver_unsigned_mask_expr` yields a bogus 32-bits-of-ones
    // mask. Assert the coupling rather than leaving a dead clamp that
    // hides it.
    let declared: u32 = s[..idx].replace('_', "").parse().ok()?;
    debug_assert!(
        declared > 0,
        "a zero-width sized literal must be rejected by the parser (harc#565)"
    );
    Some(declared)
}

/// Raw declared width of a width-carrying builtin type, for a `const`'s
/// wrap-operand width. Deliberately NOT `cast_relabel_width`: that helper
/// caps at 128 and returns `None` both for "not a width type" and for
/// "out of range", and a caller that falls back on `None` cannot tell
/// those apart — `const K : uint<200> = 10` would silently take the
/// initializer's 4-bit value width instead of being rejected as
/// unrepresentable. A non-width type still returns `None`, which is a
/// real "no declared width" and correctly falls back.
fn txn_field_solver_width(f: &TxnFieldInfo) -> u32 {
    f.list
        .as_ref()
        .map(|list| list.elem_width)
        .unwrap_or(f.width)
}

fn txn_field_solver_c_type(f: &TxnFieldInfo) -> String {
    if f.signed {
        cpp_sint_for_width(Some(f.width))
    } else {
        cpp_uint_for_width(Some(f.width))
    }
}

fn solver_scalar_c_type(width: u32, signed: bool) -> String {
    if signed {
        cpp_sint_for_width(Some(width))
    } else {
        cpp_uint_for_width(Some(width))
    }
}

fn auto_value_array_type(
    goal: &AutoCoverageGoal,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
) -> String {
    field_info
        .get(&goal.field)
        .map(txn_field_solver_c_type)
        .unwrap_or_else(|| "uint64_t".to_string())
}

fn auto_value_initializer(values: &[AutoCoverageValue]) -> String {
    values
        .iter()
        .map(|v| v.c_expr.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

fn emit_random_pref_expr(f: &TxnFieldInfo, salt: usize) -> String {
    if f.width <= 128 {
        format!(
            "harc_rt::random::harc_prefer_u128(_harc_rt_seed, {}, {})",
            salt, f.width
        )
    } else {
        let words = f.width.div_ceil(32);
        format!(
            "harc_rt::random::harc_prefer_wide<{}>(_harc_rt_seed, {}, {})",
            words, salt, f.width
        )
    }
}

fn emit_random_unsigned_expr(width: u32) -> String {
    if width <= 64 {
        format!("harc_rt::random::harc_rng_uint(harc_rng_next, {width})")
    } else if width <= 128 {
        format!("harc_rt::random::harc_rng_u128(harc_rng_next, {width})")
    } else {
        let words = width.div_ceil(32);
        format!(
            "harc_rt::random::harc_rng_wide<{}>(harc_rng_next, {width})",
            words
        )
    }
}

fn emit_random_signed_expr(width: u32) -> String {
    if width < 63 {
        format!(
            "harc_rt::random::harc_rng_range(harc_rng_next, -(1LL << {}), (1LL << {}) - 1)",
            width.saturating_sub(1),
            width.saturating_sub(1)
        )
    } else if width == 63 {
        // The full sint<63> domain has a 2^63 span, which overflows the
        // signed span arithmetic in harc_rng_range. Draw and sign-extend.
        "static_cast<int64_t>(harc_rt::harc_sext_u128(static_cast<_harc_u128>(harc_rt::random::harc_rng_uint(harc_rng_next, 63)), 63, 64))".to_string()
    } else {
        emit_random_unsigned_expr(width)
    }
}

fn c_solver_int_literal(s: &str) -> String {
    let value = c_value_literal(s);
    if value.starts_with("harc_rt::HarcWide<") || value.contains("_harc_u128") {
        value
    } else {
        format!("(int64_t)({})", c_int_literal(s))
    }
}

fn randomize_target_field_name(
    target: &Expr,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
) -> Option<String> {
    let field = expr_field_path(target, target_root)?;
    field_info.contains_key(&field).then_some(field)
}

fn randomize_target_ident(target: &Expr) -> Option<&str> {
    match &*target.kind {
        ExprKind::Ident(id) => Some(id.name.as_str()),
        ExprKind::Paren(inner) => randomize_target_ident(inner),
        _ => None,
    }
}

/// Element signedness of `sum(<list>[lo..hi])`, or `false` for any other
/// call.
///
/// Recognition mirrors `try_emit_constraint_list_call`'s `sum` arm — same
/// callee name, same single `CallArg::Expr`, same `Index` target resolved
/// through `list_field_name_from_expr`. The two must agree about what
/// counts as a `sum` over a list: if this one is narrower the comparison
/// goes back to unsigned (harc#598), and if it is wider it claims
/// signedness for something the emitter lowers some other way. Deliberately
/// does NOT require the index to be a `RangeLit` — the emitter diagnoses a
/// non-range index itself, and a signedness answer for an expression that
/// is about to be rejected is irrelevant either way.
fn sum_call_elem_signed(
    e: &Expr,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
) -> bool {
    let ExprKind::Call { callee, args } = &*e.kind else {
        return false;
    };
    let ExprKind::Ident(name) = &*callee.kind else {
        return false;
    };
    if name.name != "sum" || args.len() != 1 {
        return false;
    }
    let CallArg::Expr(arg) = &args[0] else {
        return false;
    };
    // BOTH argument shapes the emitter accepts. It takes `sum(vals[lo..hi])`
    // through an `Index` and `sum(vals)` through a bare fall-through, and
    // handling only the first left `sum(vals)` over a signed list still
    // emitting `z3::ult` — harc#598 unfixed in its sibling syntax, which is
    // exactly the drift the doc above warns about.
    let list_expr = match &*arg.kind {
        ExprKind::Index { target, .. } => target,
        _ => arg,
    };
    list_field_name_from_expr(list_expr, field_info, target_root)
        .and_then(|field| field_info.get(&field))
        .and_then(|f| f.list.as_ref())
        .map(|l| l.elem_signed)
        .unwrap_or(false)
}

fn list_field_name_from_expr(
    e: &Expr,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
) -> Option<String> {
    let field = expr_field_path(e, target_root)?;
    field_info
        .get(&field)
        .and_then(|info| info.list.as_ref().map(|_| field))
}

fn expr_field_path(e: &Expr, target_root: Option<&str>) -> Option<String> {
    let mut parts = Vec::new();
    collect_expr_field_path(e, &mut parts)?;
    if parts.is_empty() {
        return None;
    }
    if let Some(root) = target_root {
        if parts.first().is_some_and(|part| part == root) {
            parts.remove(0);
        }
    }
    Some(parts.join("."))
}

fn expr_field_root(e: &Expr) -> Option<String> {
    let mut parts = Vec::new();
    collect_expr_field_path(e, &mut parts)?;
    parts.first().cloned()
}

fn collect_expr_field_path(e: &Expr, out: &mut Vec<String>) -> Option<()> {
    match &*e.kind {
        ExprKind::Ident(id) => out.push(id.name.clone()),
        ExprKind::ImplicitSelf => {}
        ExprKind::Field { target, name } => {
            collect_expr_field_path(target, out)?;
            out.push(name.name.clone());
        }
        ExprKind::Paren(inner) => collect_expr_field_path(inner, out)?,
        _ => return None,
    }
    Some(())
}

fn list_len_call_field_name(
    e: &Expr,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
) -> Option<String> {
    let ExprKind::Call { callee, args } = &*e.kind else {
        return None;
    };
    if !args.is_empty() {
        return None;
    }
    let ExprKind::Field { target, name } = &*callee.kind else {
        return None;
    };
    if name.name != "len" {
        return None;
    }
    list_field_name_from_expr(target, field_info, target_root)
}

fn const_usize_from_constraint_expr(e: &Expr) -> Option<usize> {
    const_usize_expr(e)
}

fn list_len_upper_bound_from_expr(
    e: &Expr,
    field: &str,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
) -> Option<usize> {
    match &*e.kind {
        ExprKind::Paren(inner) => {
            list_len_upper_bound_from_expr(inner, field, field_info, target_root)
        }
        ExprKind::Membership { expr, set } => {
            if list_len_call_field_name(expr, field_info, target_root).as_deref() != Some(field) {
                return None;
            }
            match &*set.kind {
                ExprKind::RangeLit { hi: Some(hi), .. } => const_usize_from_constraint_expr(hi),
                ExprKind::SetLit(items) => items
                    .iter()
                    .filter_map(|item| match &*item.kind {
                        ExprKind::RangeLit { hi: Some(hi), .. } => {
                            const_usize_from_constraint_expr(hi)
                        }
                        _ => const_usize_from_constraint_expr(item),
                    })
                    .max(),
                _ => const_usize_from_constraint_expr(set),
            }
        }
        ExprKind::Binary { op, lhs, rhs } => {
            if matches!(op, BinaryOp::OrOr) && guarded_constraint_guard(e).is_some() {
                return list_len_upper_bound_from_expr(rhs, field, field_info, target_root);
            }
            let lhs_is_len =
                list_len_call_field_name(lhs, field_info, target_root).as_deref() == Some(field);
            let rhs_is_len =
                list_len_call_field_name(rhs, field_info, target_root).as_deref() == Some(field);
            match op {
                BinaryOp::Le if lhs_is_len => const_usize_from_constraint_expr(rhs),
                BinaryOp::Lt if lhs_is_len => {
                    const_usize_from_constraint_expr(rhs).and_then(|v| v.checked_sub(1))
                }
                BinaryOp::Eq if lhs_is_len => const_usize_from_constraint_expr(rhs),
                BinaryOp::Ge if rhs_is_len => const_usize_from_constraint_expr(lhs),
                BinaryOp::Gt if rhs_is_len => {
                    const_usize_from_constraint_expr(lhs).and_then(|v| v.checked_sub(1))
                }
                BinaryOp::Eq if rhs_is_len => const_usize_from_constraint_expr(lhs),
                BinaryOp::AndAnd | BinaryOp::AndKw => {
                    let a = list_len_upper_bound_from_expr(lhs, field, field_info, target_root);
                    let b = list_len_upper_bound_from_expr(rhs, field, field_info, target_root);
                    match (a, b) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (Some(a), None) => Some(a),
                        (None, Some(b)) => Some(b),
                        (None, None) => None,
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn infer_list_unroll_bounds(
    fields: &[TxnFieldInfo],
    hard_constraints: &[&Expr],
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
) -> std::collections::HashMap<String, usize> {
    let mut out = std::collections::HashMap::new();
    for f in fields {
        let Some(list) = &f.list else {
            continue;
        };
        let mut bound = list.declared_max_len;
        for c in hard_constraints {
            if let Some(next) = list_len_upper_bound_from_expr(c, &f.name, field_info, target_root)
            {
                bound = Some(bound.map_or(next, |old| old.min(next)));
            }
        }
        if let Some(bound) = bound {
            out.insert(f.name.clone(), bound);
        }
    }
    out
}

fn collect_randomize_target_field_refs(
    e: &Expr,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
    out: &mut std::collections::HashSet<String>,
) {
    match &*e.kind {
        ExprKind::Ident(id) => {
            if field_info.contains_key(&id.name) {
                out.insert(id.name.clone());
            }
        }
        ExprKind::Field { target, .. } => {
            if let Some(field) = expr_field_path(e, target_root) {
                if field_info.contains_key(&field) {
                    out.insert(field);
                    return;
                }
            }
            collect_randomize_target_field_refs(target, field_info, target_root, out);
        }
        ExprKind::Index { target, index } => {
            collect_randomize_target_field_refs(target, field_info, target_root, out);
            collect_randomize_target_field_refs(index, field_info, target_root, out);
        }
        ExprKind::BitSlice { target, hi, lo } => {
            collect_randomize_target_field_refs(target, field_info, target_root, out);
            collect_randomize_target_field_refs(hi, field_info, target_root, out);
            collect_randomize_target_field_refs(lo, field_info, target_root, out);
        }
        ExprKind::Call { callee, args } => {
            collect_randomize_target_field_refs(callee, field_info, target_root, out);
            for arg in args {
                match arg {
                    CallArg::Expr(e) | CallArg::Named { value: e, .. } => {
                        collect_randomize_target_field_refs(e, field_info, target_root, out)
                    }
                }
            }
        }
        ExprKind::ForEachConstraint { iter, body, .. } => {
            collect_randomize_target_field_refs(iter, field_info, target_root, out);
            for clause in body {
                collect_randomize_target_field_refs(clause, field_info, target_root, out);
            }
        }
        ExprKind::Cast { expr, .. }
        | ExprKind::Unary { expr, .. }
        | ExprKind::Paren(expr)
        | ExprKind::ForkCall { call: expr }
        | ExprKind::HashHash { expr, .. }
        | ExprKind::SeqRepeat { expr, .. } => {
            collect_randomize_target_field_refs(expr, field_info, target_root, out);
        }
        ExprKind::Membership { expr, set } => {
            collect_randomize_target_field_refs(expr, field_info, target_root, out);
            collect_randomize_target_field_refs(set, field_info, target_root, out);
        }
        ExprKind::Send { target, value }
        | ExprKind::Binary {
            lhs: target,
            rhs: value,
            ..
        } => {
            collect_randomize_target_field_refs(target, field_info, target_root, out);
            collect_randomize_target_field_refs(value, field_info, target_root, out);
        }
        ExprKind::Ternary {
            cond,
            then_branch,
            else_branch,
        } => {
            collect_randomize_target_field_refs(cond, field_info, target_root, out);
            collect_randomize_target_field_refs(then_branch, field_info, target_root, out);
            collect_randomize_target_field_refs(else_branch, field_info, target_root, out);
        }
        ExprKind::RangeLit { lo, hi } => {
            if let Some(lo) = lo {
                collect_randomize_target_field_refs(lo, field_info, target_root, out);
            }
            if let Some(hi) = hi {
                collect_randomize_target_field_refs(hi, field_info, target_root, out);
            }
        }
        ExprKind::SetLit(items) => {
            for item in items {
                collect_randomize_target_field_refs(item, field_info, target_root, out);
            }
        }
        ExprKind::DistLit(entries) => {
            for entry in entries {
                collect_randomize_target_field_refs(&entry.value, field_info, target_root, out);
                collect_randomize_target_field_refs(&entry.weight, field_info, target_root, out);
            }
        }
        ExprKind::SystemCall { args, .. }
        | ExprKind::Randomize {
            with_body: args, ..
        } => {
            for arg in args {
                collect_randomize_target_field_refs(arg, field_info, target_root, out);
            }
        }
        ExprKind::SoftConstraint(sc) => {
            collect_randomize_target_field_refs(&sc.expr, field_info, target_root, out);
            if let Some(weight) = &sc.weight {
                collect_randomize_target_field_refs(weight, field_info, target_root, out);
            }
        }
        ExprKind::DistDirective { target, entries } => {
            collect_randomize_target_field_refs(target, field_info, target_root, out);
            for entry in entries {
                collect_randomize_target_field_refs(&entry.value, field_info, target_root, out);
                collect_randomize_target_field_refs(&entry.weight, field_info, target_root, out);
            }
        }
        ExprKind::NamedArg { value, .. } => {
            collect_randomize_target_field_refs(value, field_info, target_root, out);
        }
        ExprKind::StructLit { fields, .. } => {
            for field in fields {
                collect_randomize_target_field_refs(&field.value, field_info, target_root, out);
            }
        }
        ExprKind::CoverArrow { lhs, rhs, count } => {
            collect_randomize_target_field_refs(lhs, field_info, target_root, out);
            collect_randomize_target_field_refs(rhs, field_info, target_root, out);
            if let Some(count) = count {
                collect_hash_count_field_refs(count, field_info, target_root, out);
            }
        }
        ExprKind::SolveOrder { args } => {
            for arg in args {
                collect_randomize_target_field_refs(arg, field_info, target_root, out);
            }
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Time(_)
        | ExprKind::String(_)
        | ExprKind::Bool(_)
        | ExprKind::ImplicitSelf => {}
    }
}

fn collect_hash_count_field_refs(
    count: &HashCount,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
    out: &mut std::collections::HashSet<String>,
) {
    match count {
        HashCount::Const(e) => collect_randomize_target_field_refs(e, field_info, target_root, out),
        HashCount::Range { lo, hi } => {
            collect_randomize_target_field_refs(lo, field_info, target_root, out);
            collect_randomize_target_field_refs(hi, field_info, target_root, out);
        }
    }
}

fn list_max_len(t: &TypeExpr) -> Option<usize> {
    let args = list_type_args(t)?;
    for arg in args {
        if let TypeArg::Named { name, value } = arg {
            if name.name == "max" {
                return const_usize_expr(value);
            }
        }
    }
    args.get(1).and_then(type_arg_const_usize)
}

fn scalar_field_shape(t: &TypeExpr) -> (u32, bool) {
    match t {
        TypeExpr::Builtin { name, args, .. } => match name {
            BuiltinTy::UInt | BuiltinTy::Bits | BuiltinTy::UIntCap => {
                (type_arg_width(args).unwrap_or(64), false)
            }
            BuiltinTy::SInt | BuiltinTy::SIntCap => (type_arg_width(args).unwrap_or(64), true),
            BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => (1, false),
            BuiltinTy::Int => (32, true),
            _ => (64, false),
        },
        _ => (64, false),
    }
}

fn list_field_info(t: &TypeExpr) -> Option<ListFieldInfo> {
    if !is_list_type(t) {
        return None;
    }
    let elem = list_elem_type(t)?;
    let declared_max_len = list_max_len(t);
    let (elem_width, elem_signed) = scalar_field_shape(elem);
    Some(ListFieldInfo {
        declared_max_len,
        elem_width,
        elem_signed,
    })
}

fn type_arg_width(args: &[TypeArg]) -> Option<u32> {
    let arg = args.first()?;
    match arg {
        TypeArg::Expr(e) => match &*e.kind {
            ExprKind::Int(s) => s.replace('_', "").parse().ok(),
            _ => None,
        },
        _ => None,
    }
}

fn enum_width(variant_count: usize) -> u32 {
    if variant_count <= 1 {
        1
    } else {
        usize::BITS - (variant_count - 1).leading_zeros()
    }
}

/// field, an enum field and a list are all excluded.
#[derive(Clone, Copy, Debug)]
struct LetWidth {
    signed: bool,
}

fn is_flag_field(f: &TxnFieldInfo) -> bool {
    f.list.is_none() && f.enum_variants.is_none() && f.width == 1 && !f.signed
}

/// HARC spelling of an operator, for diagnostics. Distinct from
/// `c_binary_op`, which gives the C++ spelling and so collapses `+%` onto
/// `+` — an error message quoting the operator back to the author has to
/// use the one they wrote.
///
/// Every arm is spelled out rather than delegating the tail to
/// `c_binary_op`. That delegation was safe only by luck: `c_binary_op`
/// renders the temporal and membership operators as
/// `"/* unsupported-op */ ,"`, so the first person to add `in` or `|->` to
/// the caller's reject list would have shipped that string quoted inside a
/// user-facing diagnostic. Now the compiler makes them choose a spelling.
fn constraint_op_spelling(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Mod => "%",
        AddWrap => "+%",
        SubWrap => "-%",
        MulWrap => "*%",
        BitAnd => "&",
        BitOr => "|",
        BitXor => "^",
        Shl => "<<",
        Shr => ">>",
        Eq => "==",
        Ne => "!=",
        Lt => "<",
        Le => "<=",
        Gt => ">",
        Ge => ">=",
        AndAnd | AndKw => "&&",
        OrOr | OrKw => "||",
        In => "in",
        Inside => "inside",
        PipeImplies => "|->",
        PipeImpliesNext => "|=>",
        Throughout => "throughout",
        Within => "within",
        Intersect => "intersect",
    }
}

fn c_binary_op(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        Add | AddWrap => "+",
        Sub | SubWrap => "-",
        Mul | MulWrap => "*",
        Div => "/",
        Mod => "%",
        Eq => "==",
        Ne => "!=",
        Lt => "<",
        Le => "<=",
        Gt => ">",
        Ge => ">=",
        AndAnd | AndKw => "&&",
        OrOr | OrKw => "||",
        BitAnd => "&",
        BitOr => "|",
        BitXor => "^",
        Shl => "<<",
        Shr => ">>",
        // Temporal / membership operators have no direct C++ equivalent — they
        // shouldn't appear in v0 cpp_tb input. Emit a placeholder rather than
        // fail hard so the whole emit doesn't abort on one stray operator.
        PipeImplies | PipeImpliesNext | Throughout | Within | Intersect | In | Inside => {
            "/* unsupported-op */ ,"
        }
    }
}

/// Constant-fold a base/size expression to `u64` for the addrmap
/// overlap check. Handles plain integer literals only (hex/decimal/
/// underscored). Non-literal expressions fall back to `None` — the
/// overlap check skips pairs whose bounds aren't computable, which
/// keeps the static analysis honest without surfacing false
/// positives.
fn fold_int_literal(e: &Expr) -> Option<u64> {
    match &*e.kind {
        ExprKind::Int(s) => parse_int_str(s),
        _ => None,
    }
}

fn fold_signed_int_literal(e: &Expr) -> Option<i64> {
    match &*e.kind {
        ExprKind::Int(s) => i64::try_from(parse_int_str(s)?).ok(),
        ExprKind::Unary {
            op: UnaryOp::Neg,
            expr,
        } => i64::try_from(fold_int_literal(expr)?).ok().map(|v| -v),
        ExprKind::Paren(inner) => fold_signed_int_literal(inner),
        _ => None,
    }
}

fn parse_int_str(s: &str) -> Option<u64> {
    let cleaned: String = s.chars().filter(|c| *c != '_').collect();
    if let Some(idx) = cleaned.find('\'') {
        let (_, tail) = cleaned.split_at(idx + 1);
        let kind = tail.chars().next()?;
        let digits: &str = &tail[1..];
        match kind {
            'h' | 'H' => u64::from_str_radix(digits, 16).ok(),
            'b' | 'B' => u64::from_str_radix(digits, 2).ok(),
            'o' | 'O' => u64::from_str_radix(digits, 8).ok(),
            _ => digits.parse::<u64>().ok(),
        }
    } else if let Some(hex) = cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).ok()
    } else if let Some(bin) = cleaned
        .strip_prefix("0b")
        .or_else(|| cleaned.strip_prefix("0B"))
    {
        u64::from_str_radix(bin, 2).ok()
    } else {
        cleaned.parse::<u64>().ok()
    }
}

/// An ARCH-style sized literal (`8'hAB`, `4'd3`, `1'b1`) rewritten as
/// the plain C++ spelling this backend folds it to (`0xAB`, `3`, `0b1`)
/// — width prefix dropped, base letter turned into a C prefix,
/// underscores stripped. An unsized literal only loses its underscores.
///
/// `pub(crate)` because the width prefix is NOT the value: `4'd3` folds
/// to a correct `3`, while `128'hFF..FF` folds to a 128-bit composite
/// that a 64-bit member truncates. The TB-IR lowering classifies a
/// default it cannot read by asking what this produces and whether the
/// result fits, rather than by splitting on the apostrophe.
pub(crate) fn normalized_int_literal(s: &str) -> String {
    if let Some(idx) = s.find('\'') {
        let (_, tail) = s.split_at(idx + 1);
        let kind = tail.chars().next().unwrap_or('d');
        let digits: String = tail[1..].chars().filter(|c| *c != '_').collect();
        match kind {
            'h' | 'H' => format!("0x{digits}"),
            'b' | 'B' => format!("0b{digits}"),
            _ => digits,
        }
    } else {
        s.replace('_', "")
    }
}

fn c_int_literal(s: &str) -> String {
    // ARCH-style sized literals (`8'hAB`) → C++ `0xAB`. Then the
    // unsized-literal post-pass below catches any >64-bit cases.
    let normalized = normalized_int_literal(s);

    // Hex literals wider than 64 bits (>16 hex digits) overflow C++'s
    // unsigned long long. Lower as a composite `_harc_u128` shifted-OR
    // so the value flows through `harc_assign` / `harc_read` and 128-
    // bit comparisons. Two halves are sufficient for the v0 cap of
    // 128 bits; literals above that are clamped to the low 128 bits.
    let (prefix, digits) = if let Some(d) = normalized.strip_prefix("0x") {
        ("0x", d)
    } else if let Some(d) = normalized.strip_prefix("0X") {
        ("0x", d)
    } else {
        ("", normalized.as_str())
    };
    if !prefix.is_empty() && digits.len() > 16 && digits.len() <= 32 {
        // 65..128 bits — fits in `_harc_u128`. Split into two 64-bit
        // halves and emit the composite shifted-OR.
        let hex_lo_start = digits.len() - 16;
        let lo = &digits[hex_lo_start..];
        let hi = &digits[..hex_lo_start];
        return format!("(((_harc_u128)0x{hi}ULL << 64) | (_harc_u128)0x{lo}ULL)");
    }
    // > 128 bits: handled by `c_wide_lit_words` at the assign / equality
    // call sites — they emit `harc_assign_words` / `harc_eq_words` with
    // an `std::initializer_list<uint32_t>` instead. If we got here with
    // a > 128b literal it means the literal escaped into a context that
    // can't take a word-list (e.g. used as an arithmetic operand);
    // fall back to a clamped composite to keep the output compilable
    // with a clear narrowing warning rather than a crash.
    if !prefix.is_empty() && digits.len() > 32 {
        let lo = &digits[digits.len() - 16..];
        let hi = &digits[digits.len() - 32..digits.len() - 16];
        return format!("(((_harc_u128)0x{hi}ULL << 64) | (_harc_u128)0x{lo}ULL)");
    }
    normalized
}

fn c_value_literal(s: &str) -> String {
    if let Some(words) = c_wide_lit_words(s) {
        let mut out = format!("harc_rt::HarcWide<{}>({{", words.len());
        for (i, w) in words.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(w);
        }
        out.push_str("})");
        return out;
    }
    c_int_literal(s)
}

/// If the integer literal `s` is hex with > 32 hex digits (i.e. >
/// 128 bits), return its decomposition into 32-bit words in LSB-
/// first order — `vec!["0x...", "0x...", ...]`. Each string is a
/// `uint32_t` hex literal. Returns `None` for narrower or non-hex
/// literals; callers fall back to `c_int_literal` for those.
///
/// Used by the assign and equality lowering paths: a literal that
/// can't fit in `_harc_u128` flows through `harc_assign_words` /
/// `harc_eq_words` instead, which take `std::initializer_list<uint32_t>`.
fn c_wide_lit_words(s: &str) -> Option<Vec<String>> {
    let normalized = if let Some(idx) = s.find('\'') {
        let (_, tail) = s.split_at(idx + 1);
        let kind = tail.chars().next().unwrap_or('d');
        let digits: String = tail[1..].chars().filter(|c| *c != '_').collect();
        match kind {
            'h' | 'H' => format!("0x{digits}"),
            _ => return None,
        }
    } else {
        s.replace('_', "")
    };
    let hex = normalized
        .strip_prefix("0x")
        .or_else(|| normalized.strip_prefix("0X"))?;
    if hex.len() <= 32 {
        return None;
    }
    // Split into 8-hex-digit (32-bit) chunks, LSB-first.
    let mut words = Vec::new();
    let mut remaining = hex.len();
    while remaining > 0 {
        let start = remaining.saturating_sub(8);
        let chunk = &hex[start..remaining];
        words.push(format!("0x{chunk}u"));
        remaining = start;
    }
    Some(words)
}

fn escape_c(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out
}

/// Number of 32-bit words the *value* of a `c_wide_lit_words` word list
/// actually needs — i.e. one past the index of the highest non-zero
/// word (minimum 1). This is the value-based "required width" used by
/// the over-width assignment guard: leading-zero high words (a literal
/// written wider than necessary) do NOT inflate the count, so a value
/// that fits the port is never flagged, while a value with set bits
/// above the port width is.
///
/// `words` are LSB-first `uint32_t` hex literals like `"0x1800u"`, as
/// produced by `c_wide_lit_words`.

fn guarded_constraint_guard(expr: &Expr) -> Option<&Expr> {
    let ExprKind::Binary {
        op: BinaryOp::OrOr,
        lhs,
        ..
    } = &*expr.kind
    else {
        return None;
    };
    let ExprKind::Unary {
        op: UnaryOp::Not,
        expr,
    } = &*lhs.kind
    else {
        return None;
    };
    match &*expr.kind {
        ExprKind::Paren(inner) => Some(inner),
        _ => Some(expr),
    }
}

/// Combine a list of constraint expressions into one
/// `Binary(AndAnd, …)` chain. Used by `expand_relation_subtree` when a
/// block-form relation appears in a position that expects a single
/// expression (e.g. nested inside another `Binary &&`). Empty input
/// returns a literal `true`. Single-element input returns the element
/// unchanged.
/// Walk `expr` and replace each `ExprKind::Ident(name)` whose name is
/// in `subst` with the corresponding substitute expression (cloned in
/// place). Used by `expand_relation_calls` to bind each relation's
/// formal parameter to the actual call argument before the body is
/// added to the Z3 solver block.
///
/// The substitute can be any expression — a bare ident (the common
/// case: `R(pkt)` → `pkt`), a field access (`R(env.agent.txn)`), or
/// a deeper subtree. Field-access names (`Field { name, .. }`) are
/// attribute references, not bindings, so they're never substituted.
fn substitute_idents(expr: &Expr, subst: &std::collections::HashMap<String, Expr>) -> Expr {
    let span = expr.span;
    let new_kind: ExprKind = match &*expr.kind {
        ExprKind::Ident(id) => {
            if let Some(replacement) = subst.get(&id.name) {
                // Splice the replacement's *kind* in at the current
                // span. Keeping the original ident's span preserves
                // source attribution in any future diagnostic.
                return Expr::new((*replacement.kind).clone(), span);
            }
            ExprKind::Ident(id.clone())
        }
        ExprKind::Field { target, name } => ExprKind::Field {
            target: substitute_idents(target, subst),
            name: name.clone(),
        },
        ExprKind::Index { target, index } => ExprKind::Index {
            target: substitute_idents(target, subst),
            index: substitute_idents(index, subst),
        },
        ExprKind::BitSlice { target, hi, lo } => ExprKind::BitSlice {
            target: substitute_idents(target, subst),
            hi: substitute_idents(hi, subst),
            lo: substitute_idents(lo, subst),
        },
        ExprKind::Call { callee, args } => ExprKind::Call {
            callee: substitute_idents(callee, subst),
            args: args
                .iter()
                .map(|a| match a {
                    CallArg::Expr(e) => CallArg::Expr(substitute_idents(e, subst)),
                    CallArg::Named { name, value } => CallArg::Named {
                        name: name.clone(),
                        value: substitute_idents(value, subst),
                    },
                })
                .collect(),
        },
        ExprKind::ForEachConstraint { var, iter, body } => {
            let mut scoped = subst.clone();
            scoped.remove(&var.name);
            ExprKind::ForEachConstraint {
                var: var.clone(),
                iter: substitute_idents(iter, subst),
                body: body.iter().map(|e| substitute_idents(e, &scoped)).collect(),
            }
        }
        ExprKind::Cast { expr, ty } => ExprKind::Cast {
            expr: substitute_idents(expr, subst),
            ty: ty.clone(),
        },
        ExprKind::Unary { op, expr } => ExprKind::Unary {
            op: *op,
            expr: substitute_idents(expr, subst),
        },
        ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
            op: *op,
            lhs: substitute_idents(lhs, subst),
            rhs: substitute_idents(rhs, subst),
        },
        ExprKind::Ternary {
            cond,
            then_branch,
            else_branch,
        } => ExprKind::Ternary {
            cond: substitute_idents(cond, subst),
            then_branch: substitute_idents(then_branch, subst),
            else_branch: substitute_idents(else_branch, subst),
        },
        ExprKind::Paren(inner) => ExprKind::Paren(substitute_idents(inner, subst)),
        ExprKind::Membership { expr, set } => ExprKind::Membership {
            expr: substitute_idents(expr, subst),
            set: substitute_idents(set, subst),
        },
        ExprKind::SetLit(items) => {
            ExprKind::SetLit(items.iter().map(|x| substitute_idents(x, subst)).collect())
        }
        ExprKind::RangeLit { lo, hi } => ExprKind::RangeLit {
            lo: lo.as_ref().map(|x| substitute_idents(x, subst)),
            hi: hi.as_ref().map(|x| substitute_idents(x, subst)),
        },
        // Literals, ImplicitSelf, and the spec-§5 / §6 / §8 forms
        // (HashHash, SeqRepeat, DistLit, etc.) don't bind names and
        // aren't expected inside constraint bodies; pass through
        // verbatim. If a relation body happens to contain one of these
        // shapes, the downstream constraint translator will reject it
        // with a clear "not supported" error, same as it would for an
        // un-inlined body.
        other => other.clone(),
    };
    Expr::new(new_kind, span)
}

pub(crate) struct TbirRandomizeEmitter {
    out: String,
    errors: Vec<String>,
    txn_fields: std::collections::HashMap<String, Vec<TxnFieldInfo>>,
    runtime_randomize_problem_ids: std::collections::HashMap<SourceSite, u64>,
    source_identities: std::collections::HashMap<SourceId, StableSourceIdentity>,
    randomize_site_owners: std::collections::HashMap<SourceSite, String>,
    current_source_id: SourceId,
    randomize_target_sentinel: Option<&'static str>,
    randomize_context_sentinel: Option<&'static str>,
    randomize_state_sentinel: Option<&'static str>,
    randomize_context_ident: String,
    capsule_state_ident: String,
    relations: std::collections::HashMap<String, RelationDecl>,
    shadowed_lets: std::collections::HashSet<String>,
    let_widths: std::collections::HashMap<String, LetWidth>,
    enums: std::collections::HashMap<String, usize>,
    enum_variants: std::collections::HashMap<String, i64>,
    consts: std::collections::HashMap<String, crate::constraints::IntegerConstSchema>,
    const_widths: std::collections::HashMap<String, u32>,
    flavor: Flavor,
    per_run_cells: Vec<PerRunCell>,
    current_component_method: Option<(String, String, String, bool)>,
    transactions: std::collections::HashSet<String>,
    structs: std::collections::HashSet<String>,
}

/// The per-element random draw `emit_field_random` writes inside a
/// `list<T>` loop, or `None` when this backend has none for `T` and
/// writes a bare `t->f[_i] = 0;` instead.
///
/// A `None` here is not merely a weak draw. The member is
/// `std::vector<T'>` for whatever `T'` `txn_field_c_type` picked, and
/// `= 0` only compiles when `T'` is a scalar — for
/// `list<Vec<uint<8>, 2>>` it is `std::array<uint64_t, 2>` and g++
/// rejects the assignment, for EVERY program that declares the record.
/// `record_leaf_fate` asks this rather than restating the arm.
fn list_elem_random_expr(elem: &TypeExpr) -> Option<String> {
    let TypeExpr::Builtin { args, .. } = elem else {
        return None;
    };
    match list_elem_random_kind(elem)? {
        ScalarLeafKind::Unsigned => Some(emit_random_unsigned_expr(
            type_arg_width(args).unwrap_or(32),
        )),
        ScalarLeafKind::Signed => {
            let w = type_arg_width(args).unwrap_or(32);
            Some(emit_random_signed_expr(w))
        }
        ScalarLeafKind::Bool => Some("harc_rt::random::harc_rng_range(harc_rng_next, 0, 1)".into()),
    }
}

fn build_randomize_emitter(file: &SourceFile, _opts: &EmitOpts) -> TbirRandomizeEmitter {
    let file = &normalize_vec_record_elem_fields(file);
    // The tbir backend already desugars impl-for before lowering, so the
    // `file` handed here is classic-form; desugar again is idempotent and
    // keeps the problem-table spans aligned with v1.
    let file = desugar_impl_for_test_in_file(file);
    let file = &file;

    let typed_solver_problem_table =
        crate::solver::problem_table::build_typed_solver_problem_table(file);
    let mut runtime_randomize_problem_ids = std::collections::HashMap::new();
    for entry in &typed_solver_problem_table.entries {
        let crate::solver::problem_table::TypedSolverProblemSource::RandomizeSite {
            source_id,
            span,
            ..
        } = entry.source
        else {
            continue;
        };
        if let crate::solver::problem_table::TypedSolverProblemBuild::Z3 { typed, .. } =
            &entry.build
        {
            runtime_randomize_problem_ids
                .insert(SourceSite::new(source_id, span), typed.problem_id.0);
        }
    }

    // Record-field metadata: same collection the main emission path runs
    // (record_fields/record_bodies -> flatten_*_field_infos), restricted
    // to the inputs the constraint solver block consumes.
    let mut record_fields: std::collections::HashMap<String, Vec<Field>> =
        std::collections::HashMap::new();
    let mut enum_domains: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut enums: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut enum_variants: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    let const_elab = crate::constraints::elaborate_constraints(file);
    let consts: std::collections::HashMap<_, _> = const_elab.consts.into_iter().collect();
    let mut relations: std::collections::HashMap<String, RelationDecl> =
        std::collections::HashMap::new();
    let const_widths: std::collections::HashMap<String, u32> = consts
        .iter()
        .filter_map(|(name, constant)| {
            constant
                .width
                .or_else(|| literal_operand_bit_width(&constant.literal))
                .map(|width| (name.clone(), width))
        })
        .collect();
    for item in &file.items {
        match item {
            Item::Enum(decl) => {
                enums.insert(decl.name.name.clone(), decl.variants.len());
                enum_domains.insert(
                    decl.name.name.clone(),
                    decl.variants
                        .iter()
                        .map(|variant| variant.name.clone())
                        .collect(),
                );
                for (index, variant) in decl.variants.iter().enumerate() {
                    enum_variants
                        .entry(variant.name.clone())
                        .or_insert(index as i64);
                }
            }
            Item::Relation(relation) => {
                relations.insert(relation.name.name.clone(), relation.clone());
            }
            _ => {}
        }
    }
    for item in &file.items {
        match item {
            Item::Struct(record) => {
                record_fields.insert(record.name.name.clone(), record.fields.clone());
            }
            Item::Transaction(record) => {
                record_fields.insert(record.name.name.clone(), txn_direct_fields(&record.body));
            }
            _ => {}
        }
    }
    let mut txn_fields: std::collections::HashMap<String, Vec<TxnFieldInfo>> =
        std::collections::HashMap::new();
    for item in &file.items {
        match item {
            Item::Transaction(record) => {
                txn_fields.insert(
                    record.name.name.clone(),
                    flatten_txn_body_field_infos(
                        &record.body,
                        &record_fields,
                        &enums,
                        &enum_domains,
                    ),
                );
            }
            Item::Struct(record) => {
                txn_fields.insert(
                    record.name.name.clone(),
                    flatten_record_field_infos(
                        &record.fields,
                        &record_fields,
                        &enums,
                        &enum_domains,
                    ),
                );
            }
            _ => {}
        }
    }

    TbirRandomizeEmitter {
        out: String::new(),
        errors: Vec::new(),
        txn_fields,
        runtime_randomize_problem_ids,
        source_identities: stable_source_identities(file),
        randomize_site_owners: component_randomize_site_owners(file),
        current_source_id: SourceId::default(),
        randomize_target_sentinel: Some(TBIR_RANDOMIZE_TARGET_SENTINEL),
        randomize_context_sentinel: Some(TBIR_RANDOMIZE_CONTEXT_SENTINEL),
        randomize_state_sentinel: Some(TBIR_RANDOMIZE_STATE_SENTINEL),
        randomize_context_ident: "ctx".to_string(),
        capsule_state_ident: "_harc_randomize_state".to_string(),
        relations,
        shadowed_lets: std::collections::HashSet::new(),
        let_widths: std::collections::HashMap::new(),
        enums,
        enum_variants,
        consts,
        const_widths,
        flavor: Flavor::Capsule,
        per_run_cells: Vec::new(),
        current_component_method: None,
        transactions: std::collections::HashSet::new(),
        structs: std::collections::HashSet::new(),
    }
}

impl TbirRandomizeEmitter {
    pub(crate) fn new(file: &SourceFile, opts: &EmitOpts) -> Self {
        build_randomize_emitter(file, opts)
    }

    pub(crate) fn emit_site(
        &mut self,
        site: &crate::ir::ConstraintSite,
        depth: usize,
    ) -> Result<(String, Vec<PerRunCell>), EmitError> {
        self.out.clear();
        self.errors.clear();
        self.current_source_id = site.source_id;
        let cell_start = self.per_run_cells.len();
        self.emit_randomize_for_site(site, depth);
        if let Some(error) = self.errors.first() {
            return Err(EmitError(format!(
                "tbir randomize({}): {error}",
                site.record
            )));
        }
        let snippet = std::mem::take(&mut self.out).replace(
            "harc_rng_next()",
            &format!("{}.rng.next()", TBIR_RANDOMIZE_CONTEXT_SENTINEL),
        );
        Ok((snippet, self.per_run_cells[cell_start..].to_vec()))
    }

    pub(crate) fn all_cells(&self) -> &[PerRunCell] {
        &self.per_run_cells
    }

    pub(crate) fn begin_record_helpers(&mut self) {
        self.out.clear();
        self.errors.clear();
    }

    pub(crate) fn emit_record_helper(&mut self, item: &Item) -> Result<(), EmitError> {
        match item {
            Item::Struct(record) => {
                self.emit_record_randomize_fn(&record.name.name, &record.fields)
            }
            Item::Transaction(record) => {
                let fields = txn_all_fields(&record.body);
                self.emit_record_randomize_fn(&record.name.name, &fields);
            }
            _ => unreachable!("record helper item must be a struct or transaction"),
        }
        if let Some(error) = self.errors.first() {
            return Err(EmitError(format!("tbir record randomize helper: {error}")));
        }
        Ok(())
    }

    pub(crate) fn finish_record_helpers(self) -> String {
        self.out
    }

    fn pad(&mut self, depth: usize) {
        for _ in 0..depth {
            self.out.push_str(INDENT);
        }
    }

    fn randomize_context_ref(&self) -> &str {
        self.randomize_context_sentinel
            .unwrap_or(&self.randomize_context_ident)
    }

    fn randomize_state_ref(&self) -> String {
        self.randomize_state_sentinel
            .map(str::to_string)
            .unwrap_or_else(|| self.capsule_state_ident.clone())
    }

    fn per_run_cell(&mut self, tag: String, ctype: &str, init: Option<&str>) -> String {
        let init = init.map(str::to_string);
        if let Some(existing) = self.per_run_cells.iter().find(|cell| cell.tag == tag) {
            if existing.ctype != ctype || existing.init != init {
                self.errors.push(format!(
                    "randomize runtime cell `{tag}` has conflicting declarations: `{}` / {:?} versus `{ctype}` / {:?}",
                    existing.ctype, existing.init, init
                ));
            }
        } else {
            self.per_run_cells.push(PerRunCell {
                tag: tag.clone(),
                ctype: ctype.to_string(),
                init,
            });
        }
        format!("{}.{tag}", self.randomize_state_ref())
    }

    fn auto_cov_state_ref(&mut self, cache_tag: &str) -> String {
        self.per_run_cell(
            format!("_auto_cov_state_{cache_tag}"),
            "harc_rt::random::HarcAutoCovState",
            None,
        )
    }

    fn unique_history_ref(
        &mut self,
        scope: &str,
        record: &str,
        c_name: &str,
        value_ty: &str,
    ) -> String {
        let key = stable_identity_hash(
            "harc-unique-history-v1",
            &[scope.as_bytes(), record.as_bytes(), c_name.as_bytes()],
        );
        let registry = match scope {
            "test" => format!("{}.rng.unique", self.randomize_context_ref()),
            "tseq" => TBIR_RANDOMIZE_TSEQ_SCOPE_SENTINEL.to_string(),
            "sequencer" => TBIR_RANDOMIZE_SEQUENCER_SCOPE_SENTINEL.to_string(),
            other => {
                self.errors.push(format!(
                    "randomize({record}): unsupported unique scope `{other}`"
                ));
                format!("{}.rng.unique", self.randomize_context_ref())
            }
        };
        format!("{registry}.get<{value_ty}>({key}ULL)")
    }

    fn is_record_type(&self, name: &str) -> bool {
        self.transactions.contains(name) || self.structs.contains(name)
    }

    fn emit_expr(&mut self, expr: &Expr) {
        match &*expr.kind {
            ExprKind::Int(value) => self.out.push_str(&c_value_literal(value)),
            ExprKind::Float(value) => self.out.push_str(value),
            ExprKind::Time(value) => self.out.extend(
                value
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '_'),
            ),
            ExprKind::String(value) => {
                self.out.push('"');
                for ch in value.chars() {
                    match ch {
                        '\\' => self.out.push_str("\\\\"),
                        '"' => self.out.push_str("\\\""),
                        '\n' => self.out.push_str("\\n"),
                        '\r' => self.out.push_str("\\r"),
                        '\t' => self.out.push_str("\\t"),
                        other => self.out.push(other),
                    }
                }
                self.out.push('"');
            }
            ExprKind::Bool(value) => self.out.push_str(if *value { "true" } else { "false" }),
            ExprKind::Ident(ident) => {
                if self.consts.contains_key(&ident.name) {
                    self.out.push_str(&global_const_cpp_name(&ident.name));
                } else if self.enum_variants.contains_key(&ident.name) {
                    self.out
                        .push_str(&global_enum_variant_cpp_name(&ident.name));
                } else {
                    self.out.push_str(&ident.name);
                }
            }
            ExprKind::ImplicitSelf => {}
            ExprKind::Field { target, name } => {
                self.emit_expr(target);
                if !matches!(&*target.kind, ExprKind::ImplicitSelf) {
                    self.out.push('.');
                }
                self.out.push_str(&name.name);
            }
            ExprKind::Index { target, index } => {
                self.emit_expr(target);
                self.out.push('[');
                self.emit_expr(index);
                self.out.push(']');
            }
            ExprKind::BitSlice { target, hi, lo } => {
                self.out.push_str("harc_rt::harc_bits(");
                self.emit_expr(target);
                self.out.push_str(", ");
                self.emit_expr(hi);
                self.out.push_str(", ");
                self.emit_expr(lo);
                self.out.push(')');
            }
            ExprKind::Unary { op, expr } => {
                self.out.push_str(match op {
                    UnaryOp::Neg => "-",
                    UnaryOp::Not => "!",
                    UnaryOp::BitNot => "~",
                    _ => "",
                });
                self.emit_expr(expr);
            }
            ExprKind::Binary { op, lhs, rhs } => {
                self.out.push('(');
                self.emit_expr(lhs);
                write!(self.out, " {} ", c_binary_op(*op)).ok();
                self.emit_expr(rhs);
                self.out.push(')');
            }
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => {
                self.out.push('(');
                self.emit_expr(cond);
                self.out.push_str(" ? ");
                self.emit_expr(then_branch);
                self.out.push_str(" : ");
                self.emit_expr(else_branch);
                self.out.push(')');
            }
            ExprKind::Paren(inner) => {
                self.out.push('(');
                self.emit_expr(inner);
                self.out.push(')');
            }
            ExprKind::Cast { expr, .. } => self.emit_expr(expr),
            ExprKind::Call { callee, args } => {
                self.emit_expr(callee);
                self.out.push('(');
                for (index, arg) in args.iter().enumerate() {
                    if index != 0 {
                        self.out.push_str(", ");
                    }
                    match arg {
                        CallArg::Expr(value) => self.emit_expr(value),
                        CallArg::Named { value, .. } => self.emit_expr(value),
                    }
                }
                self.out.push(')');
            }
            _ => {
                self.errors.push(format!(
                    "randomize runtime expression is unsupported by TBIR rendering: {}",
                    expr_source_str(expr)
                ));
                self.out.push('0');
            }
        }
    }
}

impl TbirRandomizeEmitter {
    /// Emit one lowered TBIR `ConstraintSite` through the standalone renderer.
    fn emit_randomize_for_site(&mut self, site: &crate::ir::ConstraintSite, depth: usize) {
        let ty = site.record.clone();
        let target = &site.target;
        let combined = &site.constraints;
        self.report_runtime_dependent_randomize_field_attrs(&ty);
        let has_solver_policy_fields = self.txn_fields.get(&ty).is_some_and(|fields| {
            fields
                .iter()
                .any(|field| field_attr_unique(field) || !auto_coverage_values(field).is_empty())
        });
        if combined.is_empty() && !has_solver_policy_fields {
            if !self.emit_runtime_unconstrained_randomize_call(&ty, target, depth) {
                self.pad(depth);
                write!(self.out, "randomize_{ty}(&").ok();
                self.emit_randomize_target(target);
                writeln!(self.out, ", harc_rng);").ok();
            }
            self.emit_randomize_trace_event(&ty, target, depth);
        } else {
            self.emit_constraint_solver_block(&ty, target, combined, site.blocking, depth);
        }
    }

    pub(super) fn emit_randomize_target(&mut self, target: &Expr) {
        if let Some(sentinel) = self.randomize_target_sentinel {
            self.out.push_str(sentinel);
        } else {
            self.emit_expr(target);
        }
    }

    pub(super) fn runtime_randomize_problem_id(&self, target: &Expr) -> Option<u64> {
        self.runtime_randomize_problem_ids
            .get(&SourceSite::new(self.current_source_id, target.span))
            .copied()
    }

    pub(super) fn randomize_site_identity(
        &self,
        target: &Expr,
        runtime_problem_id: Option<u64>,
    ) -> (u64, String) {
        if let Some(problem_id) = runtime_problem_id {
            return (problem_id, format!("_solver_site_{problem_id}"));
        }
        let source = self
            .source_identities
            .get(&self.current_source_id)
            .copied()
            .unwrap_or_else(|| {
                let fallback = [b"<unknown>".as_slice(), b"".as_slice()];
                StableSourceIdentity {
                    tag: stable_identity_hash("harc-source-tag-v2", &fallback),
                    site_seed: stable_identity_hash("harc-source-site-v2", &fallback),
                }
            });
        let owner = self
            .randomize_site_owners
            .get(&SourceSite::new(self.current_source_id, target.span))
            .cloned()
            .or_else(|| {
                self.current_component_method
                    .as_ref()
                    .map(|(component, _, method, _)| format!("component {component}::{method}"))
            })
            .unwrap_or_else(|| "anonymous".to_string());
        let owner_tag = stable_identity_hash(
            "harc-component-randomize-owner-v1",
            &[&source.tag.to_le_bytes(), owner.as_bytes()],
        );
        (
            stable_component_randomize_site_id(source, &owner, target.span),
            format!(
                "_solver_site_h{:016x}_b{}_e{}",
                owner_tag, target.span.start, target.span.end
            ),
        )
    }

    pub(super) fn randomize_global_seed_ref(&self) -> String {
        format!("{}.rng.initial_seed", self.randomize_context_ref())
    }

    pub(super) fn component_call_site_ref(&mut self, site_id: u64, cache_tag: &str) -> String {
        let tag = format!("{cache_tag}_call_site");
        let init = format!("{{{site_id}ULL, {site_id}ULL, 0}}");
        self.per_run_cell(tag, "harc_rt::random::HarcRuntimeCallSite", Some(&init))
    }

    pub(super) fn runtime_problem_call_site_ref(
        &mut self,
        site_id: u64,
        cache_tag: &str,
    ) -> String {
        if let Some(state) = self.randomize_state_sentinel {
            format!("{state}.call_site")
        } else if self.flavor == Flavor::Legacy {
            self.randomize_context_ref().to_string()
        } else {
            self.component_call_site_ref(site_id, cache_tag)
        }
    }

    pub(super) fn emit_runtime_unconstrained_randomize_call(
        &mut self,
        ty: &str,
        target: &Expr,
        depth: usize,
    ) -> bool {
        let runtime_problem_id = self.runtime_randomize_problem_id(target);
        let (site_id, cache_tag) = self.randomize_site_identity(target, runtime_problem_id);
        let global_seed = self.randomize_global_seed_ref();
        if runtime_problem_id.is_none() {
            let call_site = self.component_call_site_ref(site_id, &cache_tag);
            self.pad(depth);
            writeln!(
                self.out,
                "{{   // site-stable runtime randomize scaffold for unconstrained record"
            )
            .ok();
            self.pad(depth + 1);
            writeln!(
                self.out,
                "auto _harc_rt_seed = harc_rt::random::harc_call_site_next_seed({call_site}, {global_seed});"
            )
            .ok();
            self.pad(depth + 1);
            writeln!(self.out, "harc_rt::random::HarcRng _harc_site_rng;").ok();
            self.pad(depth + 1);
            writeln!(self.out, "_harc_site_rng.state = _harc_rt_seed;").ok();
            self.pad(depth + 1);
            writeln!(self.out, "_harc_site_rng.initial_seed = _harc_rt_seed;").ok();
            self.pad(depth + 1);
            write!(self.out, "randomize_{ty}(&").ok();
            self.emit_randomize_target(target);
            writeln!(self.out, ", _harc_site_rng);").ok();
            self.pad(depth);
            writeln!(self.out, "}}").ok();
            return true;
        }
        let problem_id = site_id;
        let prepare_state = self.runtime_problem_call_site_ref(problem_id, &cache_tag);
        self.pad(depth);
        writeln!(
            self.out,
            "{{   // runtime queued randomize scaffold for unconstrained record"
        )
        .ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "auto _harc_rt_call = harc_prepare_randomize_call({prepare_state}, {problem_id}ULL, {global_seed}, 0);"
        )
        .ok();
        self.pad(depth + 1);
        writeln!(self.out, "auto _harc_rt_seed = _harc_rt_call.seed;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "harc_rt::random::HarcRng _harc_site_rng;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "_harc_site_rng.state = _harc_rt_seed;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "_harc_site_rng.initial_seed = _harc_rt_seed;").ok();
        self.pad(depth + 1);
        write!(
            self.out,
            "auto _harc_rt_status = harc_rt::random::harc_solve_queued("
        )
        .ok();
        self.emit_randomize_target(target);
        writeln!(
            self.out,
            ", _harc_rt_call.problem_id, _harc_rt_seed, [&](auto* _harc_target) {{ randomize_{ty}(_harc_target, _harc_site_rng); }});"
        )
        .ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "(void)harc_rt::random::harc_handle_solve_status(_harc_rt_status);"
        )
        .ok();
        self.pad(depth);
        writeln!(self.out, "}}").ok();
        true
    }

    pub(super) fn emit_randomize_trace_event(&mut self, ty: &str, target: &Expr, depth: usize) {
        let Some(fields) = self.txn_fields.get(ty).cloned() else {
            return;
        };
        self.pad(depth);
        writeln!(self.out, "if (trace.enabled) {{").ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "std::string _trace_fields = \"\\\"txn_type\\\":\\\"{}\\\",\\\"fields\\\":{{\";",
            escape_c(ty)
        )
        .ok();
        let mut emitted = 0usize;
        for f in &fields {
            if f.width == 0 && f.list.is_none() {
                continue;
            }
            self.pad(depth + 1);
            let prefix = if emitted == 0 {
                format!("\\\"{}\\\":", escape_c(&f.name))
            } else {
                format!(",\\\"{}\\\":", escape_c(&f.name))
            };
            emitted += 1;
            if f.list.is_some() {
                write!(
                    self.out,
                    "_trace_fields += \"{prefix}\" + std::to_string((unsigned long long)("
                )
                .ok();
                self.emit_randomize_target(target);
                writeln!(self.out, ".{}.size()));", f.name).ok();
            } else if f.width > 64 {
                write!(
                    self.out,
                    "_trace_fields += \"{prefix}\\\"0x\" + std::string((const char*)harc_rt::HarcHexBufWide("
                )
                .ok();
                self.emit_randomize_target(target);
                writeln!(
                    self.out,
                    ".{}, {}, false)) + \"\\\"\";",
                    f.name,
                    f.width.div_ceil(4)
                )
                .ok();
            } else {
                let cast = if f.signed {
                    "long long"
                } else {
                    "unsigned long long"
                };
                write!(
                    self.out,
                    "_trace_fields += \"{prefix}\" + std::to_string(({cast})("
                )
                .ok();
                self.emit_randomize_target(target);
                writeln!(self.out, ".{}));", f.name).ok();
            }
        }
        self.pad(depth + 1);
        writeln!(self.out, "_trace_fields += \"}}\";").ok();
        self.pad(depth + 1);
        writeln!(self.out, "trace.randomize(cycle_count, _trace_fields);").ok();
        self.pad(depth);
        writeln!(self.out, "}}").ok();
    }

    /// Emit one random-field assignment honouring `[range(...)]` and
    /// `[dist {...}]` attributes. Falls back to a uniform sample over the
    /// declared type's value range.
    fn emit_field_random(&mut self, f: &Field) {
        if let Some(info) = list_field_info(&f.ty) {
            let max_len = info.declared_max_len.unwrap_or(0);
            writeln!(
                self.out,
                "{INDENT}t->{}.resize((size_t)harc_rt::random::harc_rng_range(harc_rng_next, 0, {}));",
                f.name.name, max_len
            )
            .ok();
            writeln!(
                self.out,
                "{INDENT}for (size_t _i = 0; _i < t->{}.size(); ++_i) {{",
                f.name.name
            )
            .ok();
            let draw = list_elem_type(&f.ty)
                .and_then(list_elem_random_expr)
                .unwrap_or_else(|| "0".to_string());
            writeln!(self.out, "{INDENT}{INDENT}t->{}[_i] = {draw};", f.name.name).ok();
            writeln!(self.out, "{INDENT}}}").ok();
            return;
        }

        // Look for a `[range(lo, hi)]` or `[dist {...}]` attribute.
        let mut handled = false;
        for a in &f.attrs {
            match a.name.name.as_str() {
                "range" => {
                    if a.args.len() >= 2 {
                        if let (AttrArg::Expr(lo), AttrArg::Expr(hi)) = (&a.args[0], &a.args[1]) {
                            write!(
                                self.out,
                                "{INDENT}t->{} = harc_rt::random::harc_rng_range(harc_rng_next, ",
                                f.name.name
                            )
                            .ok();
                            self.emit_expr(lo);
                            write!(self.out, ", ").ok();
                            self.emit_expr(hi);
                            writeln!(self.out, ");").ok();
                            handled = true;
                        }
                    }
                }
                "dist" => {
                    let dist_args: Option<&Vec<DistEntry>> = a.args.iter().find_map(|x| match x {
                        AttrArg::Dist(d) => Some(d),
                        _ => None,
                    });
                    if let Some(entries) = dist_args {
                        write!(
                            self.out,
                            "{INDENT}t->{} = harc_rt::random::harc_rng_dist(harc_rng_next, {{",
                            f.name.name
                        )
                        .ok();
                        self.emit_rng_dist_entries(entries);
                        writeln!(self.out, "}});").ok();
                        handled = true;
                    }
                }
                _ => {}
            }
            if handled {
                break;
            }
        }
        if handled {
            return;
        }

        // Fallback: type-driven uniform sampling.
        match &f.ty {
            TypeExpr::Builtin { name, args, .. } => {
                let width = type_arg_width(args);
                match name {
                    BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits => {
                        let w = width.unwrap_or(32);
                        writeln!(
                            self.out,
                            "{INDENT}t->{} = {};",
                            f.name.name,
                            emit_random_unsigned_expr(w)
                        )
                        .ok();
                    }
                    BuiltinTy::SInt | BuiltinTy::SIntCap => {
                        let w = width.unwrap_or(32);
                        writeln!(
                            self.out,
                            "{INDENT}t->{} = {};",
                            f.name.name,
                            emit_random_signed_expr(w)
                        )
                        .ok();
                    }
                    BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => {
                        writeln!(
                            self.out,
                            "{INDENT}t->{} = harc_rt::random::harc_rng_range(harc_rng_next, 0, 1);",
                            f.name.name
                        )
                        .ok();
                    }
                    BuiltinTy::Int => {
                        writeln!(
                            self.out,
                            "{INDENT}t->{} = harc_rt::random::harc_rng_range(harc_rng_next, 0, 0x7FFFFFFF);",
                            f.name.name
                        )
                        .ok();
                    }
                    _ => {
                        writeln!(
                            self.out,
                            "{INDENT}// {} : <unsupported type for v0 randomize>",
                            f.name.name
                        )
                        .ok();
                    }
                }
            }
            TypeExpr::Named { name, .. } => {
                let last = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                if let Some(&n) = self.enums.get(last) {
                    let hi = if n == 0 { 0 } else { (n - 1) as i64 };
                    writeln!(
                        self.out,
                        "{INDENT}t->{} = harc_rt::random::harc_rng_range(harc_rng_next, 0, {});",
                        f.name.name, hi
                    )
                    .ok();
                } else if self.is_record_type(last) {
                    writeln!(
                        self.out,
                        "{INDENT}randomize_{last}(&t->{}, harc_rng);",
                        f.name.name
                    )
                    .ok();
                } else {
                    writeln!(
                        self.out,
                        "{INDENT}// {} : {} (named, not yet supported)",
                        f.name.name, last
                    )
                    .ok();
                }
            }
        }
    }

    pub(super) fn emit_rng_dist_entries(&mut self, entries: &[DistEntry]) {
        for (i, e) in entries.iter().enumerate() {
            if i > 0 {
                write!(self.out, ", ").ok();
            }
            // Each dist entry: (lo, hi, weight). The `value` expression is
            // either a RangeLit (use lo/hi) or a scalar (use as both lo/hi).
            match &*e.value.kind {
                ExprKind::RangeLit {
                    lo: Some(lo),
                    hi: Some(hi),
                } => {
                    write!(self.out, "{{(int64_t)(").ok();
                    self.emit_expr(lo);
                    write!(self.out, "), (int64_t)(").ok();
                    self.emit_expr(hi);
                    write!(self.out, "), (int64_t)(").ok();
                    self.emit_expr(&e.weight);
                    write!(self.out, ")}}").ok();
                }
                _ => {
                    write!(self.out, "{{(int64_t)(").ok();
                    self.emit_expr(&e.value);
                    write!(self.out, "), (int64_t)(").ok();
                    self.emit_expr(&e.value);
                    write!(self.out, "), (int64_t)(").ok();
                    self.emit_expr(&e.weight);
                    write!(self.out, ")}}").ok();
                }
            }
        }
    }

    pub(super) fn emit_record_randomize_fn(&mut self, name: &str, fields: &[Field]) {
        writeln!(
            self.out,
            "static void randomize_{name}({name}* t, harc_rt::random::HarcRng& harc_rng) {{"
        )
        .ok();
        writeln!(
            self.out,
            "{INDENT}auto harc_rng_next = [&]() {{ return harc_rng.next(); }};"
        )
        .ok();
        for field in fields {
            if field.non_random {
                if let Some(default) = &field.default {
                    write!(self.out, "{INDENT}t->{} = ", field.name.name).ok();
                    self.emit_expr(default);
                    writeln!(self.out, ";").ok();
                }
                continue;
            }
            self.emit_field_random(field);
        }
        writeln!(self.out, "}}").ok();
        writeln!(self.out).ok();
    }
}

impl TbirRandomizeEmitter {
    pub(super) fn report_runtime_dependent_randomize_field_attrs(&mut self, ty: &str) {
        let Some(fields) = self.txn_fields.get(ty).cloned() else {
            return;
        };
        for f in &fields {
            for a in &f.attrs {
                match a.name.name.as_str() {
                    "range" => {
                        for arg in &a.args {
                            if let AttrArg::Expr(e) = arg {
                                self.validate_field_attr_static_expr(ty, &f.name, "range", e);
                            }
                        }
                    }
                    "dist" => {
                        for arg in &a.args {
                            if let AttrArg::Dist(entries) = arg {
                                for entry in entries {
                                    self.validate_field_attr_static_expr(
                                        ty,
                                        &f.name,
                                        "dist",
                                        &entry.value,
                                    );
                                    self.validate_field_attr_static_expr(
                                        ty,
                                        &f.name,
                                        "dist",
                                        &entry.weight,
                                    );
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    pub(super) fn emit_target_field_access(&mut self, target: &Expr, f: &TxnFieldInfo) {
        self.emit_randomize_target(target);
        for part in &f.path {
            write!(self.out, ".{part}").ok();
        }
    }

    fn validate_field_attr_static_expr(&mut self, ty: &str, field: &str, attr: &str, expr: &Expr) {
        match &*expr.kind {
            ExprKind::Field { target, name } => {
                if let ExprKind::Ident(root) = &*target.kind {
                    self.errors.push(format!(
                        "randomize({ty}): field attribute `[{attr}]` on `{ty}.{field}` references runtime state `{}.{}`; move runtime-dependent constraints into `blocking randomize(...) with`",
                        root.name, name.name,
                    ));
                }
                self.validate_field_attr_static_expr(ty, field, attr, target);
            }
            ExprKind::Ident(id) if self.let_widths.contains_key(&id.name) => {
                self.errors.push(format!(
                    "randomize({ty}): field attribute `[{attr}]` on `{ty}.{field}` references runtime state `{}`; move runtime-dependent constraints into `blocking randomize(...) with`",
                    id.name,
                ));
            }
            ExprKind::Index { target, index } => {
                self.validate_field_attr_static_expr(ty, field, attr, target);
                self.validate_field_attr_static_expr(ty, field, attr, index);
            }
            ExprKind::BitSlice { target, hi, lo } => {
                for e in [target, hi, lo] {
                    self.validate_field_attr_static_expr(ty, field, attr, e);
                }
            }
            ExprKind::Call { callee, args } => {
                self.validate_field_attr_static_expr(ty, field, attr, callee);
                for arg in args {
                    let value = match arg {
                        CallArg::Expr(e) => e,
                        CallArg::Named { value, .. } => value,
                    };
                    self.validate_field_attr_static_expr(ty, field, attr, value);
                }
            }
            ExprKind::ForEachConstraint { iter, body, .. } => {
                self.validate_field_attr_static_expr(ty, field, attr, iter);
                for clause in body {
                    self.validate_field_attr_static_expr(ty, field, attr, clause);
                }
            }
            ExprKind::Cast { expr, .. }
            | ExprKind::Unary { expr, .. }
            | ExprKind::HashHash { expr, .. }
            | ExprKind::SeqRepeat { expr, .. }
            | ExprKind::Paren(expr) => {
                self.validate_field_attr_static_expr(ty, field, attr, expr);
            }
            ExprKind::Binary { lhs, rhs, .. }
            | ExprKind::Membership {
                expr: lhs,
                set: rhs,
            }
            | ExprKind::CoverArrow { lhs, rhs, .. } => {
                self.validate_field_attr_static_expr(ty, field, attr, lhs);
                self.validate_field_attr_static_expr(ty, field, attr, rhs);
            }
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => {
                for e in [cond, then_branch, else_branch] {
                    self.validate_field_attr_static_expr(ty, field, attr, e);
                }
            }
            ExprKind::RangeLit { lo, hi } => {
                for e in lo.iter().chain(hi.iter()) {
                    self.validate_field_attr_static_expr(ty, field, attr, e);
                }
            }
            ExprKind::SetLit(items) => {
                for e in items {
                    self.validate_field_attr_static_expr(ty, field, attr, e);
                }
            }
            ExprKind::DistLit(entries) | ExprKind::DistDirective { entries, .. } => {
                for entry in entries {
                    self.validate_field_attr_static_expr(ty, field, attr, &entry.value);
                    self.validate_field_attr_static_expr(ty, field, attr, &entry.weight);
                }
            }
            ExprKind::SystemCall { args, .. } | ExprKind::SolveOrder { args } => {
                for e in args {
                    self.validate_field_attr_static_expr(ty, field, attr, e);
                }
            }
            ExprKind::NamedArg { value, .. } => {
                self.validate_field_attr_static_expr(ty, field, attr, value);
            }
            ExprKind::StructLit { fields, .. } => {
                for f in fields {
                    self.validate_field_attr_static_expr(ty, field, attr, &f.value);
                }
            }
            _ => {}
        }
    }

    pub(super) fn validate_randomize_constraint_dependencies(
        &mut self,
        ty: &str,
        expr: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        target_root: Option<&str>,
        blocking: bool,
    ) {
        match &*expr.kind {
            ExprKind::Field { target, name } => {
                if expr_field_path(expr, target_root)
                    .is_some_and(|field| field_info.contains_key(&field))
                {
                    return;
                }
                if let ExprKind::Ident(root) = &*target.kind {
                    if !blocking && target_root.is_some_and(|expected| root.name != expected) {
                        let path = format!("{}.{}", root.name, name.name);
                        self.errors.push(format!(
                            "queued randomize({ty}) constraint references runtime state `{path}`; use `blocking randomize` once runtime-dependent constraint lowering is supported"
                        ));
                    }
                }
                self.validate_randomize_constraint_dependencies(
                    ty,
                    target,
                    field_info,
                    target_root,
                    blocking,
                );
            }
            ExprKind::Ident(id) => {
                if !blocking
                    && !field_info.contains_key(&id.name)
                    && !self.enum_variants.contains_key(&id.name)
                    && self.let_widths.contains_key(&id.name)
                {
                    self.errors.push(format!(
                        "queued randomize({ty}) constraint references runtime state `{}`; use `blocking randomize` once runtime-dependent constraint lowering is supported",
                        id.name
                    ));
                }
            }
            ExprKind::Index { target, index } => {
                self.validate_randomize_constraint_dependencies(
                    ty,
                    target,
                    field_info,
                    target_root,
                    blocking,
                );
                self.validate_randomize_constraint_dependencies(
                    ty,
                    index,
                    field_info,
                    target_root,
                    blocking,
                );
            }
            ExprKind::BitSlice { target, hi, lo } => {
                for e in [target, hi, lo] {
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        e,
                        field_info,
                        target_root,
                        blocking,
                    );
                }
            }
            ExprKind::Call { callee, args } => {
                if list_len_call_field_name(expr, field_info, target_root).is_some() {
                    return;
                }
                self.validate_randomize_constraint_dependencies(
                    ty,
                    callee,
                    field_info,
                    target_root,
                    blocking,
                );
                for arg in args {
                    let value = match arg {
                        CallArg::Expr(e) => e,
                        CallArg::Named { value, .. } => value,
                    };
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        value,
                        field_info,
                        target_root,
                        blocking,
                    );
                }
            }
            ExprKind::ForEachConstraint { iter, body, .. } => {
                self.validate_randomize_constraint_dependencies(
                    ty,
                    iter,
                    field_info,
                    target_root,
                    blocking,
                );
                for clause in body {
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        clause,
                        field_info,
                        target_root,
                        blocking,
                    );
                }
            }
            ExprKind::Cast { expr, .. }
            | ExprKind::Unary { expr, .. }
            | ExprKind::HashHash { expr, .. }
            | ExprKind::SeqRepeat { expr, .. }
            | ExprKind::Paren(expr) => {
                self.validate_randomize_constraint_dependencies(
                    ty,
                    expr,
                    field_info,
                    target_root,
                    blocking,
                );
            }
            ExprKind::Binary { lhs, rhs, .. }
            | ExprKind::Membership {
                expr: lhs,
                set: rhs,
            }
            | ExprKind::CoverArrow { lhs, rhs, .. } => {
                self.validate_randomize_constraint_dependencies(
                    ty,
                    lhs,
                    field_info,
                    target_root,
                    blocking,
                );
                self.validate_randomize_constraint_dependencies(
                    ty,
                    rhs,
                    field_info,
                    target_root,
                    blocking,
                );
            }
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => {
                for e in [cond, then_branch, else_branch] {
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        e,
                        field_info,
                        target_root,
                        blocking,
                    );
                }
            }
            ExprKind::RangeLit { lo, hi } => {
                for e in lo.iter().chain(hi.iter()) {
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        e,
                        field_info,
                        target_root,
                        blocking,
                    );
                }
            }
            ExprKind::SetLit(items) => {
                for e in items {
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        e,
                        field_info,
                        target_root,
                        blocking,
                    );
                }
            }
            ExprKind::DistLit(entries) | ExprKind::DistDirective { entries, .. } => {
                for entry in entries {
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        &entry.value,
                        field_info,
                        target_root,
                        blocking,
                    );
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        &entry.weight,
                        field_info,
                        target_root,
                        blocking,
                    );
                }
            }
            ExprKind::SoftConstraint(sc) => {
                self.validate_randomize_constraint_dependencies(
                    ty,
                    &sc.expr,
                    field_info,
                    target_root,
                    blocking,
                );
                if let Some(weight) = &sc.weight {
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        weight,
                        field_info,
                        target_root,
                        blocking,
                    );
                }
            }
            ExprKind::SystemCall { args, .. } | ExprKind::SolveOrder { args } => {
                for e in args {
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        e,
                        field_info,
                        target_root,
                        blocking,
                    );
                }
            }
            ExprKind::NamedArg { value, .. } => {
                self.validate_randomize_constraint_dependencies(
                    ty,
                    value,
                    field_info,
                    target_root,
                    blocking,
                );
            }
            ExprKind::StructLit { fields, .. } => {
                for field in fields {
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        &field.value,
                        field_info,
                        target_root,
                        blocking,
                    );
                }
            }
            _ => {}
        }
    }
}
impl TbirRandomizeEmitter {
    fn emit_solver_range_constraint(
        &mut self,
        f: &TxnFieldInfo,
        lo: &Expr,
        hi: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        target_root: Option<&str>,
        blocking: bool,
        solver_width: u32,
        depth: usize,
    ) {
        self.pad(depth);
        write!(self.out, "_s.add(").ok();
        self.emit_solver_range_constraint_expr(
            f,
            lo,
            hi,
            field_info,
            target_root,
            blocking,
            solver_width,
            &std::collections::HashMap::new(),
        );
        writeln!(self.out, ");").ok();
    }

    fn emit_solver_range_constraint_expr(
        &mut self,
        f: &TxnFieldInfo,
        lo: &Expr,
        hi: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        target_root: Option<&str>,
        blocking: bool,
        solver_width: u32,
        list_unroll_bounds: &std::collections::HashMap<String, usize>,
    ) {
        let c_name = c_ident(&f.name);
        if f.signed {
            write!(self.out, "_z_{} >= ", c_name).ok();
            self.emit_constraint_expr_w(
                lo,
                field_info,
                solver_width,
                target_root,
                blocking,
                list_unroll_bounds,
            );
            write!(self.out, " && _z_{} <= ", c_name).ok();
            self.emit_constraint_expr_w(
                hi,
                field_info,
                solver_width,
                target_root,
                blocking,
                list_unroll_bounds,
            );
        } else {
            write!(self.out, "z3::uge(_z_{}, ", c_name).ok();
            self.emit_constraint_expr_w(
                lo,
                field_info,
                solver_width,
                target_root,
                blocking,
                list_unroll_bounds,
            );
            write!(self.out, ") && z3::ule(_z_{}, ", c_name).ok();
            self.emit_constraint_expr_w(
                hi,
                field_info,
                solver_width,
                target_root,
                blocking,
                list_unroll_bounds,
            );
            write!(self.out, ")").ok();
        }
    }

    /// Inline a list of top-level constraint expressions, expanding
    /// any `Call(Ident(R), args)` whose name resolves to a known
    /// `relation R(params) … end relation` (spec §4.2). Each call:
    ///   * Looks up R in `self.relations`.
    ///   * Builds a substitution map `formal_param → actual_arg`.
    ///   * For `RelationBody::Block(exprs)`, returns each expr with
    ///     substitution applied — each becomes its own constraint at
    ///     top level (`_s.add(...)` per body expr).
    ///   * For `RelationBody::Alias(expr)`, returns a 1-element Vec
    ///     of the substituted expr.
    ///
    /// Non-call expressions are also walked with `expand_relation_subtree`
    /// so a nested relation call (e.g. inside a `Binary &&`) expands too.
    /// Recursion handles relations of relations.
    fn expand_relation_calls(&self, exprs: &[Expr]) -> Vec<Expr> {
        // One budget per top-level constraint list, so a big but honest
        // program cannot exhaust it by accumulation. See
        // `RELATION_EXPANSION_BUDGET`.
        let budget = std::cell::Cell::new(RELATION_EXPANSION_BUDGET);
        let mut active: Vec<&str> = Vec::new();
        self.expand_relation_calls_budgeted(&budget, &mut active, 0, exprs)
    }

    fn expand_relation_calls_budgeted<'r>(
        &'r self,
        budget: &std::cell::Cell<u32>,
        active: &mut Vec<&'r str>,
        depth: u32,
        exprs: &[Expr],
    ) -> Vec<Expr> {
        let mut out: Vec<Expr> = Vec::with_capacity(exprs.len());
        for e in exprs {
            // Top-level Call: expand to potentially multiple constraints.
            if let Some((name, expanded)) = self.try_expand_top_level_call(budget, active, depth, e)
            {
                // Each body expr is itself walked so block-form
                // relations that contain nested calls flatten too.
                active.push(name);
                for be in &expanded {
                    out.extend(self.expand_relation_calls_budgeted(
                        budget,
                        active,
                        depth + 1,
                        &[be.clone()],
                    ));
                }
                active.pop();
                continue;
            }
            // Non-call (or unknown-name call): walk the subtree so any
            // nested R(args) inside Binary / Unary / Paren / etc.
            // expands inline.
            out.push(self.expand_relation_subtree(budget, active, depth, e));
        }
        out
    }

    /// Try to expand `e` as a top-level relation call. Returns the
    /// substituted body expressions if `e` is `Call(Ident(R), args)`
    /// for a known relation; `None` otherwise (caller falls back to
    /// subtree walking). Arity-mismatched calls return `None` so the
    /// downstream translator surfaces a "constraint expression not
    /// supported" error with a useful span — better than swallowing.
    fn try_expand_top_level_call<'r>(
        &'r self,
        budget: &std::cell::Cell<u32>,
        active: &[&str],
        depth: u32,
        e: &Expr,
    ) -> Option<(&'r str, Vec<Expr>)> {
        let ExprKind::Call { callee, args } = &*e.kind else {
            return None;
        };
        let ExprKind::Ident(id) = &*callee.kind else {
            return None;
        };
        let (name, rel) = self.relations.get_key_value(&id.name)?;
        if rel.params.len() != args.len() {
            return None;
        }
        // A relation already being expanded is expanding into itself.
        // THIS is what stops the runaway, and it stops it at the root:
        // no tree is built, so it does not matter how fast the body
        // would have grown. Everything below is a backstop.
        //
        // Guards that only bounded the RESULT were tried first and each
        // was defeated by a shape it did not measure. A per-expansion
        // budget left `R(r) = R(r + r)` doubling the argument until the
        // process was OOM-killed. Charging per produced node fixed that
        // and left `R(r) = R((((…r…))))` — 60 nested parens, 418 bytes
        // of source — multiplying the tree's DEPTH by 60 per level
        // until the structural walk overflowed the stack, which is the
        // very SIGABRT the guard was written to prevent. There is no
        // end to that list, because bounding the output of an
        // unbounded loop is the wrong place to stand.
        //
        // `constraints::typed_lower` already guarded the same recursion
        // this way. Reaching for it here took three tries.
        if active.contains(&name.as_str()) {
            return None;
        }
        // Backstop, for growth that is not cyclic: a chain of DISTINCT
        // relations, each calling the previous one twice, is finite and
        // exponential. Out of budget or too deep behaves exactly like
        // an unknown relation — the call is left unexpanded and the
        // downstream translator reports "constraint function call not
        // supported in v0 solver path".
        if depth >= RELATION_EXPANSION_MAX_DEPTH {
            return None;
        }
        let mut subst: std::collections::HashMap<String, Expr> = std::collections::HashMap::new();
        for (p, a) in rel.params.iter().zip(args.iter()) {
            let arg_expr = match a {
                CallArg::Expr(ex) => ex.clone(),
                CallArg::Named { value, .. } => value.clone(),
            };
            subst.insert(p.name.name.clone(), arg_expr);
        }
        let body_exprs: Vec<Expr> = match &rel.body {
            RelationBody::Block(exprs) => {
                exprs.iter().map(|x| substitute_idents(x, &subst)).collect()
            }
            RelationBody::Alias(expr) => vec![substitute_idents(expr, &subst)],
        };
        // Spend the budget on NODES PRODUCED, not on the number of
        // expansions: a per-expansion budget cannot see growth in the
        // ARGUMENT, and `relation R(r: Req) = R(r + r)` doubles it
        // every level.
        //
        // This charges AFTER `substitute_idents` has already built the
        // tree, so a single step can overshoot — by one step's worth,
        // which is the body's occurrence count times the argument's
        // size. That is acceptable HERE only because the name stack
        // above has already refused everything cyclic; the shapes that
        // made the overshoot matter cannot reach this line. Charging
        // during substitution would be tighter and is not done.
        let produced: u32 = body_exprs
            .iter()
            .fold(0u32, |acc, e| acc.saturating_add(expr_node_count(e)));
        match budget.get().checked_sub(produced) {
            Some(left) => budget.set(left),
            None => return None,
        }
        Some((name, body_exprs))
    }

    /// Walk a constraint expression, replacing any nested
    /// `Call(Ident(R), args)` (for a known relation R) with R's body.
    /// Block-form bodies collapse to a `&&`-chain so they fit where a
    /// single expression is expected (e.g. inside a `Binary &&`); use
    /// `expand_relation_calls` at the top level to get one constraint
    /// per body expression.
    fn expand_relation_subtree<'r>(
        &'r self,
        budget: &std::cell::Cell<u32>,
        active: &mut Vec<&'r str>,
        depth: u32,
        expr: &Expr,
    ) -> Expr {
        let span = expr.span;
        // Recognize a relation Call anywhere in the tree. For block-form
        // bodies, build the AND-of-all-body-exprs expression so the
        // call site (which expected one Expr) gets one Expr back.
        if let Some((name, body_exprs)) =
            self.try_expand_top_level_call(budget, active, depth, expr)
        {
            active.push(name);
            let exprs: Vec<Expr> = body_exprs
                .iter()
                .map(|x| self.expand_relation_subtree(budget, active, depth + 1, x))
                .collect();
            active.pop();
            return and_join(&exprs, span);
        }
        let new_kind: ExprKind = match &*expr.kind {
            ExprKind::Field { target, name } => ExprKind::Field {
                target: self.expand_relation_subtree(budget, active, depth, target),
                name: name.clone(),
            },
            ExprKind::Index { target, index } => ExprKind::Index {
                target: self.expand_relation_subtree(budget, active, depth, target),
                index: self.expand_relation_subtree(budget, active, depth, index),
            },
            ExprKind::BitSlice { target, hi, lo } => ExprKind::BitSlice {
                target: self.expand_relation_subtree(budget, active, depth, target),
                hi: self.expand_relation_subtree(budget, active, depth, hi),
                lo: self.expand_relation_subtree(budget, active, depth, lo),
            },
            ExprKind::Call { callee, args } => ExprKind::Call {
                callee: self.expand_relation_subtree(budget, active, depth, callee),
                args: args
                    .iter()
                    .map(|a| match a {
                        CallArg::Expr(e) => {
                            CallArg::Expr(self.expand_relation_subtree(budget, active, depth, e))
                        }
                        CallArg::Named { name, value } => CallArg::Named {
                            name: name.clone(),
                            value: self.expand_relation_subtree(budget, active, depth, value),
                        },
                    })
                    .collect(),
            },
            ExprKind::ForEachConstraint { var, iter, body } => ExprKind::ForEachConstraint {
                var: var.clone(),
                iter: self.expand_relation_subtree(budget, active, depth, iter),
                body: self.expand_relation_calls_budgeted(budget, active, depth, body),
            },
            ExprKind::SoftConstraint(sc) => ExprKind::SoftConstraint(SoftConstraint {
                expr: self.expand_relation_subtree(budget, active, depth, &sc.expr),
                weight: sc
                    .weight
                    .as_ref()
                    .map(|weight| self.expand_relation_subtree(budget, active, depth, weight)),
            }),
            ExprKind::Cast { expr, ty } => ExprKind::Cast {
                expr: self.expand_relation_subtree(budget, active, depth, expr),
                ty: ty.clone(),
            },
            ExprKind::Unary { op, expr } => ExprKind::Unary {
                op: *op,
                expr: self.expand_relation_subtree(budget, active, depth, expr),
            },
            ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
                op: *op,
                lhs: self.expand_relation_subtree(budget, active, depth, lhs),
                rhs: self.expand_relation_subtree(budget, active, depth, rhs),
            },
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => ExprKind::Ternary {
                cond: self.expand_relation_subtree(budget, active, depth, cond),
                then_branch: self.expand_relation_subtree(budget, active, depth, then_branch),
                else_branch: self.expand_relation_subtree(budget, active, depth, else_branch),
            },
            ExprKind::Paren(inner) => {
                ExprKind::Paren(self.expand_relation_subtree(budget, active, depth, inner))
            }
            ExprKind::Membership { expr, set } => ExprKind::Membership {
                expr: self.expand_relation_subtree(budget, active, depth, expr),
                set: self.expand_relation_subtree(budget, active, depth, set),
            },
            ExprKind::SetLit(items) => ExprKind::SetLit(
                items
                    .iter()
                    .map(|x| self.expand_relation_subtree(budget, active, depth, x))
                    .collect(),
            ),
            ExprKind::RangeLit { lo, hi } => ExprKind::RangeLit {
                lo: lo
                    .as_ref()
                    .map(|x| self.expand_relation_subtree(budget, active, depth, x)),
                hi: hi
                    .as_ref()
                    .map(|x| self.expand_relation_subtree(budget, active, depth, x)),
            },
            other => other.clone(),
        };
        Expr::new(new_kind, span)
    }

    fn enum_variants_as_ints(&self, expr: &Expr) -> Expr {
        let span = expr.span;
        if let ExprKind::Ident(id) = &*expr.kind {
            if let Some(constant) = self.consts.get(&id.name) {
                if !constant.is_bool {
                    return Expr::new(ExprKind::Int(constant.literal.clone()), span);
                }
            }
            if let Some(idx) = self.enum_variants.get(&id.name).copied() {
                return Expr::new(ExprKind::Int(idx.to_string()), span);
            }
        }
        let kind = match &*expr.kind {
            ExprKind::Field { target, name } => ExprKind::Field {
                target: self.enum_variants_as_ints(target),
                name: name.clone(),
            },
            ExprKind::Index { target, index } => ExprKind::Index {
                target: self.enum_variants_as_ints(target),
                index: self.enum_variants_as_ints(index),
            },
            ExprKind::BitSlice { target, hi, lo } => ExprKind::BitSlice {
                target: self.enum_variants_as_ints(target),
                hi: self.enum_variants_as_ints(hi),
                lo: self.enum_variants_as_ints(lo),
            },
            ExprKind::Call { callee, args } => ExprKind::Call {
                callee: self.enum_variants_as_ints(callee),
                args: args
                    .iter()
                    .map(|arg| match arg {
                        CallArg::Expr(e) => CallArg::Expr(self.enum_variants_as_ints(e)),
                        CallArg::Named { name, value } => CallArg::Named {
                            name: name.clone(),
                            value: self.enum_variants_as_ints(value),
                        },
                    })
                    .collect(),
            },
            ExprKind::Cast { expr, ty } => ExprKind::Cast {
                expr: self.enum_variants_as_ints(expr),
                ty: ty.clone(),
            },
            ExprKind::Unary { op, expr } => ExprKind::Unary {
                op: *op,
                expr: self.enum_variants_as_ints(expr),
            },
            ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
                op: *op,
                lhs: self.enum_variants_as_ints(lhs),
                rhs: self.enum_variants_as_ints(rhs),
            },
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => ExprKind::Ternary {
                cond: self.enum_variants_as_ints(cond),
                then_branch: self.enum_variants_as_ints(then_branch),
                else_branch: self.enum_variants_as_ints(else_branch),
            },
            ExprKind::Paren(inner) => ExprKind::Paren(self.enum_variants_as_ints(inner)),
            ExprKind::Membership { expr, set } => ExprKind::Membership {
                expr: self.enum_variants_as_ints(expr),
                set: self.enum_variants_as_ints(set),
            },
            ExprKind::SoftConstraint(sc) => ExprKind::SoftConstraint(SoftConstraint {
                expr: self.enum_variants_as_ints(&sc.expr),
                weight: sc
                    .weight
                    .as_ref()
                    .map(|weight| self.enum_variants_as_ints(weight)),
            }),
            ExprKind::SetLit(items) => ExprKind::SetLit(
                items
                    .iter()
                    .map(|item| self.enum_variants_as_ints(item))
                    .collect(),
            ),
            ExprKind::RangeLit { lo, hi } => ExprKind::RangeLit {
                lo: lo.as_ref().map(|e| self.enum_variants_as_ints(e)),
                hi: hi.as_ref().map(|e| self.enum_variants_as_ints(e)),
            },
            other => other.clone(),
        };
        Expr::new(kind, span)
    }

    /// Emit an inline Z3 solver block for `randomize(t) with { ... }`.
    /// Each call builds a fresh Z3 context, declares one bitvector variable
    /// per field at its declared width, translates the constraint
    /// expressions, sets `random_seed` from the runtime call-site seed so
    /// `--seed` flows through, then assigns the satisfying model back into `t`. UNSAT
    /// raises a FAIL log line and increments errors.
    pub(super) fn emit_constraint_solver_block(
        &mut self,
        ty: &str,
        target: &Expr,
        with_body: &[Expr],
        blocking: bool,
        depth: usize,
    ) {
        let runtime_context = self.randomize_context_ref().to_string();
        // Expand relation calls into their bodies up-front so the
        // rest of the function sees a flat list of constraints. Both
        // the user's `with` body AND the transaction's keeps reach
        // this function via the merge in StmtKind::Randomize.
        let with_body_owned: Vec<Expr> = self.expand_relation_calls(with_body);
        let with_body: &[Expr] = &with_body_owned;
        let fields = match self.txn_fields.get(ty).cloned() {
            Some(f) => f,
            None => {
                self.errors
                    .push(format!("internal: no field info for transaction `{ty}`"));
                return;
            }
        };
        // Tracking for the constraint translator: field names plus their
        // signedness/domain metadata. Z3 vars remain 64-bit in this migration
        // step, but operator selection and domain constraints now honor the
        // source field type.
        let field_info: std::collections::HashMap<String, TxnFieldInfo> =
            fields.iter().map(|f| (f.name.clone(), f.clone())).collect();
        let target_root = randomize_target_ident(target);
        let mut dist_directives: std::collections::HashMap<String, Vec<DistEntry>> =
            std::collections::HashMap::new();
        let mut solve_order_directives: Vec<Vec<String>> = Vec::new();
        let mut hard_constraints: Vec<&Expr> = Vec::new();
        let mut soft_constraints: Vec<(&Expr, u32)> = Vec::new();
        for c in with_body {
            match &*c.kind {
                ExprKind::SoftConstraint(sc) => {
                    if matches!(
                        &*sc.expr.kind,
                        ExprKind::DistDirective { .. } | ExprKind::SolveOrder { .. }
                    ) {
                        self.errors.push(format!(
                            "randomize({ty}) with `soft`: expected a boolean constraint expression"
                        ));
                        continue;
                    }
                    let weight = match sc
                        .weight
                        .as_ref()
                        .map_or(Some(1), |weight| fold_int_literal(weight))
                        .and_then(|weight| u32::try_from(weight).ok())
                    {
                        Some(0) | None => {
                            self.errors.push(format!(
                                "randomize({ty}) with `soft`: weight must be a positive integer literal"
                            ));
                            1
                        }
                        Some(weight) => weight,
                    };
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        &sc.expr,
                        &field_info,
                        target_root,
                        blocking,
                    );
                    soft_constraints.push((&sc.expr, weight));
                    continue;
                }
                ExprKind::DistDirective { target, entries } => {
                    if let Some(field) =
                        randomize_target_field_name(target, &field_info, target_root)
                    {
                        if let Some(info) = field_info.get(&field) {
                            if info.non_random || info.list.is_some() || info.width == 0 {
                                self.errors.push(format!(
                                    "randomize({ty}) with `dist`: target `{field}` must be a random scalar field of transaction `{ty}`"
                                ));
                                continue;
                            }
                        }
                        for entry in entries {
                            self.validate_randomize_constraint_dependencies(
                                ty,
                                &entry.value,
                                &field_info,
                                target_root,
                                blocking,
                            );
                            self.validate_randomize_constraint_dependencies(
                                ty,
                                &entry.weight,
                                &field_info,
                                target_root,
                                blocking,
                            );
                        }
                        dist_directives.insert(field, entries.clone());
                    } else {
                        self.errors.push(format!(
                            "randomize({ty}) with `dist`: target must be a field of transaction `{ty}`"
                        ));
                    }
                    continue;
                }
                ExprKind::SolveOrder { args } => {
                    if args.len() < 2 {
                        self.errors.push(format!(
                            "randomize({ty}) with `solve_order` expects at least two target fields",
                        ));
                        continue;
                    }
                    let mut fields = Vec::new();
                    let mut valid = true;
                    for arg in args {
                        if let Some(field) =
                            randomize_target_field_name(arg, &field_info, target_root)
                        {
                            if let Some(info) = field_info.get(&field) {
                                if info.non_random {
                                    self.errors.push(format!(
                                        "randomize({ty}) with `solve_order`: field `{field}` is non-random and cannot be ordered"
                                    ));
                                    valid = false;
                                    continue;
                                }
                                if info.list.is_some() || info.width == 0 {
                                    self.errors.push(format!(
                                        "randomize({ty}) with `solve_order`: field `{field}` must be a scalar random field"
                                    ));
                                    valid = false;
                                    continue;
                                }
                            }
                            fields.push(field);
                        } else {
                            self.errors.push(format!(
                                "randomize({ty}) with `solve_order`: arguments must be fields of transaction `{ty}`",
                            ));
                            valid = false;
                        }
                    }
                    if valid {
                        solve_order_directives.push(fields);
                    }
                    continue;
                }
                _ => {}
            }
            hard_constraints.push(c);
        }
        for c in &hard_constraints {
            self.validate_randomize_constraint_dependencies(
                ty,
                c,
                &field_info,
                target_root,
                blocking,
            );
        }
        let list_unroll_bounds =
            infer_list_unroll_bounds(&fields, &hard_constraints, &field_info, target_root);
        for f in &fields {
            if f.list.is_some() && !list_unroll_bounds.contains_key(&f.name) {
                self.errors.push(format!(
                    "randomize({ty}): list field `{}` needs a bounded length constraint such as `{}.len() <= N`",
                    f.name, f.name
                ));
            }
        }
        let solver_width = fields
            .iter()
            .map(txn_field_solver_width)
            .max()
            .unwrap_or(64)
            .max(64);
        if solver_width > 1024 {
            self.errors.push(format!(
                "randomize({ty}): constraint solver supports vector fields up to 1024 bits, got {} bits",
                solver_width
            ));
        }

        self.pad(depth);
        writeln!(
            self.out,
            "{{   // {} randomize(t) with — runtime constrained solve callback",
            if blocking { "blocking" } else { "queued" }
        )
        .ok();

        let runtime_problem_id = self.runtime_randomize_problem_id(target);
        let (site_number, cache_tag) = self.randomize_site_identity(target, runtime_problem_id);
        let global_seed = self.randomize_global_seed_ref();
        if let Some(problem_id) = runtime_problem_id {
            let prepare_state = self.runtime_problem_call_site_ref(problem_id, &cache_tag);
            self.pad(depth + 1);
            writeln!(
                self.out,
                "auto _harc_rt_call = harc_prepare_randomize_call({prepare_state}, {problem_id}ULL, {global_seed}, 0);"
            )
            .ok();
            self.pad(depth + 1);
            writeln!(self.out, "auto* _harc_rt_problem = _harc_rt_call.problem;").ok();
            self.pad(depth + 1);
            writeln!(self.out, "auto _harc_rt_seed = _harc_rt_call.seed;").ok();
        } else {
            self.pad(depth + 1);
            writeln!(
                self.out,
                "const harc_rt::random::HarcRuntimeProblemDescriptor* _harc_rt_problem = nullptr;"
            )
            .ok();
            let call_site = self.component_call_site_ref(site_number, &cache_tag);
            self.pad(depth + 1);
            writeln!(
                self.out,
                "auto _harc_rt_seed = harc_rt::random::harc_call_site_next_seed({call_site}, {global_seed});"
            )
            .ok();
        }
        self.pad(depth + 1);
        writeln!(
            self.out,
            "auto _harc_rt_generated_solver = [&]() -> harc_rt::random::HarcSolveStatus {{"
        )
        .ok();

        // Context + solver. We use `z3::solver` (not `optimize`) so UNSAT
        // is reported faithfully — `optimize` can return a "best partial"
        // model when soft+hard constraints conflict, hiding real UNSAT.
        // Ordinary diversity comes from deterministic seeded candidate
        // preferences; persistent no-repeat history is reserved for explicit
        // `[unique]` fields.
        self.pad(depth + 1);
        writeln!(self.out, "z3::context _ctx;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "z3::solver _s(_ctx);").ok();
        self.pad(depth + 1);
        writeln!(self.out, "z3::params _p(_ctx);").ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "_p.set(\"random_seed\", static_cast<unsigned>(_harc_rt_seed & 0x7fffffffU));"
        )
        .ok();
        self.pad(depth + 1);
        writeln!(self.out, "_s.set(_p);").ok();

        // All Z3 vars in this solve use one common bit-vector width so
        // binops with literals and across fields don't trip Z3's sort
        // compatibility checks. The common width grows past 64 when a
        // wide field participates; each narrower field then gets a range
        // constraint enforcing its declared width.
        let mut guarded_field_ranges = Vec::new();
        for f in &fields {
            let c_name = c_ident(&f.name);
            if let Some(list) = &f.list {
                let max_len = list_unroll_bounds.get(&f.name).copied().unwrap_or(0);
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "z3::expr _z_{}_len = _ctx.bv_const(\"{}_len\", {});",
                    c_name, c_name, solver_width
                )
                .ok();
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "_s.add(z3::ule(_z_{}_len, _ctx.bv_val((uint64_t){}, {})));",
                    c_name, max_len, solver_width
                )
                .ok();
                if f.non_random {
                    self.pad(depth + 1);
                    write!(
                        self.out,
                        "_s.add(_z_{}_len == _ctx.bv_val((uint64_t)(",
                        c_name
                    )
                    .ok();
                    self.emit_target_field_access(target, f);
                    writeln!(self.out, ".size()), {}));", solver_width).ok();
                }
                for i in 0..max_len {
                    self.pad(depth + 1);
                    writeln!(
                        self.out,
                        "z3::expr _z_{}_{i} = _ctx.bv_const(\"{}_{i}\", {});",
                        c_name, c_name, solver_width
                    )
                    .ok();
                    if list.elem_signed && list.elem_width < solver_width {
                        let (lo_expr, hi_expr) = signed_domain_bound_exprs(list.elem_width);
                        self.pad(depth + 1);
                        writeln!(
                            self.out,
                            "_s.add(_z_{}_{i} >= {});",
                            c_name,
                            solver_bv_value_call(&lo_expr, true, list.elem_width, solver_width)
                        )
                        .ok();
                        self.pad(depth + 1);
                        writeln!(
                            self.out,
                            "_s.add(_z_{}_{i} <= {});",
                            c_name,
                            solver_bv_value_call(&hi_expr, true, list.elem_width, solver_width)
                        )
                        .ok();
                    } else if !list.elem_signed && list.elem_width < solver_width {
                        self.pad(depth + 1);
                        writeln!(
                            self.out,
                            "_s.add(z3::ult(_z_{}_{i}, z3::shl(_ctx.bv_val((uint64_t)1, {}), _ctx.bv_val((uint64_t){}, {}))));",
                            c_name, solver_width, list.elem_width, solver_width
                        )
                        .ok();
                    }
                    if f.non_random {
                        self.pad(depth + 1);
                        write!(self.out, "if (").ok();
                        self.emit_target_field_access(target, f);
                        if list.elem_width <= 64 {
                            write!(self.out, ".size() > {i}) _s.add(_z_{}_{i} == ", c_name).ok();
                            if list.elem_signed {
                                write!(self.out, "harc_z3_bv_signed_value(_ctx, ").ok();
                            } else {
                                write!(self.out, "harc_z3_bv_value(_ctx, ").ok();
                            }
                            self.emit_target_field_access(target, f);
                            if list.elem_signed {
                                writeln!(
                                    self.out,
                                    "[{i}], {}, {}));",
                                    list.elem_width, solver_width
                                )
                                .ok();
                            } else {
                                writeln!(self.out, "[{i}], {}));", solver_width).ok();
                            }
                        } else {
                            write!(self.out, ".size() > {i}) _s.add(_z_{}_{i} == ", c_name).ok();
                            if list.elem_signed {
                                write!(self.out, "harc_z3_bv_signed_value(_ctx, ").ok();
                            } else {
                                write!(self.out, "harc_z3_bv_value(_ctx, ").ok();
                            }
                            self.emit_target_field_access(target, f);
                            if list.elem_signed {
                                writeln!(
                                    self.out,
                                    "[{i}], {}, {}));",
                                    list.elem_width, solver_width
                                )
                                .ok();
                            } else {
                                writeln!(self.out, "[{i}], {}));", solver_width).ok();
                            }
                        }
                    }
                }
                if field_attr_unique(f) && field_attr_unique_scope(f).is_none() {
                    for i in 0..max_len {
                        for j in (i + 1)..max_len {
                            self.pad(depth + 1);
                            writeln!(
                                self.out,
                                "_s.add(z3::ule(_z_{c_name}_len, _ctx.bv_val((uint64_t){j}, {solver_width})) || (_z_{c_name}_{i} != _z_{c_name}_{j}));   // [unique] within this randomize call"
                            )
                            .ok();
                        }
                    }
                }
                continue;
            }
            if f.width == 0 {
                continue;
            }
            self.pad(depth + 1);
            writeln!(
                self.out,
                "z3::expr _z_{} = _ctx.bv_const(\"{}\", {});",
                c_name, c_name, solver_width
            )
            .ok();
            if let Some(n) = f.enum_variants {
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "_s.add(z3::ule(_z_{}, _ctx.bv_val((uint64_t){}, {})));",
                    c_name,
                    n.saturating_sub(1),
                    solver_width
                )
                .ok();
            } else if f.signed && f.width < solver_width {
                let (lo_expr, hi_expr) = signed_domain_bound_exprs(f.width);
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "_s.add(_z_{} >= {});",
                    c_name,
                    solver_bv_value_call(&lo_expr, true, f.width, solver_width)
                )
                .ok();
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "_s.add(_z_{} <= {});",
                    c_name,
                    solver_bv_value_call(&hi_expr, true, f.width, solver_width)
                )
                .ok();
            } else if !f.signed && f.width < solver_width {
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "_s.add(z3::ult(_z_{}, z3::shl(_ctx.bv_val((uint64_t)1, {}), _ctx.bv_val((uint64_t){}, {}))));",
                    c_name, solver_width, f.width, solver_width
                )
                .ok();
            }
            if f.non_random {
                self.pad(depth + 1);
                if f.width <= 64 {
                    if f.signed {
                        write!(
                            self.out,
                            "_s.add(_z_{} == harc_z3_bv_signed_value(_ctx, ",
                            c_name
                        )
                        .ok();
                    } else {
                        write!(self.out, "_s.add(_z_{} == harc_z3_bv_value(_ctx, ", c_name).ok();
                    }
                    self.emit_target_field_access(target, f);
                    if f.signed {
                        writeln!(self.out, ", {}, {}));", f.width, solver_width).ok();
                    } else {
                        writeln!(self.out, ", {}));", solver_width).ok();
                    }
                } else {
                    if f.signed {
                        write!(
                            self.out,
                            "_s.add(_z_{} == harc_z3_bv_signed_value(_ctx, ",
                            c_name
                        )
                        .ok();
                    } else {
                        write!(self.out, "_s.add(_z_{} == harc_z3_bv_value(_ctx, ", c_name).ok();
                    }
                    self.emit_target_field_access(target, f);
                    if f.signed {
                        writeln!(self.out, ", {}, {}));", f.width, solver_width).ok();
                    } else {
                        writeln!(self.out, ", {}));", solver_width).ok();
                    }
                }
            }
            if let Some((lo, hi)) = field_attr_range(f) {
                if f.when_guard.is_some() {
                    guarded_field_ranges.push((f, lo, hi));
                    continue;
                }
                self.emit_solver_range_constraint(
                    f,
                    lo,
                    hi,
                    &field_info,
                    target_root,
                    blocking,
                    solver_width,
                    depth + 1,
                );
            }
        }

        let (guarded_branch_constraints, base_hard_constraints): (Vec<&Expr>, Vec<&Expr>) =
            hard_constraints
                .iter()
                .copied()
                .partition(|c| guarded_constraint_guard(c).is_some());

        // Translated base constraints. Guarded `when` branch constraints
        // are added after a discriminator-only solve below.
        for c in &base_hard_constraints {
            if let ExprKind::ForEachConstraint { var, iter, body } = &*c.kind {
                self.emit_foreach_constraint_clauses(
                    ty,
                    var,
                    iter,
                    body,
                    &field_info,
                    target_root,
                    blocking,
                    &list_unroll_bounds,
                    solver_width,
                    depth + 1,
                );
                continue;
            }
            self.pad(depth + 1);
            write!(self.out, "_s.add(").ok();
            self.emit_constraint_expr_with_list_bounds(
                c,
                &field_info,
                solver_width,
                target_root,
                blocking,
                &list_unroll_bounds,
            );
            writeln!(self.out, ");").ok();
        }
        if !guarded_branch_constraints.is_empty() || !guarded_field_ranges.is_empty() {
            let mut discriminator_fields = std::collections::HashSet::new();
            for c in &guarded_branch_constraints {
                if let Some(guard) = guarded_constraint_guard(c) {
                    collect_randomize_target_field_refs(
                        guard,
                        &field_info,
                        target_root,
                        &mut discriminator_fields,
                    );
                }
            }
            for (f, _, _) in &guarded_field_ranges {
                if let Some(guard) = &f.when_guard {
                    collect_randomize_target_field_refs(
                        guard,
                        &field_info,
                        target_root,
                        &mut discriminator_fields,
                    );
                }
            }
            if !discriminator_fields.is_empty() {
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "// discriminator-first when-subtype solve: pick guard fields before branch-local constraints"
                )
                .ok();
                self.pad(depth + 1);
                writeln!(self.out, "auto _when_r = _s.check();").ok();
                self.pad(depth + 1);
                writeln!(self.out, "if (_when_r == z3::sat) {{").ok();
                self.pad(depth + 2);
                writeln!(self.out, "z3::model _when_m = _s.get_model();").ok();
                let mut discriminator_fields: Vec<String> =
                    discriminator_fields.into_iter().collect();
                discriminator_fields.sort();
                for field in discriminator_fields {
                    let Some(info) = field_info.get(&field) else {
                        continue;
                    };
                    if info.list.is_some() || info.width == 0 {
                        continue;
                    }
                    let c_name = c_ident(&field);
                    self.pad(depth + 2);
                    writeln!(
                        self.out,
                        "_s.add(_z_{} == _when_m.eval(_z_{}, true));",
                        c_name, c_name
                    )
                    .ok();
                }
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
            }
            for c in &guarded_branch_constraints {
                if let ExprKind::ForEachConstraint { var, iter, body } = &*c.kind {
                    self.emit_foreach_constraint_clauses(
                        ty,
                        var,
                        iter,
                        body,
                        &field_info,
                        target_root,
                        blocking,
                        &list_unroll_bounds,
                        solver_width,
                        depth + 1,
                    );
                    continue;
                }
                self.pad(depth + 1);
                write!(self.out, "_s.add(").ok();
                self.emit_constraint_expr_with_list_bounds(
                    c,
                    &field_info,
                    solver_width,
                    target_root,
                    blocking,
                    &list_unroll_bounds,
                );
                writeln!(self.out, ");").ok();
            }
            for (f, lo, hi) in &guarded_field_ranges {
                let Some(guard) = &f.when_guard else {
                    continue;
                };
                self.pad(depth + 1);
                write!(self.out, "_s.add(!(").ok();
                self.emit_constraint_bool_expr_w(
                    guard,
                    &field_info,
                    solver_width,
                    target_root,
                    blocking,
                    &list_unroll_bounds,
                );
                write!(self.out, ") || ").ok();
                self.emit_solver_range_constraint_expr(
                    f,
                    lo,
                    hi,
                    &field_info,
                    target_root,
                    blocking,
                    solver_width,
                    &list_unroll_bounds,
                );
                writeln!(self.out, ");").ok();
            }
        }
        for fields in &solve_order_directives {
            self.pad(depth + 1);
            writeln!(
                self.out,
                "// solve_order({}) accepted as solver scheduling metadata",
                fields.join(", ")
            )
            .ok();
        }
        let mut ordered_soft_constraints: Vec<(usize, &Expr, u32)> = soft_constraints
            .iter()
            .enumerate()
            .map(|(idx, (expr, weight))| (idx, *expr, *weight))
            .collect();
        ordered_soft_constraints.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        for (idx, c, weight) in ordered_soft_constraints {
            self.pad(depth + 1);
            writeln!(
                self.out,
                "_s.push();   // soft constraint weight {}: {}",
                weight,
                escape_c(&expr_source_str(c))
            )
            .ok();
            if let ExprKind::ForEachConstraint { var, iter, body } = &*c.kind {
                self.emit_foreach_constraint_clauses(
                    ty,
                    var,
                    iter,
                    body,
                    &field_info,
                    target_root,
                    blocking,
                    &list_unroll_bounds,
                    solver_width,
                    depth + 1,
                );
            } else {
                self.pad(depth + 1);
                write!(self.out, "_s.add(").ok();
                self.emit_constraint_expr_with_list_bounds(
                    c,
                    &field_info,
                    solver_width,
                    target_root,
                    blocking,
                    &list_unroll_bounds,
                );
                writeln!(self.out, ");").ok();
            }
            self.pad(depth + 1);
            writeln!(self.out, "auto _soft_r_{idx} = _s.check();").ok();
            self.pad(depth + 1);
            writeln!(self.out, "if (_soft_r_{idx} != z3::sat) {{").ok();
            self.pad(depth + 2);
            writeln!(self.out, "_s.pop();").ok();
            self.pad(depth + 2);
            writeln!(
                self.out,
                "sim_log_line(\"INFO\", \"randomize(t) with: dropped unsatisfiable soft constraint `{}` (weight {})\");",
                escape_c(&expr_source_str(c)),
                weight
            )
            .ok();
            self.pad(depth + 1);
            writeln!(self.out, "}}").ok();
        }

        // Detect fields the user has equality-pinned (e.g. `t.addr == 24`).
        // Those fields have only one satisfying value, so they are removed
        // from free-field preference sampling. Scoped uniqueness still tracks
        // them and recycles once the constrained domain is exhausted.
        let pinned: std::collections::HashSet<String> = hard_constraints
            .iter()
            .copied()
            .filter_map(|e| {
                if let ExprKind::Binary {
                    op: BinaryOp::Eq,
                    lhs,
                    rhs,
                } = &*e.kind
                {
                    let pin_from = |side: &Expr, other: &Expr| -> Option<String> {
                        if matches!(&*other.kind, ExprKind::Int(_)) {
                            let field = expr_field_path(side, target_root)?;
                            if field_info.contains_key(&field) {
                                return Some(field);
                            }
                        }
                        None
                    };
                    pin_from(lhs, rhs).or_else(|| pin_from(rhs, lhs))
                } else {
                    None
                }
            })
            .collect();
        for ordered_fields in &solve_order_directives {
            for field in ordered_fields {
                if pinned.contains(field) {
                    self.errors.push(format!(
                        "randomize({ty}) with `solve_order`: field `{field}` is equality-pinned and cannot affect sampling order"
                    ));
                }
            }
        }
        let mut constrained_fields = std::collections::HashSet::new();
        for c in &hard_constraints {
            collect_randomize_target_field_refs(
                c,
                &field_info,
                target_root,
                &mut constrained_fields,
            );
        }

        let mut free_fields: Vec<&TxnFieldInfo> = fields
            .iter()
            .filter(|f| {
                f.list.is_none()
                    && f.width > 0
                    && !f.non_random
                    && !pinned.contains(&f.name)
                    && f.when_guard.is_none()
            })
            .collect();
        let free_field_names: std::collections::HashSet<String> =
            free_fields.iter().map(|f| f.name.clone()).collect();
        if !solve_order_directives.is_empty() {
            let mut before: std::collections::HashMap<String, std::collections::HashSet<String>> =
                std::collections::HashMap::new();
            let mut indegree: std::collections::HashMap<String, usize> = free_fields
                .iter()
                .map(|f| (f.name.clone(), 0usize))
                .collect();
            for ordered_fields in &solve_order_directives {
                for i in 0..ordered_fields.len() {
                    for j in (i + 1)..ordered_fields.len() {
                        let (src, dst) = (&ordered_fields[i], &ordered_fields[j]);
                        if !free_field_names.contains(src) || !free_field_names.contains(dst) {
                            continue;
                        }
                        let entry = before.entry(src.clone()).or_default();
                        if entry.insert(dst.clone()) {
                            *indegree.entry(dst.clone()).or_insert(0) += 1;
                        }
                    }
                }
            }

            let original_order: Vec<String> = free_fields.iter().map(|f| f.name.clone()).collect();
            let mut ordered_names = Vec::new();
            let mut ready: Vec<String> = original_order
                .iter()
                .filter(|name| indegree.get(*name).copied().unwrap_or(0) == 0)
                .cloned()
                .collect();
            while let Some(name) = ready.first().cloned() {
                ready.remove(0);
                ordered_names.push(name.clone());
                if let Some(nexts) = before.get(&name) {
                    for candidate in &original_order {
                        if !nexts.contains(candidate) {
                            continue;
                        }
                        if let Some(n) = indegree.get_mut(candidate) {
                            *n = n.saturating_sub(1);
                            if *n == 0 {
                                ready.push(candidate.clone());
                            }
                        }
                    }
                }
            }

            if ordered_names.len() != free_fields.len() {
                self.errors.push(format!(
                    "randomize({ty}) solve-order hints form a cycle among free fields"
                ));
            } else {
                let rank: std::collections::HashMap<String, usize> = ordered_names
                    .iter()
                    .enumerate()
                    .map(|(i, name)| (name.clone(), i))
                    .collect();
                free_fields.sort_by_key(|f| rank.get(&f.name).copied().unwrap_or(usize::MAX));
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "// solve-order sampling order: {}",
                    ordered_names.join(", ")
                )
                .ok();
            }
        }
        let unique_fields: std::collections::HashSet<String> = fields
            .iter()
            .filter(|f| {
                f.list.is_none()
                    && f.width > 0
                    && !f.non_random
                    && f.when_guard.is_none()
                    && field_attr_unique_scope(f).is_some()
            })
            .map(|f| f.name.clone())
            .collect();
        let auto_goals: Vec<AutoCoverageGoal> = free_fields
            .iter()
            .filter(|f| !constrained_fields.contains(&f.name))
            .filter_map(|f| {
                let values = auto_coverage_values(f);
                (!values.is_empty()).then(|| AutoCoverageGoal {
                    field: f.name.clone(),
                    c_field: c_ident(&f.name),
                    values,
                })
            })
            .collect();
        let mut auto_crosses: Vec<(AutoCoverageGoal, AutoCoverageGoal)> = Vec::new();
        for i in 0..auto_goals.len() {
            for j in (i + 1)..auto_goals.len() {
                let cross_bins = auto_goals[i].values.len() * auto_goals[j].values.len();
                if cross_bins <= 64 && auto_crosses.len() < 16 {
                    auto_crosses.push((auto_goals[i].clone(), auto_goals[j].clone()));
                }
            }
        }

        // History is a semantic policy now, not the default source of
        // diversity. Ordinary random fields are sampled by seeded
        // preferences; only explicit `[unique]` fields keep no-repeat state
        // across calls.
        for f in fields.iter().filter(|f| unique_fields.contains(&f.name)) {
            let c_name = c_ident(&f.name);
            let scope = field_attr_unique_scope(f).expect("persistent unique scope");
            let value_ty = txn_field_solver_c_type(f);
            let unique_cell = self.unique_history_ref(scope, ty, &c_name, &value_ty);
            let _ = unique_cell;
        }
        if !auto_goals.is_empty() {
            for goal in &auto_goals {
                self.pad(depth + 1);
                write!(
                    self.out,
                    "static const char* _auto_cov_labels_{cache_tag}_{}[] = {{",
                    goal.c_field
                )
                .ok();
                for (idx, value) in goal.values.iter().enumerate() {
                    if idx > 0 {
                        write!(self.out, ", ").ok();
                    }
                    write!(
                        self.out,
                        "\"{}.{}={}\"",
                        escape_c(ty),
                        escape_c(&goal.field),
                        escape_c(&value.label)
                    )
                    .ok();
                }
                writeln!(self.out, "}};").ok();
            }
            for (a, b) in &auto_crosses {
                self.pad(depth + 1);
                write!(
                    self.out,
                    "static const char* _auto_cross_labels_{cache_tag}_{}__{}[] = {{",
                    a.c_field, b.c_field
                )
                .ok();
                let mut first = true;
                for av in &a.values {
                    for bv in &b.values {
                        if !first {
                            write!(self.out, ", ").ok();
                        }
                        first = false;
                        write!(
                            self.out,
                            "\"{}.{}={} x {}.{}={}\"",
                            escape_c(ty),
                            escape_c(&a.field),
                            escape_c(&av.label),
                            escape_c(ty),
                            escape_c(&b.field),
                            escape_c(&bv.label)
                        )
                        .ok();
                    }
                }
                writeln!(self.out, "}};").ok();
            }
            self.pad(depth + 1);
            writeln!(
                self.out,
                "static const harc_rt::random::HarcAutoCovPointMeta _auto_cov_points_{cache_tag}[] = {{"
            )
            .ok();
            for goal in &auto_goals {
                self.pad(depth + 2);
                writeln!(
                    self.out,
                    "{{_auto_cov_labels_{cache_tag}_{}, {}}},",
                    goal.c_field,
                    goal.values.len()
                )
                .ok();
            }
            self.pad(depth + 1);
            writeln!(self.out, "}};").ok();
            if !auto_crosses.is_empty() {
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "static const harc_rt::random::HarcAutoCovCrossMeta _auto_cov_crosses_{cache_tag}[] = {{"
                )
                .ok();
                for (a, b) in &auto_crosses {
                    self.pad(depth + 2);
                    writeln!(
                        self.out,
                        "{{_auto_cross_labels_{cache_tag}_{}__{}, {}, {}}},",
                        a.c_field,
                        b.c_field,
                        a.values.len(),
                        b.values.len()
                    )
                    .ok();
                }
                self.pad(depth + 1);
                writeln!(self.out, "}};").ok();
            }
            self.pad(depth + 1);
            writeln!(
                self.out,
                "static const harc_rt::random::HarcAutoCovPlan _auto_cov_plan_{cache_tag} = {{\"{}\", {}ULL, _auto_cov_points_{cache_tag}, {}, {}, {}}};",
                escape_c(ty),
                site_number,
                auto_goals.len(),
                if auto_crosses.is_empty() {
                    "nullptr".to_string()
                } else {
                    format!("_auto_cov_crosses_{cache_tag}")
                },
                auto_crosses.len()
            )
            .ok();
            let cov_state_cell = self.auto_cov_state_ref(&cache_tag);
            self.pad(depth + 1);
            writeln!(
                self.out,
                "harc_rt::random::harc_auto_cov_init(_auto_cov_plan_{cache_tag}, {cov_state_cell});"
            )
            .ok();
            let cov_report_cell = self.per_run_cell(
                format!("_auto_cov_report_registered_{cache_tag}"),
                "bool",
                Some("false"),
            );
            self.pad(depth + 1);
            writeln!(
                self.out,
                "harc_rt::random::harc_auto_cov_register_report({cov_report_cell}, _auto_cov_reports, [&]() {{"
            )
            .ok();
            self.pad(depth + 2);
            writeln!(
                self.out,
                "harc_rt::random::harc_auto_cov_report(_auto_cov_plan_{cache_tag}, {cov_state_cell}, log_ctx.coverage_json);"
            )
            .ok();
            self.pad(depth + 1);
            writeln!(self.out, "}});").ok();
        }

        // Seeded preference values. These are hard clauses only for the first
        // check; if the preferred tuple is incompatible with the user's
        // constraints, we drop the preference stack and fall back to the
        // base solve. This makes solver-backed randomize
        // consume the HARC seed without turning preferences into false UNSATs.
        for (pref_idx, f) in free_fields.iter().enumerate() {
            let c_name = c_ident(&f.name);
            self.pad(depth + 1);
            let dist_entries = dist_directives
                .get(&f.name)
                .map(Vec::as_slice)
                .or_else(|| field_attr_dist_entries(f).map(Vec::as_slice));
            if let Some(entries) = dist_entries {
                if f.width > 64 {
                    self.errors.push(format!(
                        "randomize({ty}) [dist] on >64-bit field `{}` is not supported yet",
                        f.name
                    ));
                }
                write!(
                    self.out,
                    "uint64_t _pref_{cache_tag}_{} = (uint64_t)harc_rt::random::harc_prefer_dist(_harc_rt_seed, {}, {{",
                    c_name, pref_idx
                )
                .ok();
                self.emit_rng_dist_entries(entries);
                writeln!(self.out, "}});").ok();
            } else if let Some(n) = f.enum_variants {
                writeln!(
                    self.out,
                    "uint64_t _pref_{cache_tag}_{} = (uint64_t)harc_rt::random::harc_prefer_range(_harc_rt_seed, {}, 0, {});",
                    c_name,
                    pref_idx,
                    n.saturating_sub(1)
                )
                .ok();
            } else if let Some(expr) = literal_range_preference(f, pref_idx, solver_width) {
                let value_ty = if f.signed {
                    "int64_t".to_string()
                } else if f.width <= 64 {
                    "uint64_t".to_string()
                } else {
                    txn_field_solver_c_type(f)
                };
                writeln!(
                    self.out,
                    "{value_ty} _pref_{cache_tag}_{c_name} = static_cast<{value_ty}>({expr});"
                )
                .ok();
            } else if f.signed && f.width > 0 && f.width <= 64 {
                if f.width == 63 {
                    writeln!(
                        self.out,
                        "int64_t _pref_{cache_tag}_{} = static_cast<int64_t>(harc_rt::harc_sext_u128(static_cast<_harc_u128>(harc_rt::random::harc_prefer_uint(_harc_rt_seed, {}, 63)), 63, 64));",
                        c_name,
                        pref_idx,
                    )
                    .ok();
                } else {
                    writeln!(
                    self.out,
                    "int64_t _pref_{cache_tag}_{} = harc_rt::random::harc_prefer_sint(_harc_rt_seed, {}, {});",
                    c_name,
                    pref_idx,
                    f.width
                )
                .ok();
                }
            } else if f.width <= 64 {
                writeln!(
                    self.out,
                    "uint64_t _pref_{cache_tag}_{} = harc_rt::random::harc_prefer_uint(_harc_rt_seed, {}, {});",
                    c_name, pref_idx, f.width
                )
                .ok();
            } else {
                let value_ty = txn_field_solver_c_type(f);
                let expr = emit_random_pref_expr(f, pref_idx);
                writeln!(
                    self.out,
                    "{} _pref_{cache_tag}_{} = {};",
                    value_ty, c_name, expr
                )
                .ok();
            }
        }
        if !auto_goals.is_empty() {
            self.pad(depth + 1);
            writeln!(
                self.out,
                "harc_rt::random::HarcAutoCovSelection _auto_cov_selection_{cache_tag};   // auto coverage preference"
            )
            .ok();
        }
        for (group, (a, b)) in auto_crosses.iter().enumerate() {
            self.pad(depth + 1);
            let a_ty = auto_value_array_type(a, &field_info);
            writeln!(
                self.out,
                "static const {} _auto_cross_vals_{cache_tag}_{}__{}_{}[] = {{{}}};",
                a_ty,
                a.c_field,
                b.c_field,
                a.c_field,
                auto_value_initializer(&a.values)
            )
            .ok();
            self.pad(depth + 1);
            let b_ty = auto_value_array_type(b, &field_info);
            writeln!(
                self.out,
                "static const {} _auto_cross_vals_{cache_tag}_{}__{}_{}[] = {{{}}};",
                b_ty,
                a.c_field,
                b.c_field,
                b.c_field,
                auto_value_initializer(&b.values)
            )
            .ok();
            let cov_state = self.auto_cov_state_ref(&cache_tag);
            self.pad(depth + 1);
            writeln!(
                self.out,
                "harc_rt::random::harc_auto_cov_apply_cross_preference(_auto_cov_plan_{cache_tag}, {cov_state}, _auto_cov_selection_{cache_tag}, {group}, _auto_cross_vals_{cache_tag}_{}__{}_{}, _auto_cross_vals_{cache_tag}_{}__{}_{}, _pref_{cache_tag}_{}, _pref_{cache_tag}_{});",
                a.c_field,
                b.c_field,
                a.c_field,
                a.c_field,
                b.c_field,
                b.c_field,
                a.c_field,
                b.c_field
            )
            .ok();
        }
        for (group, goal) in auto_goals.iter().enumerate() {
            let cov_state = self.auto_cov_state_ref(&cache_tag);
            self.pad(depth + 1);
            let value_ty = auto_value_array_type(goal, &field_info);
            writeln!(
                self.out,
                "static const {} _auto_point_vals_{cache_tag}_{}[] = {{{}}};",
                value_ty,
                goal.c_field,
                auto_value_initializer(&goal.values)
            )
            .ok();
            self.pad(depth + 1);
            writeln!(
                self.out,
                "harc_rt::random::harc_auto_cov_apply_point_preference(_auto_cov_plan_{cache_tag}, {cov_state}, _auto_cov_selection_{cache_tag}, {group}, _auto_point_vals_{cache_tag}_{}, _pref_{cache_tag}_{});",
                goal.c_field, goal.c_field
            )
            .ok();
        }
        if !auto_goals.is_empty() {
            let cov_state = self.auto_cov_state_ref(&cache_tag);
            self.pad(depth + 1);
            writeln!(self.out, "harc_rt::random::harc_auto_cov_finish_selection({cov_state}, _auto_cov_selection_{cache_tag});").ok();
        }
        self.pad(depth + 1);
        writeln!(
            self.out,
            "harc_rt::random::HarcSolverRetryPolicy _harc_rt_retry_policy;"
        )
        .ok();
        // Apply uniqueness history before seeded preferences. Keep it in its
        // own frame so the exhaustion fallback can pop the exclusions and
        // recycle the domain after clearing the persistent history.
        self.pad(depth + 1);
        writeln!(self.out, "_s.push();   // unique history constraints").ok();
        for f in fields.iter().filter(|f| unique_fields.contains(&f.name)) {
            let c_name = c_ident(&f.name);
            let scope = field_attr_unique_scope(f).expect("persistent unique scope");
            self.pad(depth + 1);
            let v_expr = solver_bv_value_call("_v", f.signed, f.width, solver_width);
            let value_ty = txn_field_solver_c_type(f);
            let unique_use = self.unique_history_ref(scope, ty, &c_name, &value_ty);
            let scope_txt = escape_c(scope);
            writeln!(
                self.out,
                "for (auto _v : harc_rt::random::harc_unique_values({uu})) _s.add(_z_{cn} != {ve});   // [unique within {sc}] policy: no repeat until exhausted",
                uu = unique_use,
                cn = c_name,
                ve = v_expr,
                sc = scope_txt,
            )
            .ok();
        }
        self.pad(depth + 1);
        writeln!(
            self.out,
            "_s.push();   // seeded candidate preferences (free fields only)"
        )
        .ok();
        for f in &free_fields {
            let c_name = c_ident(&f.name);
            self.pad(depth + 1);
            let pref_expr = format!("_pref_{cache_tag}_{c_name}");
            writeln!(
                self.out,
                "_s.add(_z_{} == {});",
                c_name,
                solver_bv_value_call(&pref_expr, f.signed, f.width, solver_width)
            )
            .ok();
        }
        self.pad(depth + 1);
        writeln!(self.out, "auto _r = _s.check();").ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "if (harc_rt::random::harc_retry_without_preferences(_harc_rt_retry_policy, _r == z3::sat)) {{"
        )
        .ok();
        // A failed preferred tuple does not prove any coverage goal unreachable.
        // History and call-dependent hard constraints also make persistent
        // BLOCKED inference unsafe. Leave unproven goals available for retry.
        if !auto_goals.is_empty() {
            let cov_state = self.auto_cov_state_ref(&cache_tag);
            self.pad(depth + 2);
            writeln!(self.out, "harc_rt::random::harc_auto_cov_defer_selected(_auto_cov_plan_{cache_tag}, {cov_state}, _auto_cov_selection_{cache_tag});").ok();
        }
        self.pad(depth + 2);
        writeln!(self.out, "_s.pop();").ok();
        self.pad(depth + 2);
        writeln!(self.out, "_s.push();   // retry without seeded preferences").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        // Final check: seeded preferences have already been dropped if they
        // conflict. If `[unique]` history saturates the satisfiable space,
        // clear that history and retry under the original hard constraints.
        self.pad(depth + 1);
        writeln!(self.out, "_r = _s.check();").ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "if (harc_rt::random::harc_retry_without_unique_history(_harc_rt_retry_policy, _r == z3::sat)) {{"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "bool _harc_recycled_unique_history = false;").ok();
        self.pad(depth + 2);
        writeln!(self.out, "_s.pop();   // drop preference scope").ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "_s.pop();   // drop exhausted unique-history scope"
        )
        .ok();
        for candidate in fields.iter().filter(|f| unique_fields.contains(&f.name)) {
            let candidate_name = c_ident(&candidate.name);
            let candidate_scope =
                field_attr_unique_scope(candidate).expect("persistent unique scope");
            self.pad(depth + 2);
            writeln!(self.out, "if (!_harc_recycled_unique_history) {{").ok();
            self.pad(depth + 3);
            writeln!(self.out, "_s.push();").ok();
            for other in fields
                .iter()
                .filter(|f| unique_fields.contains(&f.name) && f.name != candidate.name)
            {
                let c_name = c_ident(&other.name);
                let scope = field_attr_unique_scope(other).expect("persistent unique scope");
                let v_expr = solver_bv_value_call("_v", other.signed, other.width, solver_width);
                let value_ty = txn_field_solver_c_type(other);
                let unique_use = self.unique_history_ref(scope, ty, &c_name, &value_ty);
                self.pad(depth + 3);
                writeln!(
                    self.out,
                    "for (auto _v : harc_rt::random::harc_unique_values({uu})) _s.add(_z_{cn} != {ve});",
                    uu = unique_use,
                    cn = c_name,
                    ve = v_expr,
                )
                .ok();
            }
            self.pad(depth + 3);
            writeln!(self.out, "_r = _s.check();").ok();
            self.pad(depth + 3);
            writeln!(self.out, "if (_r == z3::sat) {{").ok();
            let candidate_cell = self.unique_history_ref(
                candidate_scope,
                ty,
                &candidate_name,
                &txn_field_solver_c_type(candidate),
            );
            self.pad(depth + 4);
            writeln!(
                self.out,
                "harc_rt::random::harc_unique_clear({candidate_cell});"
            )
            .ok();
            self.pad(depth + 4);
            writeln!(self.out, "_harc_recycled_unique_history = true;").ok();
            self.pad(depth + 3);
            writeln!(self.out, "}} else {{").ok();
            self.pad(depth + 4);
            writeln!(self.out, "_s.pop();").ok();
            self.pad(depth + 3);
            writeln!(self.out, "}}\n").ok();
            self.pad(depth + 2);
            writeln!(self.out, "}}").ok();
        }
        self.pad(depth + 2);
        writeln!(self.out, "if (!_harc_recycled_unique_history) {{").ok();
        for f in fields.iter().filter(|f| unique_fields.contains(&f.name)) {
            let c_name = c_ident(&f.name);
            let scope = field_attr_unique_scope(f).expect("persistent unique scope");
            let cell = self.unique_history_ref(scope, ty, &c_name, &txn_field_solver_c_type(f));
            self.pad(depth + 3);
            writeln!(self.out, "harc_rt::random::harc_unique_clear({cell});").ok();
        }
        self.pad(depth + 3);
        writeln!(self.out, "_r = _s.check();").ok();
        self.pad(depth + 2);
        writeln!(self.out, "}}").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();

        // Assign model values back into t.
        self.pad(depth + 1);
        writeln!(self.out, "if (_r == z3::sat) {{").ok();
        self.pad(depth + 2);
        writeln!(self.out, "z3::model _m = _s.get_model();").ok();
        let field_root_names: std::collections::HashSet<String> = field_info
            .keys()
            .filter_map(|name| name.split('.').next().map(str::to_string))
            .collect();
        for f in &fields {
            let c_name = c_ident(&f.name);
            if f.non_random {
                continue;
            }
            if let Some(list) = &f.list {
                let max_len = list_unroll_bounds.get(&f.name).copied().unwrap_or(0);
                let guard_expr = f.when_guard.as_ref().map(|guard| {
                    self.enum_variants_as_ints(&prefix_record_keep_expr(
                        guard,
                        target,
                        &field_root_names,
                    ))
                });
                let list_depth = if guard_expr.is_some() {
                    depth + 3
                } else {
                    depth + 2
                };
                if let Some(guard) = &guard_expr {
                    self.pad(depth + 2);
                    write!(self.out, "if (").ok();
                    self.emit_expr(guard);
                    writeln!(self.out, ") {{   // active when-subtype field {}", f.name).ok();
                }
                self.pad(list_depth);
                writeln!(
                    self.out,
                    "z3::expr _eval_{}_len = _m.eval(_z_{}_len, true).simplify();",
                    c_name, c_name
                )
                .ok();
                self.pad(list_depth);
                writeln!(self.out, "uint64_t _raw_{}_len = 0;", c_name).ok();
                self.pad(list_depth);
                writeln!(
                    self.out,
                    "if (!_eval_{}_len.is_numeral_u64(_raw_{}_len)) {{",
                    c_name, c_name
                )
                .ok();
                self.pad(list_depth + 1);
                writeln!(
                    self.out,
                    "sim_log_line(\"FAIL\", \"randomize(t) with: solver model for field '{}.len' is not a uint64 numeral\");",
                    escape_c(&f.name)
                )
                .ok();
                self.pad(list_depth + 1);
                writeln!(self.out, "{runtime_context}.errors++;").ok();
                self.pad(list_depth);
                writeln!(self.out, "}}").ok();
                self.pad(list_depth);
                writeln!(
                    self.out,
                    "if (_raw_{}_len > {}) _raw_{}_len = {};",
                    c_name, max_len, c_name, max_len
                )
                .ok();
                self.pad(list_depth);
                self.emit_target_field_access(target, f);
                writeln!(self.out, ".resize((size_t)_raw_{}_len);", c_name).ok();
                for i in 0..max_len {
                    self.pad(list_depth);
                    writeln!(self.out, "if (_raw_{}_len > {i}) {{", c_name).ok();
                    self.pad(list_depth + 1);
                    writeln!(
                        self.out,
                        "z3::expr _eval_{}_{i} = _m.eval(_z_{}_{i}, true).simplify();",
                        c_name, c_name
                    )
                    .ok();
                    self.pad(list_depth + 1);
                    if list.elem_width <= 64 {
                        let val_ty = solver_scalar_c_type(list.elem_width, list.elem_signed);
                        writeln!(
                            self.out,
                            "uint64_t _raw_{}_{i} = harc_z3_bv_low_u64(_ctx, _eval_{}_{i});",
                            c_name, c_name
                        )
                        .ok();
                        self.pad(list_depth + 1);
                        self.emit_target_field_access(target, f);
                        writeln!(self.out, "[{i}] = ({})_raw_{}_{i};", val_ty, c_name).ok();
                    } else {
                        let val_ty = solver_scalar_c_type(list.elem_width, list.elem_signed);
                        let words = list.elem_width.div_ceil(32);
                        writeln!(
                            self.out,
                            "const char* _bin_{}_{i} = Z3_get_numeral_binary_string(_ctx, _eval_{}_{i});",
                            c_name, c_name
                        )
                        .ok();
                        self.pad(list_depth + 1);
                        writeln!(self.out, "if (_bin_{}_{i} == nullptr) {{", c_name).ok();
                        self.pad(list_depth + 2);
                        writeln!(
                            self.out,
                            "sim_log_line(\"FAIL\", \"randomize(t) with: solver model for field '{}[{i}]' is not a binary numeral\");",
                            escape_c(&f.name)
                        )
                        .ok();
                        self.pad(list_depth + 2);
                        writeln!(self.out, "{runtime_context}.errors++;").ok();
                        self.pad(list_depth + 1);
                        writeln!(self.out, "}}").ok();
                        self.pad(list_depth + 1);
                        self.emit_target_field_access(target, f);
                        writeln!(
                            self.out,
                            "[{i}] = ({})harc_rt::harc_wide_from_binary<{}>(_bin_{}_{i});",
                            val_ty, words, c_name
                        )
                        .ok();
                    }
                    self.pad(list_depth);
                    writeln!(self.out, "}}").ok();
                }
                if guard_expr.is_some() {
                    self.pad(depth + 2);
                    writeln!(self.out, "}}").ok();
                }
                continue;
            }
            if f.width == 0 {
                continue;
            }
            let guard_expr = f.when_guard.as_ref().map(|guard| {
                self.enum_variants_as_ints(&prefix_record_keep_expr(
                    guard,
                    target,
                    &field_root_names,
                ))
            });
            let scalar_depth = if guard_expr.is_some() {
                depth + 3
            } else {
                depth + 2
            };
            if let Some(guard) = &guard_expr {
                self.pad(depth + 2);
                write!(self.out, "if (").ok();
                self.emit_expr(guard);
                writeln!(self.out, ") {{   // active when-subtype field {}", f.name).ok();
            }
            // Every declared field is assigned from the model — equality-
            // pinned fields take their constrained value; free fields take
            // a Z3-chosen satisfying value. Only explicit `[unique]` fields
            // get pushed into persistent no-repeat history.
            self.pad(scalar_depth);
            writeln!(
                self.out,
                "z3::expr _eval_{} = _m.eval(_z_{}, true).simplify();",
                c_name, c_name
            )
            .ok();
            if f.width <= 64 {
                let val_ty = if f.signed { "int64_t" } else { "uint64_t" };
                self.pad(scalar_depth);
                writeln!(
                    self.out,
                    "uint64_t _raw_{} = harc_z3_bv_low_u64(_ctx, _eval_{});",
                    c_name, c_name
                )
                .ok();
                self.pad(scalar_depth);
                writeln!(
                    self.out,
                    "{} _val_{} = ({})_raw_{};",
                    val_ty, c_name, val_ty, c_name
                )
                .ok();
            } else {
                let val_ty = txn_field_solver_c_type(f);
                let words = f.width.div_ceil(32);
                self.pad(scalar_depth);
                writeln!(
                    self.out,
                    "const char* _bin_{} = Z3_get_numeral_binary_string(_ctx, _eval_{});",
                    c_name, c_name
                )
                .ok();
                self.pad(scalar_depth);
                writeln!(self.out, "if (_bin_{} == nullptr) {{", c_name).ok();
                self.pad(scalar_depth + 1);
                writeln!(
                    self.out,
                    "sim_log_line(\"FAIL\", \"randomize(t) with: solver model for field '{}' is not a binary numeral\");",
                    escape_c(&f.name)
                )
                .ok();
                self.pad(scalar_depth + 1);
                writeln!(self.out, "{runtime_context}.errors++;").ok();
                self.pad(scalar_depth);
                writeln!(self.out, "}}").ok();
                self.pad(scalar_depth);
                writeln!(
                    self.out,
                    "{} _val_{} = ({})harc_rt::harc_wide_from_binary<{}>(_bin_{});",
                    val_ty, c_name, val_ty, words, c_name
                )
                .ok();
            }
            self.pad(scalar_depth);
            write!(self.out, "").ok();
            self.emit_target_field_access(target, f);
            writeln!(self.out, " = _val_{};", c_name).ok();
            if f.when_guard.is_none() && unique_fields.contains(&f.name) {
                let scope = field_attr_unique_scope(f).expect("persistent unique scope");
                let cell = self.unique_history_ref(scope, ty, &c_name, &txn_field_solver_c_type(f));
                self.pad(scalar_depth);
                writeln!(
                    self.out,
                    "harc_rt::random::harc_unique_remember({cell}, _val_{});",
                    c_name
                )
                .ok();
            }
            if guard_expr.is_some() {
                self.pad(depth + 2);
                writeln!(self.out, "}}").ok();
            }
        }
        for (group, goal) in auto_goals.iter().enumerate() {
            let cov_state = self.auto_cov_state_ref(&cache_tag);
            for (idx, value) in goal.values.iter().enumerate() {
                self.pad(depth + 2);
                writeln!(
                    self.out,
                    "harc_rt::random::harc_auto_cov_mark_value_hit(_val_{}, {}, _auto_cov_plan_{cache_tag}, {cov_state}, {}, {});",
                    goal.c_field, value.c_expr, group, idx
                )
                .ok();
            }
        }
        for (group, (a, b)) in auto_crosses.iter().enumerate() {
            let cov_state = self.auto_cov_state_ref(&cache_tag);
            for (i, av) in a.values.iter().enumerate() {
                for (j, bv) in b.values.iter().enumerate() {
                    self.pad(depth + 2);
                    writeln!(
                        self.out,
                        "harc_rt::random::harc_auto_cov_mark_cross_hit(_val_{}, {}, _val_{}, {}, _auto_cov_plan_{cache_tag}, {cov_state}, {}, {}, {});",
                        a.c_field, av.c_expr, b.c_field, bv.c_expr, group, i, j
                    )
                    .ok();
                }
            }
        }
        self.emit_randomize_trace_event(ty, target, depth + 2);
        self.pad(depth + 2);
        writeln!(self.out, "return harc_rt::random::harc_solve_status_ok();").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}} else {{").ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "auto _harc_rt_status = harc_rt::random::harc_solve_status_unsat(_harc_rt_problem ? _harc_rt_problem->id : 0, _harc_rt_seed);"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "sim_log_line(\"FAIL\", \"%s\", _harc_rt_status.message ? _harc_rt_status.message : \"randomize(t) with: constraint UNSAT\");"
        )
        .ok();
        let mut constraint_origins: Vec<String> = hard_constraints
            .iter()
            .map(|expr| format!("constraint `{}`", expr_source_str(expr)))
            .collect();
        for f in &fields {
            if field_attr_range(f).is_some() {
                constraint_origins.push(format!("field attribute `[range]` on `{ty}.{}`", f.name));
            }
        }
        constraint_origins.sort();
        constraint_origins.dedup();
        for origin in constraint_origins {
            self.pad(depth + 2);
            writeln!(
                self.out,
                "sim_log_line(\"FAIL\", \"randomize(t) with: {} participated in the solve\");",
                escape_c(&origin)
            )
            .ok();
        }
        let mut when_guards: Vec<String> = fields
            .iter()
            .filter_map(|f| f.when_guard.as_ref().map(expr_source_str))
            .collect();
        when_guards.extend(
            hard_constraints
                .iter()
                .filter_map(|expr| guarded_constraint_guard(expr).map(expr_source_str)),
        );
        when_guards.sort();
        when_guards.dedup();
        for guard in when_guards {
            self.pad(depth + 2);
            writeln!(
                self.out,
                "sim_log_line(\"FAIL\", \"randomize(t) with: active when subtype guard `{}` participated in the solve\");",
                escape_c(&guard)
            )
            .ok();
        }
        self.pad(depth + 2);
        writeln!(self.out, "{runtime_context}.errors++;").ok();
        self.pad(depth + 2);
        writeln!(self.out, "return _harc_rt_status;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}};").ok();
        self.pad(depth + 1);
        write!(
            self.out,
            "auto _harc_rt_solve_status = harc_rt::random::harc_solve_constrained("
        )
        .ok();
        self.emit_randomize_target(target);
        writeln!(
            self.out,
            ", _harc_rt_problem ? _harc_rt_problem->id : 0, _harc_rt_seed, harc_rt::random::HarcSolveMode::{}, _harc_rt_generated_solver);",
            if blocking { "Blocking" } else { "Queued" }
        )
        .ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "(void)harc_rt::random::harc_handle_solve_status(_harc_rt_solve_status);"
        )
        .ok();

        self.pad(depth);
        writeln!(self.out, "}}").ok();
    }

    /// Operand bit-width of a constraint expression, for the `+% -% *%`
    /// mask. `None` when no operand carries a width — the same "wrap width
    /// undefined" condition both emitters reject at the statement level.
    ///
    /// Two ways to get this wrong, both found in review:
    ///
    /// - Resolving a `Field` by its leaf `name` finds an unrelated
    ///   top-level field of the same name, so `hdr.len : uint<16>` next to
    ///   a `len : uint<8>` masks to 8 bits. The solver then returns a value
    ///   that does not satisfy the source constraint — silent, and worse
    ///   than the unsatisfiable report this mask exists to prevent. Dotted
    ///   paths go through `expr_field_path`, exactly as the emitter does.
    /// - Resolving *less* than the emitter turns a constraint the emitter
    ///   would have emitted into a hard build error.
    ///
    /// The second is why this reads only state BOTH emitter constructors
    /// populate: `field_info`, `const_widths`, `enum_variants`, and pure
    /// syntax. **Do not add a fallback that reads `let_types`,
    /// `let_widths` or `probe_widths` to close it.** Those are empty in
    /// `build_randomize_emitter`, which emits per constraint site and
    /// never walks statements, so such a fallback resolves on
    /// `--codegen v1` alone and makes the two backends disagree about the
    /// same source — a worse failure than the build error, and the reason
    /// the `blocking` operand fallback added here was backed out again.
    /// `let_types` is also unscoped (`emit_let` inserts with no block
    /// restore), so a dead inner shadow supplies a wrong width. Both the
    /// blocking gap and its scoping prerequisite are tracked in harc#566.
    ///
    /// `sum(...)` and `.len()` are deliberately absent for a different
    /// reason: the emitter turns them into solver-internal variables with
    /// no declared source width, so there is no honest mask to apply.
    fn constraint_expr_width(
        &self,
        e: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        target_root: Option<&str>,
    ) -> Option<u32> {
        let recur = |inner| self.constraint_expr_width(inner, field_info, target_root);
        match &*e.kind {
            // A bare ident is its own whole path — resolve it the way the
            // emitter's `Ident` arm does, NOT through `expr_field_path`,
            // whose `target_root` strip would erase a field named after
            // the randomize target (`randomize(t) with t +% 10 == 5`).
            ExprKind::Ident(id) => {
                if let Some(info) = field_info.get(&id.name) {
                    return Some(txn_field_solver_width(info));
                }
                if let Some(width) = self.const_widths.get(&id.name) {
                    return Some(*width);
                }
                if let Some(idx) = self.enum_variants.get(&id.name) {
                    return u64::try_from(*idx).ok().map(value_bit_width);
                }
                None
            }
            ExprKind::Field { .. } => expr_field_path(e, target_root)
                .and_then(|field| field_info.get(&field))
                .map(txn_field_solver_width),
            // `items[i]` — a list element has the list's element width.
            // A `foreach` clause is unrolled into exactly this shape, so
            // without it every `foreach` constraint containing a wrap
            // would be rejected for an unknown width. A bit-select on a
            // scalar field is one bit.
            ExprKind::Index { target, .. } => {
                if let Some(field) = list_field_name_from_expr(target, field_info, target_root) {
                    return field_info.get(&field).map(txn_field_solver_width);
                }
                expr_field_path(target, target_root)
                    .and_then(|field| field_info.get(&field))
                    .filter(|info| info.list.is_none() && info.width != 0)
                    .map(|_| 1)
            }
            ExprKind::BitSlice { hi, lo, .. } => {
                let hi = const_usize_expr(hi)?;
                let lo = const_usize_expr(lo)?;
                (hi >= lo).then(|| (hi - lo + 1) as u32)
            }
            ExprKind::Paren(inner) => recur(inner),
            ExprKind::Cast { ty, .. } => crate::ir::lower::cast_relabel_width(ty),
            ExprKind::Int(s) => literal_operand_bit_width(s),
            ExprKind::Binary {
                op: BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap,
                lhs,
                rhs,
            } => Some(recur(lhs)?.max(recur(rhs)?)),
            _ => None,
        }
    }

    /// Classify operands using native usual arithmetic conversions. Untyped
    /// literals take the signedness of a typed operand.

    fn constraint_expr_signedness(
        &self,
        e: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        target_root: Option<&str>,
    ) -> ConstraintOperandSignedness {
        let recur = |x| self.constraint_expr_signedness(x, field_info, target_root);
        match &*e.kind {
            ExprKind::Ident(id) => field_info
                .get(&id.name)
                .map(|f| {
                    if f.signed {
                        ConstraintOperandSignedness::Signed
                    } else {
                        ConstraintOperandSignedness::Unsigned
                    }
                })
                .or_else(|| {
                    (!self.shadowed_lets.contains(&id.name))
                        .then(|| self.let_widths.get(&id.name))
                        .flatten()
                        .map(|l| {
                            if l.signed {
                                ConstraintOperandSignedness::Signed
                            } else {
                                ConstraintOperandSignedness::Unsigned
                            }
                        })
                })
                .or_else(|| {
                    self.consts.get(&id.name).map(|c| match c.signedness {
                        crate::constraints::Signedness::Signed
                            if c.width.is_none() && !c.literal.starts_with('-') =>
                        {
                            ConstraintOperandSignedness::UntypedLiteral
                        }
                        crate::constraints::Signedness::Signed => {
                            ConstraintOperandSignedness::Signed
                        }
                        crate::constraints::Signedness::Unsigned => {
                            ConstraintOperandSignedness::Unsigned
                        }
                        crate::constraints::Signedness::Unknown => {
                            ConstraintOperandSignedness::UntypedLiteral
                        }
                        crate::constraints::Signedness::NotNumeric => {
                            ConstraintOperandSignedness::Unsigned
                        }
                    })
                })
                .unwrap_or(ConstraintOperandSignedness::Unsigned),
            ExprKind::Field { .. } => expr_field_path(e, target_root)
                .and_then(|f| field_info.get(&f))
                .map(|f| {
                    if f.signed {
                        ConstraintOperandSignedness::Signed
                    } else {
                        ConstraintOperandSignedness::Unsigned
                    }
                })
                .unwrap_or(ConstraintOperandSignedness::Unsigned),
            ExprKind::Index { target, .. } => {
                list_field_name_from_expr(target, field_info, target_root)
                    .and_then(|f| field_info.get(&f))
                    .and_then(|f| f.list.as_ref())
                    .map(|l| {
                        if l.elem_signed {
                            ConstraintOperandSignedness::Signed
                        } else {
                            ConstraintOperandSignedness::Unsigned
                        }
                    })
                    .unwrap_or(ConstraintOperandSignedness::Unsigned)
            }
            ExprKind::Call { .. } => {
                if sum_call_elem_signed(e, field_info, target_root) {
                    ConstraintOperandSignedness::Signed
                } else {
                    ConstraintOperandSignedness::Unsigned
                }
            }
            ExprKind::Paren(inner) => recur(inner),
            ExprKind::Unary {
                op: UnaryOp::Neg,
                expr,
            } => match recur(expr) {
                ConstraintOperandSignedness::Unsigned => ConstraintOperandSignedness::Unsigned,
                _ => ConstraintOperandSignedness::Signed,
            },
            ExprKind::Unary { expr, .. } => recur(expr),
            ExprKind::Cast { ty, .. } => match ty {
                TypeExpr::Builtin {
                    name: BuiltinTy::SInt | BuiltinTy::SIntCap,
                    ..
                } => ConstraintOperandSignedness::Signed,
                _ => ConstraintOperandSignedness::Unsigned,
            },
            ExprKind::Int(_) => ConstraintOperandSignedness::UntypedLiteral,
            ExprKind::Binary { op, lhs, rhs } => {
                if matches!(op, BinaryOp::Shl | BinaryOp::Shr) {
                    recur(lhs)
                } else if self.constraint_binary_is_signed(lhs, rhs, field_info, target_root) {
                    ConstraintOperandSignedness::Signed
                } else if recur(lhs) == ConstraintOperandSignedness::UntypedLiteral
                    && recur(rhs) == ConstraintOperandSignedness::UntypedLiteral
                {
                    ConstraintOperandSignedness::UntypedLiteral
                } else {
                    ConstraintOperandSignedness::Unsigned
                }
            }
            ExprKind::Membership { expr, .. } => recur(expr),
            _ => ConstraintOperandSignedness::Unsigned,
        }
    }

    fn constraint_binary_is_signed(
        &self,
        lhs: &Expr,
        rhs: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        target_root: Option<&str>,
    ) -> bool {
        let l = self.constraint_expr_signedness(lhs, field_info, target_root);
        let r = self.constraint_expr_signedness(rhs, field_info, target_root);
        crate::numeric_semantics::solver_binary_is_signed(
            l,
            r,
            self.constraint_expr_native_width(lhs, field_info, target_root),
            self.constraint_expr_native_width(rhs, field_info, target_root),
        )
    }

    fn constraint_expr_native_width(
        &self,
        e: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        target_root: Option<&str>,
    ) -> Option<u32> {
        if let Some(width) = self.constraint_expr_width(e, field_info, target_root) {
            return Some(width);
        }
        match &*e.kind {
            ExprKind::Paren(inner) | ExprKind::Unary { expr: inner, .. } => {
                self.constraint_expr_native_width(inner, field_info, target_root)
            }
            ExprKind::Binary {
                op: BinaryOp::Shl | BinaryOp::Shr,
                lhs,
                ..
            } => self.constraint_expr_native_width(lhs, field_info, target_root),
            ExprKind::Binary { lhs, rhs, .. } => Some(
                self.constraint_expr_native_width(lhs, field_info, target_root)?
                    .max(self.constraint_expr_native_width(rhs, field_info, target_root)?),
            ),
            _ => None,
        }
    }

    /// A resolvable target field that is not a flag cannot be a whole
    /// constraint. Its own arm cannot express this — `Ident` and `Field`
    /// match earlier in `emit_constraint_bool_expr_w` than any guard could
    /// run — so both arms call this in their non-flag branch.
    fn reject_non_flag_field(&mut self, name: &str) {
        self.errors.push(format!(
            "`{name}` is a value, not a condition, so it cannot be a constraint on its own \
             — write a comparison such as `{name} != 0`. Only a one-bit UNSIGNED field \
             reads as a flag on its own; a wider, signed or enum-typed field does not"
        ));
        write!(self.out, "_ctx.bool_val(true)").ok();
    }

    /// A whole constraint, as passed to `_s.add(...)`.
    ///
    /// This is a BOOL position and must route to the Bool emitter. It used
    /// to call `emit_constraint_expr_w` — the value emitter — so a
    /// constraint that is not a proposition was emitted as a bitvector and
    /// handed to `z3::solver::add`, which takes a Bool: it built cleanly
    /// and died as a sort error at runtime (harc#560). All four callers
    /// write `_s.add(` immediately before this, so there is no value-
    /// position caller to preserve.
    ///
    /// Beyond the diagnostics this now reaches, it fixes a bare one-bit
    /// field: `keep flag` emitted `_s.add(_z_flag)` and now emits
    /// `_s.add(_z_flag != 0)`, the coercion the Bool emitter already
    /// applied everywhere the same field appeared under `&&` or `||`.
    fn emit_constraint_expr_with_list_bounds(
        &mut self,
        e: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        width: u32,
        target_root: Option<&str>,
        blocking: bool,
        list_unroll_bounds: &std::collections::HashMap<String, usize>,
    ) {
        self.emit_constraint_bool_expr_w(
            e,
            field_info,
            width,
            target_root,
            blocking,
            list_unroll_bounds,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_foreach_constraint_clauses(
        &mut self,
        ty: &str,
        var: &Ident,
        iter: &Expr,
        body: &[Expr],
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        target_root: Option<&str>,
        blocking: bool,
        list_unroll_bounds: &std::collections::HashMap<String, usize>,
        solver_width: u32,
        depth: usize,
    ) {
        let Some(field) = list_field_name_from_expr(iter, field_info, target_root) else {
            self.errors.push(format!(
                "randomize({ty}) foreach constraint must iterate a list field of transaction `{ty}`"
            ));
            return;
        };
        let Some(max_len) = list_unroll_bounds.get(&field).copied() else {
            self.errors.push(format!(
                "randomize({ty}) foreach constraint over `{field}` needs a bounded length constraint such as `{field}.len() <= N`"
            ));
            return;
        };
        let c_field = c_ident(&field);
        for i in 0..max_len {
            let item = Expr::new(
                ExprKind::Index {
                    target: iter.clone(),
                    index: Expr::new(ExprKind::Int(i.to_string()), var.span),
                },
                iter.span,
            );
            let mut subst = std::collections::HashMap::new();
            subst.insert(var.name.clone(), item);
            for clause in body {
                let lowered = substitute_idents(clause, &subst);
                self.pad(depth);
                write!(
                    self.out,
                    "_s.add(z3::ule(_z_{}_len, _ctx.bv_val((uint64_t){i}, {})) || (",
                    c_field, solver_width
                )
                .ok();
                self.emit_constraint_expr_with_list_bounds(
                    &lowered,
                    field_info,
                    solver_width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
                writeln!(self.out, "));").ok();
            }
        }
    }

    fn try_emit_constraint_list_call(
        &mut self,
        e: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        width: u32,
        target_root: Option<&str>,
        list_unroll_bounds: &std::collections::HashMap<String, usize>,
    ) -> bool {
        if let Some(field) = list_len_call_field_name(e, field_info, target_root) {
            write!(self.out, "_z_{}_len", c_ident(&field)).ok();
            return true;
        }

        let ExprKind::Call { callee, args } = &*e.kind else {
            return false;
        };
        let ExprKind::Ident(name) = &*callee.kind else {
            return false;
        };
        if name.name != "sum" || args.len() != 1 {
            return false;
        }
        let CallArg::Expr(arg) = &args[0] else {
            return false;
        };

        let (field, lo, hi) = match &*arg.kind {
            ExprKind::Index { target, index } => {
                let Some(field) = list_field_name_from_expr(target, field_info, target_root) else {
                    return false;
                };
                match &*index.kind {
                    ExprKind::RangeLit { lo, hi } => (field, lo.as_ref(), hi.as_ref()),
                    _ => {
                        self.errors.push(
                            "`sum(list[index])` constraints require a range slice like `sum(items[0..items.len()])`".into(),
                        );
                        write!(self.out, "_ctx.bool_val(true)").ok();
                        return true;
                    }
                }
            }
            _ => {
                let Some(field) = list_field_name_from_expr(arg, field_info, target_root) else {
                    return false;
                };
                (field, None, None)
            }
        };
        self.emit_constraint_list_sum(
            &field,
            lo,
            hi,
            field_info,
            width,
            target_root,
            list_unroll_bounds,
        );
        true
    }

    fn emit_constraint_list_sum(
        &mut self,
        field: &str,
        lo: Option<&Expr>,
        hi: Option<&Expr>,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        width: u32,
        target_root: Option<&str>,
        list_unroll_bounds: &std::collections::HashMap<String, usize>,
    ) {
        let Some(_) = field_info.get(field).and_then(|f| f.list.as_ref()) else {
            write!(self.out, "_ctx.bv_val((uint64_t)0, {width})").ok();
            return;
        };
        let max_len = list_unroll_bounds.get(field).copied().unwrap_or(0);
        let c_field = c_ident(field);
        let lo_const = lo.and_then(const_usize_expr).unwrap_or(0);
        let hi_const = hi.and_then(const_usize_expr);
        let hi_is_len = hi
            .and_then(|h| list_len_call_field_name(h, field_info, target_root))
            .as_deref()
            == Some(field);
        write!(self.out, "(").ok();
        let mut emitted = false;
        for i in 0..max_len {
            if i < lo_const {
                continue;
            }
            if let Some(hi) = hi_const {
                if i >= hi {
                    continue;
                }
            }
            if emitted {
                write!(self.out, " + ").ok();
            }
            if hi_is_len || hi.is_none() {
                write!(
                    self.out,
                    "z3::ite(z3::ugt(_z_{}_len, _ctx.bv_val((uint64_t){i}, {})), _z_{}_{i}, _ctx.bv_val((uint64_t)0, {}))",
                    c_field, width, c_field, width
                )
                .ok();
            } else {
                write!(self.out, "_z_{}_{i}", c_field).ok();
            }
            emitted = true;
        }
        if !emitted {
            write!(self.out, "_ctx.bv_val((uint64_t)0, {width})").ok();
        }
        write!(self.out, ")").ok();
    }

    fn emit_constraint_bool_expr_w(
        &mut self,
        e: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        width: u32,
        target_root: Option<&str>,
        blocking: bool,
        list_unroll_bounds: &std::collections::HashMap<String, usize>,
    ) {
        match &*e.kind {
            ExprKind::Bool(b) => {
                write!(
                    self.out,
                    "_ctx.bool_val({})",
                    if *b { "true" } else { "false" }
                )
                .ok();
                return;
            }
            ExprKind::Ident(id) => {
                if let Some(f) = field_info.get(&id.name) {
                    if is_flag_field(f) {
                        write!(
                            self.out,
                            "_z_{} != _ctx.bv_val((uint64_t)0, {})",
                            c_ident(&id.name),
                            width
                        )
                        .ok();
                    } else {
                        self.reject_non_flag_field(&id.name);
                    }
                    return;
                }
                if let Some(constant) = self.consts.get(&id.name) {
                    if constant.is_bool {
                        write!(
                            self.out,
                            "_ctx.bool_val({})",
                            if constant.value != 0 { "true" } else { "false" }
                        )
                        .ok();
                    } else {
                        self.errors.push(format!(
                            "the constant `{}` is a value, not a condition, so it cannot be a constraint on its own",
                            id.name
                        ));
                        write!(self.out, "_ctx.bool_val(true)").ok();
                    }
                    return;
                }
            }
            ExprKind::Field { .. } => {
                if let Some(field) = expr_field_path(e, target_root) {
                    if let Some(f) = field_info.get(&field) {
                        if is_flag_field(f) {
                            write!(
                                self.out,
                                "_z_{} != _ctx.bv_val((uint64_t)0, {})",
                                c_ident(&field),
                                width
                            )
                            .ok();
                        } else {
                            self.reject_non_flag_field(&field);
                        }
                        return;
                    }
                }
            }
            ExprKind::Paren(inner) => {
                write!(self.out, "(").ok();
                self.emit_constraint_bool_expr_w(
                    inner,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
                write!(self.out, ")").ok();
                return;
            }
            ExprKind::Unary {
                op: UnaryOp::Not | UnaryOp::NotKw,
                expr,
            } => {
                write!(self.out, "!(").ok();
                self.emit_constraint_bool_expr_w(
                    expr,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
                write!(self.out, ")").ok();
                return;
            }
            ExprKind::Binary {
                op: BinaryOp::AndAnd | BinaryOp::AndKw,
                lhs,
                rhs,
            } => {
                self.emit_constraint_bool_expr_w(
                    lhs,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
                write!(self.out, " && ").ok();
                self.emit_constraint_bool_expr_w(
                    rhs,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
                return;
            }
            ExprKind::Binary {
                op: BinaryOp::OrOr | BinaryOp::OrKw,
                lhs,
                rhs,
            } => {
                self.emit_constraint_bool_expr_w(
                    lhs,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
                write!(self.out, " || ").ok();
                self.emit_constraint_bool_expr_w(
                    rhs,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
                return;
            }
            // A constraint has to BE a proposition. Everything above
            // produces a z3 Bool; the fall-through below produces whatever
            // the expression is, and for an arithmetic, bitwise, shift or
            // wrapping operator that is a BITVECTOR. `z3::solver::add`
            // takes a Bool, so those built cleanly and died as a sort
            // error at runtime (harc#560).
            //
            // `keep len & 3 == 0` is the case worth explaining rather than
            // just rejecting. HARC's precedence, like C++'s, binds `&`
            // LOOSER than `==`, so that parses as `len & (3 == 0)` — the
            // top operator is `&`, which is why this one rule catches it.
            // Emitting a paren to "fix" it would make codegen contradict
            // the parser and silently change what the program means, so
            // the diagnostic names the grouping and lets the author say
            // which one they meant.
            ExprKind::Binary { op, .. }
                if matches!(
                    op,
                    BinaryOp::Add
                        | BinaryOp::Sub
                        | BinaryOp::Mul
                        | BinaryOp::Div
                        | BinaryOp::Mod
                        | BinaryOp::AddWrap
                        | BinaryOp::SubWrap
                        | BinaryOp::MulWrap
                        | BinaryOp::BitAnd
                        | BinaryOp::BitOr
                        | BinaryOp::BitXor
                        | BinaryOp::Shl
                        | BinaryOp::Shr
                ) =>
            {
                let spelling = constraint_op_spelling(*op);
                let mut msg = format!(
                    "`{spelling}` produces a value, not a condition, so it cannot be a \
                     constraint on its own — a `keep` (or a `randomize ... with` line) \
                     must be a comparison"
                );
                if matches!(op, BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor) {
                    // Says "the comparison operators" rather than naming
                    // `==`: `a & b != c` and `a & b < c` reach here too, and
                    // quoting an operator the author did not write reads as
                    // a diagnostic about someone else's program. `==` stays
                    // as the illustration only.
                    msg.push_str(&format!(
                        ". Note `{spelling}` binds LOOSER than the comparison operators, so \
                         `a {spelling} b == c` groups as `a {spelling} (b == c)`; write \
                         `(a {spelling} b) == c` if that is what you meant. For a logical \
                         and/or, use `&&` / `||`"
                    ));
                }
                self.errors.push(msg);
                write!(self.out, "_ctx.bool_val(true)").ok();
                return;
            }
            ExprKind::Unary {
                op: op @ (UnaryOp::Neg | UnaryOp::BitNot),
                ..
            } => {
                let spelling = if matches!(op, UnaryOp::Neg) { "-" } else { "~" };
                self.errors.push(format!(
                    "`{spelling}` produces a value, not a condition, so it cannot be a \
                     constraint on its own — compare it, as in `{spelling}x == 0`"
                ));
                write!(self.out, "_ctx.bool_val(true)").ok();
                return;
            }
            ExprKind::Int(lit) => {
                self.errors.push(format!(
                    "the literal `{lit}` is a value, not a condition, so it cannot be a \
                     constraint — write a comparison such as `x == {lit}`, or `true` / \
                     `false` for a constant one"
                ));
                write!(self.out, "_ctx.bool_val(true)").ok();
                return;
            }
            // A bare field reference is a value too, and only ONE width of
            // it is a condition: the arms above coerce a one-bit unsigned
            // scalar to `!= 0`. Anything else — a wider field, a signed
            // one-bit field, an enum field, a bit-select, a bit-slice, a
            // list element — reached `z3::solver::add` as a bitvector and
            // died as the same runtime sort error the operator arms above
            // are about (harc#560). The coercion cannot simply be widened
            // to cover them: `keep len` on a `uint<8>` has no meaning that
            // is obviously `!= 0` rather than a typo for a comparison, so
            // it is rejected and named.
            //
            // The `Ident` and `Field` arms above do the rejecting for a
            // resolvable field, since they match first — see the
            // `reject_non_flag_field` calls there.
            ExprKind::Index { .. } | ExprKind::BitSlice { .. } => {
                self.errors.push(format!(
                    "a {} is a value, not a condition, so it cannot be a constraint on its \
                     own — compare it, as in `x[0] == 1`",
                    if matches!(&*e.kind, ExprKind::BitSlice { .. }) {
                        "bit-slice"
                    } else {
                        "bit-select or list element"
                    }
                ));
                write!(self.out, "_ctx.bool_val(true)").ok();
                return;
            }
            _ => {}
        }
        self.emit_constraint_expr_w(
            e,
            field_info,
            width,
            target_root,
            blocking,
            list_unroll_bounds,
        );
    }

    fn constraint_expr_is_bool(&self, e: &Expr) -> bool {
        match &*e.kind {
            ExprKind::Bool(_) => false,
            ExprKind::Ident(_) => false,
            ExprKind::Paren(inner) => self.constraint_expr_is_bool(inner),
            ExprKind::Unary { op: UnaryOp::Not | UnaryOp::NotKw, .. } => true,
            ExprKind::Binary { op, .. } => matches!(
                op,
                BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le
                    | BinaryOp::Gt | BinaryOp::Ge | BinaryOp::AndAnd | BinaryOp::AndKw
                    | BinaryOp::OrOr | BinaryOp::OrKw
            ),
            ExprKind::Membership { .. } => true,
            _ => false,
        }
    }

    fn constraint_expr_numeric_bool_value(&self, e: &Expr) -> Option<bool> {
        match &*e.kind {
            ExprKind::Paren(inner) => self.constraint_expr_numeric_bool_value(inner),
            ExprKind::Int(s) => match c_int_literal(s).as_str() {
                "0" => Some(false),
                "1" => Some(true),
                _ => None,
            },
            ExprKind::Ident(id) => self.consts.get(&id.name).and_then(|c| {
                if c.is_bool { None } else { (c.value == 0).then_some(false).or_else(|| (c.value == 1).then_some(true)) }
            }),
            _ => None,
        }
    }

    fn emit_constraint_equality_operand(
        &mut self,
        e: &Expr,
        bool_context: bool,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        width: u32,
        target_root: Option<&str>,
        blocking: bool,
        list_unroll_bounds: &std::collections::HashMap<String, usize>,
    ) {
        if bool_context {
            write!(self.out, "(").ok();
            if self.constraint_expr_is_bool(e) {
                self.emit_constraint_bool_expr_w(
                    e, field_info, width, target_root, blocking, list_unroll_bounds,
                );
            } else if let Some(value) = self.constraint_expr_numeric_bool_value(e) {
                write!(self.out, "_ctx.bool_val({value})")
                .ok();
            } else {
                self.emit_constraint_bool_expr_w(
                    e, field_info, width, target_root, blocking, list_unroll_bounds,
                );
            }
            write!(self.out, ")").ok();
        } else {
            self.emit_constraint_expr_w(
                e, field_info, width, target_root, blocking, list_unroll_bounds,
            );
        }
    }

    fn emit_constraint_bit_slice_expr(
        &mut self,
        target: &Expr,
        hi: usize,
        lo: usize,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        width: u32,
        target_root: Option<&str>,
        blocking: bool,
        list_unroll_bounds: &std::collections::HashMap<String, usize>,
    ) {
        let slice_width = (hi - lo + 1) as u32;
        let mask = solver_unsigned_mask_expr(slice_width);
        write!(self.out, "(").ok();
        if lo == 0 {
            self.emit_constraint_expr_w(
                target,
                field_info,
                width,
                target_root,
                blocking,
                list_unroll_bounds,
            );
        } else {
            write!(self.out, "z3::lshr(").ok();
            self.emit_constraint_expr_w(
                target,
                field_info,
                width,
                target_root,
                blocking,
                list_unroll_bounds,
            );
            write!(self.out, ", _ctx.bv_val((uint64_t){lo}, {width}))").ok();
        }
        write!(self.out, " & harc_z3_bv_value(_ctx, {mask}, {width}))").ok();
    }

    /// Translate a HARC expression to a z3++ C++ expression. Field accesses
    /// `t.<name>` resolve to the per-field `_z_<name>` Z3 var declared in
    /// the surrounding solver block. Integer literals become `_ctx.bv_val(N, W)`
    /// at the field's width inferred from context. v0 is permissive — any
    /// untranslatable form falls back to a comment + `_ctx.bool_val(true)`.
    fn emit_constraint_expr_w(
        &mut self,
        e: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        width: u32,
        target_root: Option<&str>,
        blocking: bool,
        list_unroll_bounds: &std::collections::HashMap<String, usize>,
    ) {
        match &*e.kind {
            ExprKind::ForEachConstraint { .. } => {
                self.errors.push(
                    "foreach constraints are only supported as top-level `randomize ... with` clauses".into(),
                );
                write!(self.out, "_ctx.bool_val(true)").ok();
            }
            ExprKind::Call { .. } => {
                if self.try_emit_constraint_list_call(
                    e,
                    field_info,
                    width,
                    target_root,
                    list_unroll_bounds,
                ) {
                    return;
                }
                self.errors
                    .push("constraint function call not supported in v0 solver path".into());
                write!(self.out, "_ctx.bool_val(true)").ok();
            }
            ExprKind::Index { target, index } => {
                if let Some(field) = list_field_name_from_expr(target, field_info, target_root) {
                    if let Some(i) = const_usize_expr(index) {
                        if field_info
                            .get(&field)
                            .and_then(|f| f.list.as_ref())
                            .is_some()
                        {
                            if i < list_unroll_bounds.get(&field).copied().unwrap_or(0) {
                                write!(self.out, "_z_{}_{i}", c_ident(&field)).ok();
                                return;
                            }
                        }
                    }
                    self.errors.push(
                        "constraint list indexing requires a constant index within the list max length".into(),
                    );
                    write!(self.out, "_ctx.bool_val(true)").ok();
                    return;
                }
                if let Some(field) = expr_field_path(target, target_root) {
                    if let Some(info) = field_info.get(&field) {
                        if info.list.is_none() && info.width != 0 {
                            let Some(i) = const_usize_expr(index) else {
                                self.errors
                                    .push("constraint bit-select index must be constant".into());
                                write!(self.out, "_ctx.bool_val(true)").ok();
                                return;
                            };
                            if i as u32 >= info.width {
                                self.errors.push(format!(
                                    "constraint bit-select index {i} out of range for field `{field}`"
                                ));
                                write!(self.out, "_ctx.bool_val(true)").ok();
                                return;
                            }
                            self.emit_constraint_bit_slice_expr(
                                target,
                                i,
                                i,
                                field_info,
                                width,
                                target_root,
                                blocking,
                                list_unroll_bounds,
                            );
                            return;
                        }
                    }
                }
                self.errors
                    .push("constraint index expression not supported in v0 solver path".into());
                write!(self.out, "_ctx.bool_val(true)").ok();
            }
            ExprKind::BitSlice { target, hi, lo } => {
                let Some(hi) = const_usize_expr(hi) else {
                    self.errors
                        .push("constraint bit-slice high bound must be constant".into());
                    write!(self.out, "_ctx.bool_val(true)").ok();
                    return;
                };
                let Some(lo) = const_usize_expr(lo) else {
                    self.errors
                        .push("constraint bit-slice low bound must be constant".into());
                    write!(self.out, "_ctx.bool_val(true)").ok();
                    return;
                };
                if hi < lo {
                    self.errors
                        .push("constraint bit-slice high bound must be >= low bound".into());
                    write!(self.out, "_ctx.bool_val(true)").ok();
                    return;
                }
                self.emit_constraint_bit_slice_expr(
                    target,
                    hi,
                    lo,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
            }
            // `t.<name>` → _z_<name>. Strip the `t.` prefix.
            ExprKind::Field { .. } => {
                if let Some(field) = expr_field_path(e, target_root) {
                    if let Some(info) = field_info.get(&field) {
                        if info.list.is_some() {
                            self.errors.push(format!(
                                "constraint references list field `{}` as a scalar; use `{}.len()`, `{}[index]`, or `sum({}[0..{}.len()])`",
                                field, field, field, field, field
                            ));
                            write!(self.out, "_ctx.bool_val(true)").ok();
                        } else if info.width == 0 {
                            self.errors.push(format!(
                                "constraint references composite field `{}` as a scalar; nested record constraint flattening is not supported yet",
                                field
                            ));
                            write!(self.out, "_ctx.bool_val(true)").ok();
                        } else {
                            write!(self.out, "_z_{}", c_ident(&field)).ok();
                        }
                    } else if blocking
                        && target_root
                            .zip(expr_field_root(e).as_deref())
                            .is_some_and(|(target, root)| root != target)
                    {
                        if width <= 64 {
                            write!(self.out, "_ctx.bv_val((uint64_t)(").ok();
                            self.emit_expr(e);
                            write!(self.out, "), {width})").ok();
                        } else {
                            write!(self.out, "harc_z3_bv_value(_ctx, ").ok();
                            self.emit_expr(e);
                            write!(self.out, ", {width})").ok();
                        }
                    } else {
                        self.errors.push(format!(
                            "constraint references unknown field `{}` (only fields of the randomize target are supported)",
                            field
                        ));
                        write!(self.out, "_ctx.bool_val(true)").ok();
                    }
                } else if blocking {
                    if width <= 64 {
                        write!(self.out, "_ctx.bv_val((uint64_t)(").ok();
                        self.emit_expr(e);
                        write!(self.out, "), {width})").ok();
                    } else {
                        write!(self.out, "harc_z3_bv_value(_ctx, ").ok();
                        self.emit_expr(e);
                        write!(self.out, ", {width})").ok();
                    }
                } else {
                    self.errors.push(
                        "constraint references unsupported field path (only fields of the randomize target are supported)".into(),
                    );
                    write!(self.out, "_ctx.bool_val(true)").ok();
                }
            }
            ExprKind::Ident(id) => {
                // Bare ident: try fields first, then enum variants.
                // Lets a constraint like `keep op != WRAP` work — the
                // RHS isn't a field, it's the enum variant `WRAP`.
                if field_info.contains_key(&id.name) {
                    if field_info
                        .get(&id.name)
                        .and_then(|f| f.list.as_ref())
                        .is_some()
                    {
                        self.errors.push(format!(
                            "constraint references list field `{}` as a scalar; use `{}.len()`, `{}[index]`, or `sum({}[0..{}.len()])`",
                            id.name, id.name, id.name, id.name, id.name
                        ));
                        write!(self.out, "_ctx.bool_val(true)").ok();
                    } else if field_info.get(&id.name).is_some_and(|f| f.width == 0) {
                        self.errors.push(format!(
                            "constraint references composite field `{}` as a scalar; nested record constraint flattening is not supported yet",
                            id.name
                        ));
                        write!(self.out, "_ctx.bool_val(true)").ok();
                    } else {
                        write!(self.out, "_z_{}", c_ident(&id.name)).ok();
                    }
                } else if let Some(constant) = self.consts.get(&id.name).cloned() {
                    if width <= 64 {
                        write!(
                            self.out,
                            "_ctx.bv_val((uint64_t){}, {})",
                            c_int_literal(&constant.literal),
                            width
                        )
                        .ok();
                    } else {
                        write!(
                            self.out,
                            "harc_z3_bv_value(_ctx, {}, {})",
                            c_int_literal(&constant.literal),
                            width
                        )
                        .ok();
                    }
                } else if let Some(idx) = self.enum_variants.get(&id.name).copied() {
                    if width <= 64 {
                        write!(self.out, "_ctx.bv_val((uint64_t){}, {})", idx, width).ok();
                    } else {
                        write!(
                            self.out,
                            "harc_z3_bv_value(_ctx, (uint64_t){}, {})",
                            idx, width
                        )
                        .ok();
                    }
                } else if blocking && self.let_widths.contains_key(&id.name) {
                    if width <= 64 {
                        write!(self.out, "_ctx.bv_val((uint64_t)({}), {})", id.name, width).ok();
                    } else {
                        write!(self.out, "harc_z3_bv_value(_ctx, {}, {})", id.name, width).ok();
                    }
                } else {
                    self.errors
                        .push(format!("constraint references unknown name `{}`", id.name));
                    write!(self.out, "_ctx.bool_val(true)").ok();
                }
            }
            ExprKind::Int(s) => {
                if width <= 64 {
                    write!(
                        self.out,
                        "_ctx.bv_val((uint64_t){}, {})",
                        c_int_literal(s),
                        width
                    )
                    .ok();
                } else {
                    write!(
                        self.out,
                        "harc_z3_bv_value(_ctx, {}, {})",
                        c_solver_int_literal(s),
                        width
                    )
                    .ok();
                }
            }
            ExprKind::Bool(b) => {
                write!(
                    self.out,
                    "_ctx.bv_val((uint64_t){}, {})",
                    if *b { 1 } else { 0 },
                    width
                )
                .ok();
            }
            ExprKind::Paren(inner) => {
                write!(self.out, "(").ok();
                self.emit_constraint_expr_w(
                    inner,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
                write!(self.out, ")").ok();
            }
            ExprKind::Unary { op, expr } => {
                let s = match op {
                    UnaryOp::Neg => "-",
                    UnaryOp::Not | UnaryOp::NotKw => "!",
                    UnaryOp::BitNot => "~",
                };
                if matches!(op, UnaryOp::Not | UnaryOp::NotKw) {
                    write!(self.out, "!(").ok();
                    self.emit_constraint_bool_expr_w(
                        expr,
                        field_info,
                        width,
                        target_root,
                        blocking,
                        list_unroll_bounds,
                    );
                    write!(self.out, ")").ok();
                } else {
                    write!(self.out, "{s}").ok();
                    self.emit_constraint_expr_w(
                        expr,
                        field_info,
                        width,
                        target_root,
                        blocking,
                        list_unroll_bounds,
                    );
                }
            }
            // `e in <range-or-set>` parses as ExprKind::Membership
            // (not Binary(In, …) — the `in` keyword is lexed as
            // TokenKind::In and lowered structurally). Expand to an
            // OR-chain of equality / range-comparison sub-expressions
            // the solver handles natively.
            ExprKind::Membership { expr, set } => {
                self.emit_constraint_membership(
                    expr,
                    set,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
            }
            ExprKind::Binary { op, lhs, rhs } => {
                use BinaryOp::*;
                // Defensive: BinaryOp::In/Inside isn't produced by the
                // current parser (Membership is used instead) but the
                // op variants exist, so keep the handler in case
                // future parser paths emit them.
                if matches!(op, In | Inside) {
                    self.emit_constraint_membership(
                        lhs,
                        rhs,
                        field_info,
                        width,
                        target_root,
                        blocking,
                        list_unroll_bounds,
                    );
                    return;
                }
                // Z3's operator overloads on `z3::expr` default to
                // *signed* semantics for `<`, `>`, `<=`, `>=`, `/`,
                // `%`. HARC fields are unsigned (uint<N> / bits<N>),
                // so we explicitly route those through the
                // unsigned-named functions: ult/ule/ugt/uge for
                // comparisons, udiv for division, urem for modulus.
                // C++ has no `%%` infix — the old `" %% "` mapping was
                // a half-finished placeholder. Equality (`==`, `!=`),
                // logical / bitwise use the natural overloads. Z3's C++ API
                // exposes bit-vector shifts as shl/lshr/ashr functions;
                // `operator<<`/`operator>>` are not bit-vector overloads and
                // can therefore fall through implicit `ast::operator bool()`
                // conversions before C++ applies the native integer shift.
                let signed = self.constraint_binary_is_signed(lhs, rhs, field_info, target_root);
                // Shift signedness follows the value being shifted. A signed
                // shift count must not turn an unsigned left operand's
                // logical right shift into an arithmetic one.
                let lhs_signed = self.constraint_expr_signedness(lhs, field_info, target_root)
                    == ConstraintOperandSignedness::Signed;
                // `+% -% *%` mask the result to `max(W(lhs), W(rhs))`
                // (spec §2.4). The solver variable is a 64-bit bitvector
                // with the field's width carried as a separate range
                // assumption, so the wrap does NOT fall out of the
                // bitvector semantics — it has to be applied explicitly.
                // Without it `keep len +% 10 == 5` on a `uint<8>` field
                // was solved as `len + 10 == 5 && len < 256`, reported
                // unsatisfiable where `len = 251` is a solution.
                let wrap_mask = match op {
                    AddWrap | SubWrap | MulWrap => {
                        let spelling = match op {
                            SubWrap => "-%",
                            MulWrap => "*%",
                            _ => "+%",
                        };
                        match self.constraint_expr_width(e, field_info, target_root) {
                            // A wrap exactly as wide as the solver bitvector
                            // needs no mask. That bitvector is `solver_width`
                            // — `max(field widths).max(64)`, so it is >= 64,
                            // NOT always 64: a transaction with any field
                            // wider than 64 bits solves every constraint at
                            // that wider rank, and a 32-bit wrap there still
                            // needs its mask.
                            Some(w) if w == width => None,
                            // Wider than the solver bitvector: the residue is
                            // not representable, so there is no mask to
                            // apply. Rejected rather than silently emitted
                            // unmasked. Reachable two ways — a bit-slice
                            // (`emit_constraint_bit_slice_expr` does not
                            // bound `hi` by the field width, unlike the
                            // bit-select path) and a sized literal that
                            // declares more bits than the solver has
                            // (`128'h1` where every field fits in 64).
                            Some(w) => {
                                if w > width {
                                    self.errors.push(format!(
                                        "wrapping operator `{spelling}` in a constraint has a \
                                         {w}-bit result, wider than the {width}-bit solver \
                                         bitvector, so the §2.4 mask is not representable"
                                    ));
                                    None
                                } else {
                                    Some(solver_unsigned_mask_expr(w))
                                }
                            }
                            None => {
                                self.errors.push(format!(
                                    "wrapping operator `{spelling}` in a constraint needs both \
                                     operands to have a statically known bit-width so the §2.4 \
                                     mask is defined; use a transaction field, a `const`, an \
                                     enum variant, a sized literal (`8'hAB`) or a plain integer \
                                     literal. `sum(...)` and `.len()` have no declared width \
                                     and are not accepted here"
                                ));
                                None
                            }
                        }
                    }
                    _ => None,
                };
                if wrap_mask.is_some() {
                    // TWO parens: the inner one groups the arithmetic, the
                    // outer one guards the whole masked value. C++ binds
                    // `&` looser than `==`, so `(a + b) & mask == 5` would
                    // parse as `(a + b) & (mask == 5)`.
                    write!(self.out, "((").ok();
                }
                let (sep, fname) = match op {
                    Add | AddWrap => (" + ", None),
                    Sub | SubWrap => (" - ", None),
                    Mul | MulWrap => (" * ", None),
                    Div if signed => (" / ", None),
                    Div => ("", Some("udiv")),
                    // z3::expr `%` is bvsmod (sign follows the divisor),
                    // while HARC signed `%` truncates toward zero. Use the
                    // explicit bvsrem helper to preserve the dividend sign.
                    Mod if signed => ("", Some("srem")),
                    Mod => ("", Some("urem")),
                    Eq => (" == ", None),
                    Ne => (" != ", None),
                    Lt if signed => (" < ", None),
                    Lt => ("", Some("ult")),
                    Le if signed => (" <= ", None),
                    Le => ("", Some("ule")),
                    Gt if signed => (" > ", None),
                    Gt => ("", Some("ugt")),
                    Ge if signed => (" >= ", None),
                    Ge => ("", Some("uge")),
                    AndAnd | AndKw => {
                        self.emit_constraint_bool_expr_w(
                            lhs,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                        write!(self.out, " && ").ok();
                        self.emit_constraint_bool_expr_w(
                            rhs,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                        return;
                    }
                    OrOr | OrKw => {
                        self.emit_constraint_bool_expr_w(
                            lhs,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                        write!(self.out, " || ").ok();
                        self.emit_constraint_bool_expr_w(
                            rhs,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                        return;
                    }
                    BitAnd => (" & ", None),
                    BitOr => (" | ", None),
                    BitXor => (" ^ ", None),
                    Shl => ("", Some("shl")),
                    Shr if lhs_signed => ("", Some("ashr")),
                    Shr => ("", Some("lshr")),
                    _ => {
                        self.errors.push(format!(
                            "constraint operator `{:?}` not supported in v0 solver path",
                            op
                        ));
                        write!(self.out, "_ctx.bool_val(true)").ok();
                        return;
                    }
                };
                if let Some(fn_name) = fname {
                    // Unsigned op via z3::<fn>(lhs, rhs).
                    write!(self.out, "z3::{fn_name}(").ok();
                    self.emit_constraint_expr_w(
                        lhs,
                        field_info,
                        width,
                        target_root,
                        blocking,
                        list_unroll_bounds,
                    );
                    write!(self.out, ", ").ok();
                    self.emit_constraint_expr_w(
                        rhs,
                        field_info,
                        width,
                        target_root,
                        blocking,
                        list_unroll_bounds,
                    );
                    write!(self.out, ")").ok();
                } else {
                    let bool_context = matches!(op, Eq | Ne)
                        && (self.constraint_expr_is_bool(lhs)
                            || self.constraint_expr_is_bool(rhs));
                    self.emit_constraint_equality_operand(
                        lhs, bool_context, field_info, width, target_root, blocking,
                        list_unroll_bounds,
                    );
                    write!(self.out, "{sep}").ok();
                    self.emit_constraint_equality_operand(
                        rhs, bool_context, field_info, width, target_root, blocking,
                        list_unroll_bounds,
                    );
                }
                if let Some(mask) = wrap_mask {
                    // `bvand` with the low-W mask, at the solver's width.
                    // `harc_z3_bv_value` (not `bv_val`) because at a solver
                    // width above 64 the mask does not fit a `uint64_t` —
                    // same pairing the bit-slice emitter already uses.
                    write!(self.out, ") & harc_z3_bv_value(_ctx, {mask}, {width}))").ok();
                }
            }
            _ => {
                self.errors.push(format!(
                    "constraint expression not supported in v0 solver path"
                ));
                write!(self.out, "_ctx.bool_val(true)").ok();
            }
        }
    }

    /// Translate `<lhs> in <rhs>` (and `inside`) for the Z3 constraint
    /// path. Rhs shapes:
    ///   * `[lo..hi]` (RangeLit) → `lhs >= lo && lhs <= hi`
    ///   * `{a, b, c}` (SetLit)  → `lhs == a || lhs == b || lhs == c`
    ///   * Anything else         → falls back to `lhs == rhs`
    ///                              (treats rhs as a singleton).
    ///
    /// Open and unbounded sides on RangeLit (`[..hi]`, `[lo..]`)
    /// collapse the missing comparison. Set elements that are
    /// themselves RangeLits recurse (so `{[0..3], 7}` expands to
    /// `(_z>=0 && _z<=3) || _z==7`).
    fn emit_constraint_membership(
        &mut self,
        lhs: &Expr,
        rhs: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        width: u32,
        target_root: Option<&str>,
        blocking: bool,
        list_unroll_bounds: &std::collections::HashMap<String, usize>,
    ) {
        match &*rhs.kind {
            ExprKind::RangeLit { lo, hi } => {
                write!(self.out, "(").ok();
                let mut has_any = false;
                let signed = self.constraint_expr_signedness(lhs, field_info, target_root)
                    == ConstraintOperandSignedness::Signed;
                if let Some(l) = lo {
                    if signed {
                        self.emit_constraint_expr_w(
                            lhs,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                        write!(self.out, " >= ").ok();
                        self.emit_constraint_expr_w(
                            l,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                    } else {
                        write!(self.out, "z3::uge(").ok();
                        self.emit_constraint_expr_w(
                            lhs,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                        write!(self.out, ", ").ok();
                        self.emit_constraint_expr_w(
                            l,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                        write!(self.out, ")").ok();
                    }
                    has_any = true;
                }
                if let Some(h) = hi {
                    if has_any {
                        write!(self.out, " && ").ok();
                    }
                    if signed {
                        self.emit_constraint_expr_w(
                            lhs,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                        write!(self.out, " <= ").ok();
                        self.emit_constraint_expr_w(
                            h,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                    } else {
                        write!(self.out, "z3::ule(").ok();
                        self.emit_constraint_expr_w(
                            lhs,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                        write!(self.out, ", ").ok();
                        self.emit_constraint_expr_w(
                            h,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                        write!(self.out, ")").ok();
                    }
                    has_any = true;
                }
                if !has_any {
                    // Open-ended on both sides — vacuously true.
                    write!(self.out, "_ctx.bool_val(true)").ok();
                }
                write!(self.out, ")").ok();
            }
            ExprKind::SetLit(items) => {
                write!(self.out, "(").ok();
                if items.is_empty() {
                    write!(self.out, "_ctx.bool_val(false)").ok();
                } else {
                    for (i, it) in items.iter().enumerate() {
                        if i > 0 {
                            write!(self.out, " || ").ok();
                        }
                        // Recurse so `{[0..3], 7}` expands correctly.
                        match &*it.kind {
                            ExprKind::RangeLit { .. } | ExprKind::SetLit(_) => {
                                self.emit_constraint_membership(
                                    lhs,
                                    it,
                                    field_info,
                                    width,
                                    target_root,
                                    blocking,
                                    list_unroll_bounds,
                                );
                            }
                            ExprKind::Paren(inner) => {
                                self.emit_constraint_membership(
                                    lhs,
                                    inner,
                                    field_info,
                                    width,
                                    target_root,
                                    blocking,
                                    list_unroll_bounds,
                                );
                            }
                            _ => {
                                // Singleton element — equality test.
                                self.emit_constraint_expr_w(
                                    lhs,
                                    field_info,
                                    width,
                                    target_root,
                                    blocking,
                                    list_unroll_bounds,
                                );
                                write!(self.out, " == ").ok();
                                self.emit_constraint_expr_w(
                                    it,
                                    field_info,
                                    width,
                                    target_root,
                                    blocking,
                                    list_unroll_bounds,
                                );
                            }
                        }
                    }
                }
                write!(self.out, ")").ok();
            }
            ExprKind::Paren(inner) => {
                self.emit_constraint_membership(
                    lhs,
                    inner,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
            }
            _ => {
                // Fallback: treat rhs as a singleton — `a in b` becomes
                // `a == b`. Same shape `emit_bin_membership` uses.
                //
                // Parenthesised, unlike the original: HARC puts `in` at
                // RELATIONAL precedence, tighter than `==`, but the `==`
                // this lowers to is looser than a relational operator in
                // C++. So `a in b < c`, which HARC grouped as
                // `(a in b) < c`, re-parsed as `a == (b < c)`. The sibling
                // range and set arms already wrap; this arm did not
                // (harc#560 review). Unlike the bitwise case, this IS a
                // codegen grouping bug, because the operator being emitted
                // is not the operator that was written.
                write!(self.out, "(").ok();
                self.emit_constraint_expr_w(
                    lhs,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
                write!(self.out, " == ").ok();
                self.emit_constraint_expr_w(
                    rhs,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
                write!(self.out, ")").ok();
            }
        }
    }
}
