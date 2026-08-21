//! Covergroup struct + auto-sampler emission for the TB-IR backend.
//!
//! Byte-mirrors v1's `emit_covergroup_struct` / clock-sample
//! registration (cpp_tb.rs) for the schema subset that lowering
//! accepts: clock-triggered groups, value-set/range bins, declared
//! `cross` items, and the implicit pairwise auto-crosses. The
//! `report()` text and the per-cycle sample counts are
//! runtime-observable (log lines + check-phase asserts), so both must
//! match v1 exactly for the trace-equivalence gate.

use crate::codegen::cpp_tb::EmitError;
use crate::ir::{
    BinOp, CallTarget, CovBinBound, CovBinValue, CoverPointSchema, CovgroupSchema, Expr,
    FunctionId, IrType, PortRef, TbProgram, TransactorMethodSchema, TransactorSchema, UnOp,
    WidthCastKind,
};
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;

const INDENT: &str = "    ";
/// Mirrors v1's `COVERGROUP_AUTO_CROSS_BIN_CAP`.
const AUTO_CROSS_BIN_CAP: usize = 64;
/// Mirrors v1's `COVERGROUP_CROSS_MISSING_DETAIL_LIMIT`.
const CROSS_MISSING_DETAIL_LIMIT: usize = 16;

/// Pairwise auto-crosses: indices into `schema.points` of binned-point
/// pairs whose bin product fits the cap. A pair the user crossed
/// explicitly is excluded — same rule as v1's `covergroup_auto_crosses`
/// (only declared crosses of exactly two points suppress an auto pair).
fn auto_cross_pairs(schema: &CovgroupSchema) -> Vec<(usize, usize)> {
    let declared_pairs: BTreeSet<(&str, &str)> = schema
        .crosses
        .iter()
        .filter(|c| c.point_indices.len() == 2)
        .map(|c| {
            let mut names = [
                schema.points[c.point_indices[0]].name.as_str(),
                schema.points[c.point_indices[1]].name.as_str(),
            ];
            names.sort();
            (names[0], names[1])
        })
        .collect();
    let binned: Vec<usize> = schema
        .points
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.bins.is_empty())
        .map(|(i, _)| i)
        .collect();
    let mut pairs = Vec::new();
    for i in 0..binned.len() {
        for j in (i + 1)..binned.len() {
            let (a, b) = (binned[i], binned[j]);
            let mut names = [
                schema.points[a].name.as_str(),
                schema.points[b].name.as_str(),
            ];
            names.sort();
            if declared_pairs.contains(&(names[0], names[1])) {
                continue;
            }
            if schema.points[a].bins.len() * schema.points[b].bins.len() <= AUTO_CROSS_BIN_CAP {
                pairs.push((a, b));
            }
        }
    }
    pairs
}

/// One declared cross, resolved for emission. Storage/label mirror
/// v1's `DeclaredCoverCross` (`_cross_<item_idx>_<p1>__<p2>` flat
/// array, "p1 x p2" label).
struct DeclaredCross<'a> {
    storage: String,
    label: String,
    points: Vec<&'a CoverPointSchema>,
    total_bins: usize,
}

fn declared_crosses(schema: &CovgroupSchema) -> Vec<DeclaredCross<'_>> {
    schema
        .crosses
        .iter()
        .map(|c| {
            let points: Vec<&CoverPointSchema> =
                c.point_indices.iter().map(|&i| &schema.points[i]).collect();
            let names: Vec<&str> = points.iter().map(|p| p.name.as_str()).collect();
            DeclaredCross {
                storage: format!("_cross_{}_{}", c.item_index, names.join("__")),
                label: names.join(" x "),
                total_bins: points.iter().map(|p| p.bins.len()).product(),
                points,
            }
        })
        .collect()
}

/// Row-major flat index expression over the per-point loop variables
/// `_i0.._iN` — v1's `declared_cross_index_expr`.
fn cross_index_expr(cross: &DeclaredCross<'_>) -> String {
    let mut expr = "_i0".to_string();
    for (idx, point) in cross.points.iter().enumerate().skip(1) {
        expr = format!("({expr} * {} + _i{idx})", point.bins.len());
    }
    expr
}

/// `(flat_index, "p1.bin x p2.bin")` for every bin combination, in
/// row-major order — v1's `declared_cross_bin_labels`.
fn cross_bin_labels(cross: &DeclaredCross<'_>) -> Vec<(usize, String)> {
    fn walk(
        cross: &DeclaredCross<'_>,
        depth: usize,
        index: usize,
        labels: &mut Vec<String>,
        out: &mut Vec<(usize, String)>,
    ) {
        if depth == cross.points.len() {
            out.push((index, labels.join(" x ")));
            return;
        }
        let point = cross.points[depth];
        for (bin_idx, bin) in point.bins.iter().enumerate() {
            labels.push(format!("{}.{}", point.name, bin.name));
            walk(
                cross,
                depth + 1,
                index * point.bins.len() + bin_idx,
                labels,
                out,
            );
            labels.pop();
        }
    }

    let mut out = Vec::new();
    walk(cross, 0, 0, &mut Vec::new(), &mut out);
    out
}

/// C++ boolean for "is `_v` in this bin?", mirroring v1's
/// `emit_bin_membership` semantics: `Eq` is `_v == x`, `Range` is the
/// inclusive `_v >= lo && _v <= hi` (open bounds drop their side; a
/// fully open range is `true`), members `||`-joined.
struct CoverWidths<'a> {
    lanes: &'a HashMap<String, u32>,
    ports: &'a HashMap<String, u32>,
    /// Late-resolved hook argument and record-field types, keyed as `arg`
    /// and `arg.field`. Clock-triggered covergroups pass an empty map.
    hook_types: &'a HashMap<String, IrType>,
}

fn cover_type_width(ty: &IrType) -> Option<u32> {
    match ty {
        // Widthless scalar types use the compiler's 64-bit host ABI.
        IrType::UInt(width) | IrType::SInt(width) => Some(width.unwrap_or(64)),
        IrType::Bool => Some(1),
        _ => None,
    }
}

fn cover_expr_width(e: &Expr, widths: &CoverWidths<'_>) -> Option<u32> {
    match e {
        Expr::Literal { ty, .. } => cover_type_width(ty),
        Expr::WideLiteral(words) => words
            .len()
            .checked_mul(32)
            .and_then(|width| u32::try_from(width).ok()),
        Expr::Port(port) => match &port.lane {
            Some(_) => cover_lane_width(widths.lanes, port),
            None => port
                .width
                .or_else(|| widths.ports.get(&port.port_path.join("_")).copied())
                .filter(|width| *width != u32::MAX),
        },
        Expr::CovHookArg { param } => widths.hook_types.get(param).and_then(cover_type_width),
        Expr::CovHookParam { param, field, .. } => widths
            .hook_types
            .get(&format!("{param}.{field}"))
            .and_then(cover_type_width),
        Expr::Unary(UnOp::Not, _) => Some(1),
        Expr::Unary(_, inner) => cover_expr_width(inner, widths),
        Expr::Binary(op, lhs, rhs) => match op {
            BinOp::Eq
            | BinOp::Ne
            | BinOp::Lt
            | BinOp::Le
            | BinOp::Gt
            | BinOp::Ge
            | BinOp::And
            | BinOp::Or => Some(1),
            // v1 evaluates narrow shifts in its 64-bit host carrier. Keep
            // that effective width in the expression metadata as well as in
            // emission so a later wide composition does not mask the shifted
            // value back to the LHS's declared width.
            BinOp::Shl | BinOp::Shr => cover_expr_width(lhs, widths).map(|width| width.max(64)),
            _ => match (cover_expr_width(lhs, widths), cover_expr_width(rhs, widths)) {
                (Some(lhs), Some(rhs)) => Some(lhs.max(rhs)),
                (width, None) | (None, width) => width,
            },
        },
        Expr::Ternary(_, then_expr, else_expr) => match (
            cover_expr_width(then_expr, widths),
            cover_expr_width(else_expr, widths),
        ) {
            (Some(then_width), Some(else_width)) => Some(then_width.max(else_width)),
            _ => None,
        },
        Expr::BitSlice { hi, lo, .. } => Some(hi - lo + 1),
        Expr::BitSliceDyn { .. } => Some(64),
        Expr::WidthCast { width, .. } => Some(*width),
        Expr::Call(CallTarget::Helper { ret, .. } | CallTarget::ExternFn { ret, .. }, _) => {
            cover_type_width(ret)
        }
        _ => None,
    }
}

fn cover_expr_signed(e: &Expr, widths: &CoverWidths<'_>) -> bool {
    match e {
        Expr::Literal { ty, .. } => matches!(ty, IrType::SInt(_)),
        Expr::Unary(UnOp::Not, _) => false,
        Expr::Unary(_, inner) => cover_expr_signed(inner, widths),
        Expr::Binary(op, lhs, rhs) => match op {
            BinOp::Eq
            | BinOp::Ne
            | BinOp::Lt
            | BinOp::Le
            | BinOp::Gt
            | BinOp::Ge
            | BinOp::And
            | BinOp::Or => false,
            BinOp::Shl | BinOp::Shr => cover_expr_signed(lhs, widths),
            _ => cover_expr_signed(lhs, widths) && cover_expr_signed(rhs, widths),
        },
        Expr::Ternary(_, then_expr, else_expr) => {
            cover_expr_signed(then_expr, widths) && cover_expr_signed(else_expr, widths)
        }
        Expr::WidthCast { kind, .. } => matches!(kind, WidthCastKind::Sext),
        Expr::Call(CallTarget::Helper { ret, .. } | CallTarget::ExternFn { ret, .. }, _) => {
            matches!(ret, IrType::SInt(_))
        }
        Expr::CovHookArg { param } => widths
            .hook_types
            .get(param)
            .is_some_and(|ty| matches!(ty, IrType::SInt(_))),
        Expr::CovHookParam { param, field, .. } => widths
            .hook_types
            .get(&format!("{param}.{field}"))
            .is_some_and(|ty| matches!(ty, IrType::SInt(_))),
        _ => false,
    }
}

fn has_unresolved_arch_width(e: &Expr, widths: &CoverWidths<'_>) -> bool {
    match e {
        Expr::Port(port) if port.lane.is_none() && port.width.is_none() => widths
            .ports
            .get(&port.port_path.join("_"))
            .is_some_and(|width| *width == u32::MAX),
        Expr::Unary(_, inner) => has_unresolved_arch_width(inner, widths),
        // Truncation, direction-agnostic resize (including `as uint<N>`),
        // and a fixed slice establish a concrete low-bit result without
        // needing the source's parameter-dependent packed width. Other casts
        // (especially sign extension) still need that width.
        Expr::WidthCast {
            kind: WidthCastKind::Trunc | WidthCastKind::Resize,
            ..
        }
        | Expr::BitSlice { .. } => false,
        // `as sint<N>` is represented as Sext with equal source/target
        // widths to preserve signedness without filling. A real `.sext<N>()`
        // over an unresolved port has no source width and must recurse.
        Expr::WidthCast {
            kind: WidthCastKind::Sext,
            width,
            src_width: Some(src_width),
            ..
        } if width == src_width => false,
        Expr::WidthCast { inner, .. } => has_unresolved_arch_width(inner, widths),
        Expr::Binary(_, lhs, rhs) => {
            has_unresolved_arch_width(lhs, widths) || has_unresolved_arch_width(rhs, widths)
        }
        Expr::Ternary(cond, then_expr, else_expr) => {
            has_unresolved_arch_width(cond, widths)
                || has_unresolved_arch_width(then_expr, widths)
                || has_unresolved_arch_width(else_expr, widths)
        }
        Expr::BitSliceDyn { target, hi, lo } => {
            has_unresolved_arch_width(target, widths)
                || has_unresolved_arch_width(hi, widths)
                || has_unresolved_arch_width(lo, widths)
        }
        Expr::CovHookParam {
            index: Some(index), ..
        } => has_unresolved_arch_width(index, widths),
        Expr::Call(_, args) => args
            .iter()
            .any(|arg| has_unresolved_arch_width(arg, widths)),
        _ => false,
    }
}

fn reject_wide_cpp_operand(
    what: &str,
    operands: &[&Expr],
    widths: &CoverWidths<'_>,
) -> Result<(), EmitError> {
    if operands
        .iter()
        .any(|operand| cover_expr_width(operand, widths).is_some_and(|width| width > 64))
    {
        return Err(EmitError(format!(
            "tbir: covergroup {what} uses a DUT value wider than 64 bits without type-directed operand coercion"
        )));
    }
    Ok(())
}

fn cover_coerce_cpp(rendered: String, src_width: Option<u32>, width: u32, signed: bool) -> String {
    if let Some(words) = super::wide_scalar_words(width) {
        if signed {
            if let Some(src_width) = src_width {
                return format!(
                    "harc_rt::harc_wide_sext<{words}>({rendered}, {src_width}, {width})"
                );
            }
        }
        return match src_width {
            Some(src_width) => {
                format!("harc_rt::harc_wide_zext<{words}>({rendered}, {src_width})")
            }
            None => format!("harc_rt::harc_wide_zext<{words}>({rendered})"),
        };
    }
    if width > 64 {
        if signed {
            if let Some(src_width) = src_width {
                return format!(
                    "harc_rt::harc_sext_u128((_harc_u128)({rendered}), {src_width}, {width})"
                );
            }
        }
        format!("((_harc_u128)({rendered}))")
    } else {
        format!("((uint64_t)({rendered}))")
    }
}

fn cover_bool_cpp(e: &Expr, widths: &CoverWidths<'_>) -> Result<String, EmitError> {
    let width = cover_expr_width(e, widths);
    let rendered = cover_expr_cpp(e, widths)?;
    Ok(match width {
        Some(width) if width > 128 => {
            let value =
                cover_coerce_cpp(rendered, Some(width), width, cover_expr_signed(e, widths));
            format!("(!harc_rt::harc_wide_is_zero({value}))")
        }
        Some(width) if width > 64 => format!("(((_harc_u128)({rendered})) != 0)"),
        _ => format!("(({rendered}) != 0)"),
    })
}

fn cover_common_width(lhs: Option<u32>, rhs: Option<u32>) -> Option<u32> {
    match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => Some(lhs.max(rhs)),
        (width, None) | (None, width) => width,
    }
}

fn cover_binary_cpp(
    op: BinOp,
    lhs: &Expr,
    rhs: &Expr,
    widths: &CoverWidths<'_>,
) -> Result<String, EmitError> {
    if has_unresolved_arch_width(lhs, widths) || has_unresolved_arch_width(rhs, widths) {
        return Err(EmitError(
            "tbir: covergroup composed expression uses a parameter-dependent ARCH DUT port width; use an elaborated interface with a concrete packed width"
                .to_string(),
        ));
    }
    if matches!(op, BinOp::And | BinOp::Or) {
        let lhs = cover_bool_cpp(lhs, widths)?;
        let rhs = cover_bool_cpp(rhs, widths)?;
        return Ok(format!("({lhs} {} {rhs})", cover_bin_op_cpp(op)));
    }

    let lhs_width = cover_expr_width(lhs, widths);
    let rhs_width = cover_expr_width(rhs, widths);
    let lhs_signed = cover_expr_signed(lhs, widths);
    let rhs_signed = cover_expr_signed(rhs, widths);
    let common_signed = lhs_signed && rhs_signed;
    let lhs_cpp = cover_expr_cpp(lhs, widths)?;
    let rhs_cpp = cover_expr_cpp(rhs, widths)?;

    if matches!(op, BinOp::Shl | BinOp::Shr) {
        // Clamp a full-width RHS to the operation's carrier width. Merely
        // casting a wide count to uint64_t would turn (1 << 100) into zero
        // and incorrectly perform an unshifted operation. v1 evaluates
        // <=64-bit cover shifts in its uint64_t/int64_t host carrier (so a
        // one-bit `en << 1` produces 2); wider values keep their HARC width.
        let lhs_width = lhs_width.unwrap_or(64);
        let shift_width = lhs_width.max(64);
        let shift_count = match rhs_width {
            Some(width) if width > 128 => {
                format!("harc_rt::harc_wide_shift_count(({rhs_cpp}), {shift_width})")
            }
            Some(width) if width > 64 => {
                format!("harc_rt::harc_u128_shift_count((_harc_u128)({rhs_cpp}), {shift_width})")
            }
            _ => format!("harc_rt::harc_u64_shift_count((uint64_t)({rhs_cpp}), {shift_width})"),
        };
        return Ok(match lhs_width {
            width if width > 128 => {
                let lhs = cover_coerce_cpp(lhs_cpp, Some(width), width, lhs_signed);
                let shifted = format!("(({lhs}) {} ({shift_count}))", cover_bin_op_cpp(op));
                if matches!(op, BinOp::Shr) && lhs_signed {
                    format!("harc_rt::harc_wide_ashr(({lhs}), {shift_count}, {width})")
                } else {
                    format!("harc_rt::harc_wide_mask_bits({shifted}, {width})")
                }
            }
            width if width > 64 => {
                let helper = if matches!(op, BinOp::Shl) {
                    "harc_shl_u128"
                } else if lhs_signed {
                    "harc_ashr_u128"
                } else {
                    "harc_shr_u128"
                };
                format!("harc_rt::{helper}((_harc_u128)({lhs_cpp}), {shift_count}, {width})")
            }
            _ => {
                let helper = if matches!(op, BinOp::Shl) {
                    "harc_shl_u128"
                } else if lhs_signed {
                    "harc_ashr_u128"
                } else {
                    "harc_shr_u128"
                };
                format!("((uint64_t)harc_rt::{helper}((_harc_u128)({lhs_cpp}), {shift_count}, 64))")
            }
        });
    }

    let common_width = cover_common_width(lhs_width, rhs_width);
    let (lhs_cpp, rhs_cpp) = match common_width {
        Some(width) if width > 64 => (
            cover_coerce_cpp(lhs_cpp, lhs_width, width, common_signed),
            cover_coerce_cpp(rhs_cpp, rhs_width, width, common_signed),
        ),
        _ => (lhs_cpp, rhs_cpp),
    };
    if common_signed {
        if let Some(width) = common_width.filter(|width| *width > 64) {
            let less = |lhs: &str, rhs: &str| {
                if width > 128 {
                    format!("harc_rt::harc_wide_slt({lhs}, {rhs}, {width})")
                } else {
                    format!("harc_rt::harc_slt_u128({lhs}, {rhs}, {width})")
                }
            };
            return Ok(match op {
                BinOp::Lt => less(&lhs_cpp, &rhs_cpp),
                BinOp::Le => format!("(!{})", less(&rhs_cpp, &lhs_cpp)),
                BinOp::Gt => less(&rhs_cpp, &lhs_cpp),
                BinOp::Ge => format!("(!{})", less(&lhs_cpp, &rhs_cpp)),
                BinOp::Div if width > 128 => {
                    format!("harc_rt::harc_wide_sdiv({lhs_cpp}, {rhs_cpp}, {width})")
                }
                BinOp::Mod if width > 128 => {
                    format!("harc_rt::harc_wide_smod({lhs_cpp}, {rhs_cpp}, {width})")
                }
                BinOp::Div => format!("harc_rt::harc_sdiv_u128({lhs_cpp}, {rhs_cpp}, {width})"),
                BinOp::Mod => format!("harc_rt::harc_smod_u128({lhs_cpp}, {rhs_cpp}, {width})"),
                _ => format!("({lhs_cpp} {} {rhs_cpp})", cover_bin_op_cpp(op)),
            });
        }
    }
    Ok(format!("({lhs_cpp} {} {rhs_cpp})", cover_bin_op_cpp(op)))
}

fn cover_unary_cpp(op: UnOp, inner: &Expr, widths: &CoverWidths<'_>) -> Result<String, EmitError> {
    if !matches!(op, UnOp::Not) && has_unresolved_arch_width(inner, widths) {
        return Err(EmitError(
            "tbir: covergroup unary expression uses a parameter-dependent ARCH DUT port width; use an elaborated interface with a concrete packed width"
                .to_string(),
        ));
    }
    if matches!(op, UnOp::Not) {
        return Ok(format!("!({})", cover_bool_cpp(inner, widths)?));
    }
    let width = cover_expr_width(inner, widths);
    let rendered = cover_expr_cpp(inner, widths)?;
    Ok(match (op, width) {
        (UnOp::Neg, Some(width)) if width > 128 => {
            let words = width.div_ceil(32);
            let value = cover_coerce_cpp(
                rendered,
                Some(width),
                width,
                cover_expr_signed(inner, widths),
            );
            format!(
                "harc_rt::harc_wide_mask_bits((harc_rt::HarcWide<{words}>{{}} - {value}), {width})"
            )
        }
        (UnOp::Neg, Some(width)) if width > 64 => {
            format!("harc_rt::harc_trunc_u128(-((_harc_u128)({rendered})), {width})")
        }
        (UnOp::BitNot, Some(width)) if width > 128 => {
            let value = cover_coerce_cpp(
                rendered,
                Some(width),
                width,
                cover_expr_signed(inner, widths),
            );
            format!("harc_rt::harc_wide_mask_bits(~({value}), {width})")
        }
        (UnOp::BitNot, Some(width)) if width > 64 => {
            format!("harc_rt::harc_trunc_u128(~((_harc_u128)({rendered})), {width})")
        }
        _ => format!("{}({rendered})", cover_un_op_cpp(op)),
    })
}

fn cover_ternary_cpp(
    cond: &Expr,
    then_expr: &Expr,
    else_expr: &Expr,
    widths: &CoverWidths<'_>,
) -> Result<String, EmitError> {
    if has_unresolved_arch_width(then_expr, widths) || has_unresolved_arch_width(else_expr, widths)
    {
        return Err(EmitError(
            "tbir: covergroup ternary branches use a parameter-dependent ARCH DUT port width; use an elaborated interface with a concrete packed width"
                .to_string(),
        ));
    }
    let cond = cover_bool_cpp(cond, widths)?;
    let then_width = cover_expr_width(then_expr, widths);
    let else_width = cover_expr_width(else_expr, widths);
    let signed = cover_expr_signed(then_expr, widths) && cover_expr_signed(else_expr, widths);
    let width = cover_common_width(then_width, else_width);
    let mut then_cpp = cover_expr_cpp(then_expr, widths)?;
    let mut else_cpp = cover_expr_cpp(else_expr, widths)?;
    if let Some(width) = width.filter(|width| *width > 64) {
        then_cpp = cover_coerce_cpp(then_cpp, then_width, width, signed);
        else_cpp = cover_coerce_cpp(else_cpp, else_width, width, signed);
    }
    Ok(format!("({cond} ? {then_cpp} : {else_cpp})"))
}

fn cover_expr_cpp(e: &Expr, widths: &CoverWidths<'_>) -> Result<String, EmitError> {
    Ok(match e {
        Expr::Literal { value, .. } => format!("{value}"),
        Expr::WideLiteral(words) => super::expr::wide_literal_cpp(words),
        Expr::Port(p) => {
            let sig = cover_port_signal(p);
            match &p.lane {
                None => format!("harc_rt::harc_read({sig})"),
                Some(lane) => {
                    let idx = match lane {
                        crate::ir::LaneIndex::Const(c) => c.to_string(),
                        crate::ir::LaneIndex::Var(index) => {
                            reject_wide_cpp_operand("runtime lane selector", &[index], widths)?;
                            cover_expr_cpp(index, widths)?
                        }
                    };
                    match cover_lane_width(widths.lanes, p) {
                        Some(w) => {
                            format!("harc_rt::harc_vec_lane_read<{w}>({sig}, (std::size_t)({idx}))")
                        }
                        None => format!("{sig}[{idx}]"),
                    }
                }
            }
        }
        Expr::Binary(op, a, b) => cover_binary_cpp(*op, a, b, widths)?,
        Expr::Unary(op, a) => cover_unary_cpp(*op, a, widths)?,
        Expr::Ternary(c, t, f) => cover_ternary_cpp(c, t, f, widths)?,
        Expr::BitSlice { target, hi, lo } => {
            let t = cover_expr_cpp(target, widths)?;
            let width = hi - lo + 1;
            if width <= 64 {
                format!("harc_rt::harc_bits(({t}), {hi}, {lo})")
            } else if width <= 128 {
                format!(
                    "static_cast<_harc_u128>(harc_rt::harc_wide_extract_bits<4>(({t}), {lo}, {width}))"
                )
            } else {
                let words = width.div_ceil(32);
                format!("harc_rt::harc_wide_extract_bits<{words}>(({t}), {lo}, {width})")
            }
        }
        Expr::BitSliceDyn { target, hi, lo } => {
            reject_wide_cpp_operand("runtime bit-slice selector", &[hi, lo], widths)?;
            let target = cover_expr_cpp(target, widths)?;
            let hi = cover_expr_cpp(hi, widths)?;
            let lo = cover_expr_cpp(lo, widths)?;
            format!("harc_rt::harc_bits(({target}), (uint32_t)({hi}), (uint32_t)({lo}))")
        }
        Expr::WidthCast {
            kind,
            width,
            src_width,
            inner,
        } => cover_width_cast_cpp(*kind, *width, *src_width, inner, widths)?,
        // Hook-param cover target (`cover t.burst` / `cover t.data[i]`):
        // `param` is the hook-sampler closure's by-value argument, so the
        // sample reads `param.field` directly — same shape as v1 emitting
        // the field access over its by-value param. Only reachable from
        // `hook_sampler_registration`, whose closure binds `param`.
        Expr::CovHookParam {
            param,
            field,
            index,
        } => match index {
            Some(idx) => {
                reject_wide_cpp_operand("hook-field lane selector", &[idx], widths)?;
                let i = cover_expr_cpp(idx, widths)?;
                format!("{param}.{field}[{i}]")
            }
            None => format!("{param}.{field}"),
        },
        Expr::CovHookArg { param } => param.clone(),
        Expr::Call(target, args) => {
            if args
                .iter()
                .any(|arg| has_unresolved_arch_width(arg, widths))
            {
                return Err(EmitError(
                    "tbir: covergroup call argument uses a parameter-dependent ARCH DUT port width; use an elaborated interface with a concrete packed width"
                        .to_string(),
                ));
            }
            let name = match target {
                // A pure helper's declared parameter types are retained and
                // checked during lowering/verification.  They provide the
                // exact C++ carrier context for wide values (`HarcWide<N>`),
                // so rejecting the argument here would discard information
                // the call already has.
                CallTarget::Helper { name, .. } => super::expr::helper_cpp_name(name),
                CallTarget::ExternFn { name, .. } => {
                    // FFI arguments do not yet carry parameter metadata in
                    // CallTarget, so retain the conservative wide gate.
                    reject_wide_cpp_operand(
                        "call argument",
                        &args.iter().collect::<Vec<_>>(),
                        widths,
                    )?;
                    name.clone()
                }
                other => {
                    return Err(EmitError(format!(
                        "tbir: covergroup target call is outside the sampler subset: {other:?}"
                    )))
                }
            };
            let mut rendered = Vec::with_capacity(args.len());
            for arg in args {
                rendered.push(cover_expr_cpp(arg, widths)?);
            }
            format!("{name}({})", rendered.join(", "))
        }
        other => {
            return Err(EmitError(format!(
                "tbir: covergroup target expression is outside the sampler subset: {other:?}"
            )));
        }
    })
}

fn cover_port_signal(p: &PortRef) -> String {
    format!("dut->{}", p.port_path.join("_"))
}

fn cover_lane_width(lanes: &HashMap<String, u32>, p: &PortRef) -> Option<u32> {
    lanes.get(&p.port_path.join("_")).copied()
}

fn cover_bin_op_cpp(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "&&",
        BinOp::Or => "||",
        BinOp::BitAnd => "&",
        BinOp::BitOr => "|",
        BinOp::BitXor => "^",
        BinOp::Shl => "<<",
        BinOp::Shr => ">>",
    }
}

fn cover_un_op_cpp(op: UnOp) -> &'static str {
    match op {
        UnOp::Neg => "-",
        UnOp::Not => "!",
        UnOp::BitNot => "~",
    }
}

fn cover_width_cast_cpp(
    kind: WidthCastKind,
    width: u32,
    src_width: Option<u32>,
    inner: &Expr,
    widths: &CoverWidths<'_>,
) -> Result<String, EmitError> {
    let e = cover_expr_cpp(inner, widths)?;
    if let Some(words) = super::wide_scalar_words(width) {
        return Ok(match kind {
            WidthCastKind::Trunc => {
                format!("harc_rt::harc_wide_trunc<{words}>({e}, {width})")
            }
            WidthCastKind::Zext => match src_width {
                Some(sw) => format!("harc_rt::harc_wide_zext<{words}>({e}, {sw})"),
                None => format!("harc_rt::harc_wide_zext<{words}>({e})"),
            },
            WidthCastKind::Sext => match src_width {
                Some(sw) if sw < width => {
                    format!("harc_rt::harc_wide_sext<{words}>({e}, {sw}, {width})")
                }
                Some(sw) => format!("harc_rt::harc_wide_trunc<{words}>({e}, {sw})"),
                None => format!("harc_rt::harc_wide_zext<{words}>({e})"),
            },
            WidthCastKind::Resize => match src_width {
                Some(sw) if width < sw => {
                    format!("harc_rt::harc_wide_trunc<{words}>({e}, {width})")
                }
                Some(sw) => format!("harc_rt::harc_wide_zext<{words}>({e}, {sw})"),
                None => format!("harc_rt::harc_wide_trunc<{words}>({e}, {width})"),
            },
        });
    }
    let c_unsigned = if width > 64 { "_harc_u128" } else { "uint64_t" };
    let mask = |w: u32| (1u64 << w) - 1;
    let trunc_shape = |e: &str| {
        if width > 64 {
            format!("harc_rt::harc_trunc_u128((_harc_u128)({e}), {width})")
        } else if width == 64 {
            format!("((uint64_t)({e}))")
        } else {
            format!("((uint64_t)(((uint64_t)({e}) & 0x{:X}ULL)))", mask(width))
        }
    };
    let plain_cast = |e: &str| format!("(({c_unsigned})({e}))");
    Ok(match kind {
        WidthCastKind::Trunc => trunc_shape(&e),
        WidthCastKind::Zext => plain_cast(&e),
        WidthCastKind::Sext => match src_width {
            Some(sw) if sw < width => {
                if width > 64 {
                    format!("harc_rt::harc_sext_u128((_harc_u128)({e}), {sw}, {width})")
                } else {
                    let shift = 64 - sw;
                    if width == 64 {
                        format!("((int64_t)(((int64_t)((uint64_t)({e}) << {shift})) >> {shift}))")
                    } else {
                        format!(
                            "((uint64_t)(((int64_t)((uint64_t)({e}) << {shift})) >> {shift}) & 0x{:X}ULL)",
                            mask(width)
                        )
                    }
                }
            }
            _ if width <= 64 => format!("((int64_t)((uint64_t)({e})))"),
            _ => plain_cast(&e),
        },
        WidthCastKind::Resize => match src_width {
            Some(sw) if width < sw => trunc_shape(&e),
            Some(_) => plain_cast(&e),
            None => trunc_shape(&e),
        },
    })
}

fn bin_membership(values: &[CovBinValue], widths: &CoverWidths<'_>) -> Result<String, EmitError> {
    if values.is_empty() {
        return Ok("(false)".to_string());
    }
    // Render one bin bound — an exact value or a range end, which are
    // the same thing here. A constant folds inline; a runtime bound is
    // emitted with the same expression lowerer used for point targets,
    // matching v1's per-sample `emit_expr(bound)`.
    //
    // NOT byte-for-byte, and deliberately so: `cover_expr_cpp`
    // parenthesises a compound expression, v1 does not. For an operator
    // that binds tighter than the comparison (`{dut.en + 4}` ->
    // `_v == harc_read(dut->en) + 4`) the two agree. For one that does
    // not, v1 is WRONG: `{dut.en | 8}` emits `_v == harc_read(dut->en) | 8`,
    // which C++ groups as `(_v == en) | 8` — non-zero always, so the bin
    // hits on every sample. TB-IR emits `_v == (harc_read(dut->en) | 8)`
    // and counts what the user wrote.
    //
    // So a low-precedence operator here is a place TB-IR is AHEAD, and
    // a program using one must not go in `tbir_equiv_fixtures.txt` — the
    // trace diff would fail on v1's bug. `cov_runtime_bin_value_test`
    // uses `+` for exactly that reason; the divergence itself is pinned
    // by `a_low_precedence_bin_value_is_a_place_v1_is_wrong`. The same
    // was already true of range bounds before exact values joined them.
    let bound = |b: &CovBinBound| -> Result<String, EmitError> {
        match b {
            CovBinBound::Const(x) => Ok(x.to_string()),
            CovBinBound::Runtime(e) => cover_expr_cpp(e, widths),
        }
    };
    let mut parts = Vec::with_capacity(values.len());
    for v in values {
        let part = match v {
            CovBinValue::Eq(x) => format!("(_v == {})", bound(x)?),
            CovBinValue::Range { lo, hi } => match (lo, hi) {
                (Some(l), Some(h)) => format!("(_v >= {} && _v <= {})", bound(l)?, bound(h)?),
                (Some(l), None) => format!("(_v >= {})", bound(l)?),
                (None, Some(h)) => format!("(_v <= {})", bound(h)?),
                (None, None) => "(true)".to_string(),
            },
        };
        parts.push(part);
    }
    Ok(format!("({})", parts.join(" || ")))
}

/// `struct <Name> { ... bin counters ... void report() const { ... } };`
pub(super) fn covgroup_struct(out: &mut String, schema: &CovgroupSchema) {
    let crosses = auto_cross_pairs(schema);
    let declared = declared_crosses(schema);
    writeln!(out, "struct {} {{", schema.name).ok();
    for p in &schema.points {
        writeln!(out, "{INDENT}struct {{").ok();
        for b in &p.bins {
            writeln!(out, "{INDENT}{INDENT}uint64_t {} = 0;", b.name).ok();
        }
        writeln!(out, "{INDENT}}} {};", p.name).ok();
    }
    for &(ai, bi) in &crosses {
        let (a, b) = (&schema.points[ai], &schema.points[bi]);
        writeln!(
            out,
            "{INDENT}uint64_t _auto_cross_{}__{}[{}][{}] = {{}};",
            a.name,
            b.name,
            a.bins.len(),
            b.bins.len()
        )
        .ok();
    }
    for cross in &declared {
        writeln!(
            out,
            "{INDENT}uint64_t {}[{}] = {{}};",
            cross.storage, cross.total_bins
        )
        .ok();
    }
    writeln!(out).ok();

    // report() — same ARCH-format coverage dump as v1 (stdout).
    let total_bins: usize = schema.points.iter().map(|p| p.bins.len()).sum();
    writeln!(out, "{INDENT}void report() const {{").ok();
    writeln!(
        out,
        "{INDENT}{INDENT}uint64_t _total = {total_bins}; uint64_t _hit = 0;"
    )
    .ok();
    for p in &schema.points {
        for b in &p.bins {
            writeln!(
                out,
                "{INDENT}{INDENT}if ({}.{} > 0) _hit++;",
                p.name, b.name
            )
            .ok();
        }
    }
    writeln!(
        out,
        "{INDENT}{INDENT}harc_rt::log::harc_print_covergroup_summary(\"{}\", _hit, _total);",
        schema.name
    )
    .ok();
    writeln!(
        out,
        "{INDENT}{INDENT}harc_rt::log::harc_cov_json_summary(\"{}\", _hit, _total);",
        schema.name
    )
    .ok();
    for p in &schema.points {
        for b in &p.bins {
            writeln!(
                out,
                "{INDENT}{INDENT}harc_rt::log::harc_print_covergroup_bin(\"{0}\", \"{1}\", {0}.{1});",
                p.name, b.name
            )
            .ok();
            writeln!(
                out,
                "{INDENT}{INDENT}harc_rt::log::harc_cov_json_bin(\"{}\", \"{}\", \"{}\", {}.{});",
                schema.name, p.name, b.name, p.name, b.name
            )
            .ok();
        }
    }
    // Declared crosses report before the auto-crosses — v1 ordering.
    for cross in &declared {
        let pad2 = INDENT.repeat(2);
        let pad3 = INDENT.repeat(3);
        writeln!(out, "{pad2}{{").ok();
        writeln!(out, "{pad3}uint64_t _cross_hit = 0;").ok();
        writeln!(out, "{pad3}uint64_t _cross_missing = 0;").ok();
        writeln!(
            out,
            "{pad3}for (size_t _i = 0; _i < {}; ++_i) if ({}[_i] > 0) _cross_hit++;",
            cross.total_bins, cross.storage
        )
        .ok();
        writeln!(
            out,
            "{pad3}harc_rt::log::harc_print_covergroup_cross_summary(\"{}\", \"cross\", \"{}\", _cross_hit, {});",
            schema.name, cross.label, cross.total_bins
        )
        .ok();
        writeln!(
            out,
            "{pad3}harc_rt::log::harc_cov_json_cross_summary(\"{}\", \"cross\", \"{}\", _cross_hit, {});",
            schema.name, cross.label, cross.total_bins
        )
        .ok();
        for (idx, label) in cross_bin_labels(cross) {
            writeln!(
                out,
                "{pad3}harc_rt::log::harc_cov_json_cross_bin(\"{}\", \"cross\", \"{}\", \"{}\", {}[{idx}]);",
                schema.name, cross.label, label, cross.storage
            )
            .ok();
            writeln!(
                out,
                "{pad3}if ({}[{idx}] == 0) {{ if (_cross_missing < {CROSS_MISSING_DETAIL_LIMIT}) harc_rt::log::harc_print_covergroup_missing_bin(\"{label}\"); _cross_missing++; }}",
                cross.storage
            )
            .ok();
        }
        writeln!(
            out,
            "{pad3}harc_rt::log::harc_print_covergroup_more_missing(_cross_missing, {CROSS_MISSING_DETAIL_LIMIT}, \"cross\");"
        )
        .ok();
        writeln!(out, "{pad2}}}").ok();
    }
    for &(ai, bi) in &crosses {
        let (a, b) = (&schema.points[ai], &schema.points[bi]);
        let pad2 = INDENT.repeat(2);
        let pad3 = INDENT.repeat(3);
        writeln!(out, "{pad2}{{").ok();
        writeln!(out, "{pad3}uint64_t _cross_hit = 0;").ok();
        writeln!(out, "{pad3}uint64_t _cross_missing = 0;").ok();
        writeln!(
            out,
            "{pad3}for (size_t _i = 0; _i < {}; ++_i) for (size_t _j = 0; _j < {}; ++_j) if (_auto_cross_{}__{}[_i][_j] > 0) _cross_hit++;",
            a.bins.len(),
            b.bins.len(),
            a.name,
            b.name
        )
        .ok();
        writeln!(
            out,
            "{pad3}harc_rt::log::harc_print_covergroup_cross_summary(\"{}\", \"auto_cross\", \"{} x {}\", _cross_hit, {});",
            schema.name,
            a.name,
            b.name,
            a.bins.len() * b.bins.len()
        )
        .ok();
        writeln!(
            out,
            "{pad3}harc_rt::log::harc_cov_json_cross_summary(\"{}\", \"auto_cross\", \"{} x {}\", _cross_hit, {});",
            schema.name,
            a.name,
            b.name,
            a.bins.len() * b.bins.len()
        )
        .ok();
        for (i, ab) in a.bins.iter().enumerate() {
            for (j, bb) in b.bins.iter().enumerate() {
                writeln!(
                    out,
                    "{pad3}harc_rt::log::harc_cov_json_cross_bin(\"{}\", \"auto_cross\", \"{} x {}\", \"{}.{} x {}.{}\", _auto_cross_{}__{}[{}][{}]);",
                    schema.name, a.name, b.name, a.name, ab.name, b.name, bb.name, a.name, b.name, i, j
                )
                .ok();
                writeln!(
                    out,
                    "{pad3}if (_auto_cross_{}__{}[{}][{}] == 0) {{ if (_cross_missing < {CROSS_MISSING_DETAIL_LIMIT}) harc_rt::log::harc_print_covergroup_missing_bin(\"{}.{} x {}.{}\"); _cross_missing++; }}",
                    a.name, b.name, i, j, a.name, ab.name, b.name, bb.name
                )
                .ok();
            }
        }
        writeln!(
            out,
            "{pad3}harc_rt::log::harc_print_covergroup_more_missing(_cross_missing, {CROSS_MISSING_DETAIL_LIMIT}, \"auto-cross\");"
        )
        .ok();
        writeln!(out, "{pad2}}}").ok();
    }
    writeln!(out, "{INDENT}}}").ok();
    writeln!(out, "}};").ok();
    writeln!(out).ok();
}

/// `_checkers.push_back([&]() { ...sample... });` at depth 1 — same
/// registration point and body shape as v1's clock-sample path, so the
/// per-cycle bin counts are identical. `instance` is the C++ lvalue of
/// the covergroup instance (`_tb.cov`).
pub(super) fn sampler_registration(
    out: &mut String,
    schema: &CovgroupSchema,
    instance: &str,
    lanes: &HashMap<String, u32>,
    ports: &HashMap<String, u32>,
) -> Result<(), EmitError> {
    writeln!(out, "{INDENT}_checkers.push_back([&]() {{").ok();
    let hook_types = HashMap::new();
    sample_body(
        out,
        schema,
        instance,
        &CoverWidths {
            lanes,
            ports,
            hook_types: &hook_types,
        },
        2,
    )?;
    writeln!(out, "{INDENT}}});").ok();
    Ok(())
}

/// Emit the per-point bin-membership tests + cross updates that make up
/// one covergroup sample, at indentation `depth`. Shared by the clock
/// (`_checkers`) and hook (`<Type>_<method>_<side>`) registration
/// wrappers; the body is identical so the per-sample bin counts match
/// regardless of trigger (byte-mirrors v1's `emit_covergroup_sample_body`).
fn sample_body(
    out: &mut String,
    schema: &CovgroupSchema,
    instance: &str,
    widths: &CoverWidths<'_>,
    depth: usize,
) -> Result<(), EmitError> {
    let crosses = auto_cross_pairs(schema);
    let declared = declared_crosses(schema);
    let any_cross = !crosses.is_empty() || !declared.is_empty();
    let pad2 = INDENT.repeat(depth);
    let pad3 = INDENT.repeat(depth + 1);
    let pad4 = INDENT.repeat(depth + 2);
    if any_cross {
        for p in schema.points.iter().filter(|p| !p.bins.is_empty()) {
            writeln!(
                out,
                "{pad2}bool _cg_hit_{}[{}] = {{}};",
                p.name,
                p.bins.len()
            )
            .ok();
        }
    }
    for p in &schema.points {
        writeln!(out, "{pad2}{{").ok();
        let target = cover_expr_cpp(&p.target, widths)?;
        writeln!(out, "{pad3}uint64_t _v = (uint64_t)({});", target).ok();
        for (bin_idx, b) in p.bins.iter().enumerate() {
            let membership = bin_membership(&b.values, widths)?;
            if !any_cross {
                writeln!(
                    out,
                    "{pad3}if ({membership}) {instance}.{}.{}++;",
                    p.name, b.name
                )
                .ok();
            } else {
                writeln!(
                    out,
                    "{pad3}if ({membership}) {{ {instance}.{}.{}++; _cg_hit_{}[{bin_idx}] = true; }}",
                    p.name, b.name, p.name
                )
                .ok();
            }
        }
        writeln!(out, "{pad2}}}").ok();
    }
    for &(ai, bi) in &crosses {
        let (a, b) = (&schema.points[ai], &schema.points[bi]);
        writeln!(
            out,
            "{pad2}for (size_t _i = 0; _i < {}; ++_i) {{",
            a.bins.len()
        )
        .ok();
        writeln!(
            out,
            "{pad3}for (size_t _j = 0; _j < {}; ++_j) {{",
            b.bins.len()
        )
        .ok();
        writeln!(
            out,
            "{pad4}if (_cg_hit_{}[_i] && _cg_hit_{}[_j]) {instance}._auto_cross_{}__{}[_i][_j]++;",
            a.name, b.name, a.name, b.name
        )
        .ok();
        writeln!(out, "{pad3}}}").ok();
        writeln!(out, "{pad2}}}").ok();
    }
    // Declared-cross updates: bump the flat cell for every combination
    // of bins hit in THIS sample (v1's nested `_i0.._iN` loops).
    for cross in &declared {
        for (idx, point) in cross.points.iter().enumerate() {
            let pad = INDENT.repeat(depth + idx);
            writeln!(
                out,
                "{pad}for (size_t _i{idx} = 0; _i{idx} < {}; ++_i{idx}) {{",
                point.bins.len()
            )
            .ok();
        }
        let pad_if = INDENT.repeat(depth + cross.points.len());
        let pad_body = INDENT.repeat(depth + 1 + cross.points.len());
        let hit_cond = cross
            .points
            .iter()
            .enumerate()
            .map(|(idx, point)| format!("_cg_hit_{}[_i{idx}]", point.name))
            .collect::<Vec<_>>()
            .join(" && ");
        writeln!(out, "{pad_if}if ({hit_cond}) {{").ok();
        writeln!(
            out,
            "{pad_body}{instance}.{}[{}]++;",
            cross.storage,
            cross_index_expr(cross)
        )
        .ok();
        writeln!(out, "{pad_if}}}").ok();
        for idx in (0..cross.points.len()).rev() {
            let pad = INDENT.repeat(depth + idx);
            writeln!(out, "{pad}}}").ok();
        }
    }
    Ok(())
}

/// Emit the `<Type>_<method>_pre` and `<Type>_<method>_post` hook-vector
/// declarations for one transactor method that has covergroup
/// subscribers. Mirrors v1's `emit_hook_vectors`: a
/// `std::vector<std::function<void(args)>>` per side, holding the sample
/// closures. The method body fans these out at its pre/post boundary
/// (`emit_method`). Param C-types match the method-lambda param shape.
pub(super) fn hook_vector_decls(
    out: &mut String,
    prog: &TbProgram,
    owner_name: &str,
    method_name: &str,
    function: FunctionId,
    n_params: usize,
    pad: &str,
) -> Result<(), EmitError> {
    let arg_csv = method_param_ctypes(prog, function, n_params).join(", ");
    writeln!(
        out,
        "{pad}std::vector<std::function<void({arg_csv})>> {}_{}_pre;",
        owner_name, method_name
    )
    .ok();
    writeln!(
        out,
        "{pad}std::vector<std::function<void({arg_csv})>> {}_{}_post;",
        owner_name, method_name
    )
    .ok();
    Ok(())
}

pub(super) fn transactor_hook_vector_decls(
    out: &mut String,
    prog: &TbProgram,
    schema: &TransactorSchema,
    m: &TransactorMethodSchema,
    pad: &str,
) -> Result<(), EmitError> {
    hook_vector_decls(
        out,
        prog,
        &schema.name,
        &m.name,
        m.function,
        m.param_names.len(),
        pad,
    )
}

/// Push one hook-triggered covergroup's sample closure onto the resolved
/// method's `<Type>_<method>_<side>` hook vector — the trigger-specific
/// analogue of `sampler_registration`. The closure takes the method's
/// args (so the body could sample them; the shipped target subset reads
/// DUT ports, identical to the clock sampler) and runs the same
/// `sample_body`. Mirrors v1's `emit_covergroup_hook_sample_registration`.
#[allow(clippy::too_many_arguments)]
pub(super) fn hook_sampler_registration(
    out: &mut String,
    prog: &TbProgram,
    schema: &CovgroupSchema,
    vector_base: String,
    function: FunctionId,
    n_params: usize,
    side: crate::ast::HookSide,
    instance: &str,
    lanes: &HashMap<String, u32>,
    ports: &HashMap<String, u32>,
) -> Result<(), EmitError> {
    let side_str = match side {
        crate::ast::HookSide::Pre => "pre",
        crate::ast::HookSide::Post => "post",
    };
    // Param decls for the closure signature: `<cty> <name>` per method
    // param, matching the hook-vector element type. Names come from the
    // method's lowered locals (param slots are the leading locals).
    let func = prog.function(function);
    let arg_decls: Vec<String> = (0..n_params)
        .map(|i| {
            let cty = param_cty(prog, &func.locals[i].ty);
            format!("{cty} {}", func.locals[i].name)
        })
        .collect();
    writeln!(
        out,
        "{INDENT}{vector_base}_{side_str}.push_back([&]({}) {{",
        arg_decls.join(", ")
    )
    .ok();
    let mut hook_types = HashMap::new();
    for param in func.params.iter().take(n_params) {
        hook_types.insert(param.name.clone(), param.ty.clone());
        if let IrType::Record(record) = &param.ty {
            for field in &prog.records[record.index()].fields {
                hook_types.insert(format!("{}.{}", param.name, field.name), field.ty.clone());
            }
        }
    }
    sample_body(
        out,
        schema,
        instance,
        &CoverWidths {
            lanes,
            ports,
            hook_types: &hook_types,
        },
        2,
    )?;
    writeln!(out, "{INDENT}}});").ok();
    Ok(())
}

/// C-type list for a method's params (the hook-vector signature),
/// matching `emit_method`'s lambda param shape exactly.
fn method_param_ctypes(prog: &TbProgram, function: FunctionId, n_params: usize) -> Vec<String> {
    let func = prog.function(function);
    (0..n_params)
        .map(|i| param_cty(prog, &func.locals[i].ty))
        .collect()
}

/// One param's C-type, mirroring `emit_method`'s mapping (record by
/// value, record-seq as `std::vector`, scalars via `local_scalar_cty`).
fn param_cty(prog: &TbProgram, ty: &IrType) -> String {
    match ty {
        IrType::Record(r) => prog.records[r.index()].name.clone(),
        IrType::RecordSeq(r) => format!("std::vector<{}>", prog.records[r.index()].name),
        IrType::Seq(scalar) => format!("std::vector<{}>", super::local_scalar_cty(scalar)),
        IrType::Component(c) => prog.components[c.index()].name.clone(),
        other => super::local_scalar_cty(other),
    }
}
