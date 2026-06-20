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
    BinOp, CovBinValue, CoverPointSchema, CovgroupSchema, Expr, IrType, PortRef, TbProgram,
    TransactorMethodSchema, TransactorSchema, UnOp, WidthCastKind,
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
fn cover_expr_cpp(e: &Expr, lanes: &HashMap<String, u32>) -> Result<String, EmitError> {
    Ok(match e {
        Expr::Literal { value, .. } => format!("{value}"),
        Expr::Port(p) => {
            let sig = cover_port_signal(p);
            match &p.lane {
                None => format!("harc_rt::harc_read({sig})"),
                Some(lane) => {
                    // Covergroup lane indices are constant-only (the
                    // schema lowers before any runtime scope; see
                    // `lower_covergroups`), so a `Var` index never
                    // reaches here. Render a constant directly; surface a
                    // precise codegen error if a runtime index ever does.
                    let idx = match lane {
                        crate::ir::LaneIndex::Const(c) => c.to_string(),
                        crate::ir::LaneIndex::Var(_) => {
                            return Err(EmitError(
                                "tbir: covergroup point with a runtime lane \
                                 index — only constant lanes are in subset"
                                    .to_string(),
                            ))
                        }
                    };
                    match cover_lane_width(lanes, p) {
                        Some(w) => {
                            format!(
                                "harc_rt::harc_vec_lane_read<{w}>({sig}, (std::size_t)({idx}))"
                            )
                        }
                        None => format!("{sig}[{idx}]"),
                    }
                }
            }
        }
        Expr::Binary(op, a, b) => {
            let a = cover_expr_cpp(a, lanes)?;
            let b = cover_expr_cpp(b, lanes)?;
            format!("({a} {} {b})", cover_bin_op_cpp(*op))
        }
        Expr::Unary(op, a) => {
            let a = cover_expr_cpp(a, lanes)?;
            format!("{}({a})", cover_un_op_cpp(*op))
        }
        Expr::Ternary(c, t, f) => {
            let c = cover_expr_cpp(c, lanes)?;
            let t = cover_expr_cpp(t, lanes)?;
            let f = cover_expr_cpp(f, lanes)?;
            format!("({c} ? {t} : {f})")
        }
        Expr::BitSlice { target, hi, lo } => {
            let t = cover_expr_cpp(target, lanes)?;
            let width = hi - lo + 1;
            let mask = if width >= 64 {
                u64::MAX
            } else {
                (1u64 << width) - 1
            };
            format!("(((uint64_t)({t}) >> {lo}) & 0x{mask:X}ULL)")
        }
        Expr::WidthCast {
            kind,
            width,
            src_width,
            inner,
        } => cover_width_cast_cpp(*kind, *width, *src_width, inner, lanes)?,
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
                let i = cover_expr_cpp(idx, lanes)?;
                format!("{param}.{field}[{i}]")
            }
            None => format!("{param}.{field}"),
        },
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
    match p.port_path.as_slice() {
        [name] => lanes.get(name).copied(),
        _ => None,
    }
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
    lanes: &HashMap<String, u32>,
) -> Result<String, EmitError> {
    let e = cover_expr_cpp(inner, lanes)?;
    let mask = |w: u32| (1u64 << w) - 1;
    let trunc_shape = |e: &str| {
        if width == 64 {
            format!("((uint64_t)({e}))")
        } else {
            format!("((uint64_t)((({e}) & 0x{:X}ULL)))", mask(width))
        }
    };
    let plain_cast = |e: &str| format!("((uint64_t)({e}))");
    Ok(match kind {
        WidthCastKind::Trunc => trunc_shape(&e),
        WidthCastKind::Zext => plain_cast(&e),
        WidthCastKind::Sext => match src_width {
            Some(sw) if sw < width => {
                let shift = 64 - sw;
                if width == 64 {
                    format!("((uint64_t)(((int64_t)((uint64_t)({e}) << {shift})) >> {shift}))")
                } else {
                    format!(
                        "((uint64_t)(((int64_t)((uint64_t)({e}) << {shift})) >> {shift}) & 0x{:X}ULL)",
                        mask(width)
                    )
                }
            }
            _ => plain_cast(&e),
        },
        WidthCastKind::Resize => match src_width {
            Some(sw) if width < sw => trunc_shape(&e),
            Some(_) => plain_cast(&e),
            None => trunc_shape(&e),
        },
    })
}

fn bin_membership(values: &[CovBinValue]) -> String {
    if values.is_empty() {
        return "(false)".to_string();
    }
    let parts = values
        .iter()
        .map(|v| match v {
            CovBinValue::Eq(x) => format!("(_v == {x})"),
            CovBinValue::Range { lo, hi } => match (lo, hi) {
                (Some(l), Some(h)) => format!("(_v >= {l} && _v <= {h})"),
                (Some(l), None) => format!("(_v >= {l})"),
                (None, Some(h)) => format!("(_v <= {h})"),
                (None, None) => "(true)".to_string(),
            },
        })
        .collect::<Vec<_>>();
    format!("({})", parts.join(" || "))
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
    for p in &schema.points {
        for b in &p.bins {
            writeln!(
                out,
                "{INDENT}{INDENT}harc_rt::log::harc_print_covergroup_bin(\"{0}\", \"{1}\", {0}.{1});",
                p.name, b.name
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
        for (idx, label) in cross_bin_labels(cross) {
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
        for (i, ab) in a.bins.iter().enumerate() {
            for (j, bb) in b.bins.iter().enumerate() {
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
) -> Result<(), EmitError> {
    writeln!(out, "{INDENT}_checkers.push_back([&]() {{").ok();
    sample_body(out, schema, instance, lanes, 2)?;
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
    lanes: &HashMap<String, u32>,
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
        let target = cover_expr_cpp(&p.target, lanes)?;
        writeln!(out, "{pad3}uint64_t _v = (uint64_t)({});", target).ok();
        for (bin_idx, b) in p.bins.iter().enumerate() {
            let membership = bin_membership(&b.values);
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
    schema: &TransactorSchema,
    m: &TransactorMethodSchema,
    pad: &str,
) -> Result<(), EmitError> {
    let arg_csv = method_param_ctypes(prog, m).join(", ");
    writeln!(
        out,
        "{pad}std::vector<std::function<void({arg_csv})>> {}_{}_pre;",
        schema.name, m.name
    )
    .ok();
    writeln!(
        out,
        "{pad}std::vector<std::function<void({arg_csv})>> {}_{}_post;",
        schema.name, m.name
    )
    .ok();
    Ok(())
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
    xschema: &TransactorSchema,
    m: &TransactorMethodSchema,
    side: crate::ast::HookSide,
    instance: &str,
    lanes: &HashMap<String, u32>,
) -> Result<(), EmitError> {
    let side_str = match side {
        crate::ast::HookSide::Pre => "pre",
        crate::ast::HookSide::Post => "post",
    };
    // Param decls for the closure signature: `<cty> <name>` per method
    // param, matching the hook-vector element type. Names come from the
    // method's lowered locals (param slots are the leading locals).
    let func = prog.function(m.function);
    let arg_decls: Vec<String> = (0..m.n_params)
        .map(|i| {
            let cty = param_cty(prog, &func.locals[i].ty);
            format!("{cty} {}", func.locals[i].name)
        })
        .collect();
    writeln!(
        out,
        "{INDENT}{}_{}_{side_str}.push_back([&]({}) {{",
        xschema.name,
        m.name,
        arg_decls.join(", ")
    )
    .ok();
    sample_body(out, schema, instance, lanes, 2)?;
    writeln!(out, "{INDENT}}});").ok();
    Ok(())
}

/// C-type list for a method's params (the hook-vector signature),
/// matching `emit_method`'s lambda param shape exactly.
fn method_param_ctypes(prog: &TbProgram, m: &TransactorMethodSchema) -> Vec<String> {
    let func = prog.function(m.function);
    (0..m.n_params)
        .map(|i| param_cty(prog, &func.locals[i].ty))
        .collect()
}

/// One param's C-type, mirroring `emit_method`'s mapping (record by
/// value, record-seq as `std::vector`, scalars via `local_scalar_cty`).
fn param_cty(prog: &TbProgram, ty: &IrType) -> String {
    match ty {
        IrType::Record(r) => prog.records[r.index()].name.clone(),
        IrType::RecordSeq(r) => format!("std::vector<{}>", prog.records[r.index()].name),
        other => super::local_scalar_cty(other),
    }
}
