//! Covergroup struct + auto-sampler emission for the TB-IR backend.
//!
//! Byte-mirrors v1's `emit_covergroup_struct` / clock-sample
//! registration (cpp_tb.rs) for the schema subset that lowering
//! accepts: clock-triggered groups, value-set/range bins, declared
//! `cross` items, and the implicit pairwise auto-crosses. The
//! `report()` text and the per-cycle sample counts are
//! runtime-observable (log lines + check-phase asserts), so both must
//! match v1 exactly for the trace-equivalence gate.

use super::expr::port_signal;
use crate::ir::{CovBinValue, CoverPointSchema, CovgroupSchema};
use std::collections::BTreeSet;
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
            let points: Vec<&CoverPointSchema> = c
                .point_indices
                .iter()
                .map(|&i| &schema.points[i])
                .collect();
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
/// Covergroup sample-point read. Cover targets are direct DUT ports
/// (no lanes), so this stays the plain `harc_read` wrap.
fn cover_target_read(p: &crate::ir::PortRef) -> String {
    format!("harc_rt::harc_read({})", port_signal(p))
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
            writeln!(out, "{INDENT}{INDENT}if ({}.{} > 0) _hit++;", p.name, b.name).ok();
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
pub(super) fn sampler_registration(out: &mut String, schema: &CovgroupSchema, instance: &str) {
    let crosses = auto_cross_pairs(schema);
    let declared = declared_crosses(schema);
    let any_cross = !crosses.is_empty() || !declared.is_empty();
    let pad1 = INDENT;
    let pad2 = INDENT.repeat(2);
    let pad3 = INDENT.repeat(3);
    let pad4 = INDENT.repeat(4);
    writeln!(out, "{pad1}_checkers.push_back([&]() {{").ok();
    if any_cross {
        for p in schema.points.iter().filter(|p| !p.bins.is_empty()) {
            writeln!(out, "{pad2}bool _cg_hit_{}[{}] = {{}};", p.name, p.bins.len()).ok();
        }
    }
    for p in &schema.points {
        writeln!(out, "{pad2}{{").ok();
        writeln!(
            out,
            "{pad3}uint64_t _v = (uint64_t)({});",
            cover_target_read(&p.target)
        )
        .ok();
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
        writeln!(out, "{pad2}for (size_t _i = 0; _i < {}; ++_i) {{", a.bins.len()).ok();
        writeln!(out, "{pad3}for (size_t _j = 0; _j < {}; ++_j) {{", b.bins.len()).ok();
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
            let pad = INDENT.repeat(2 + idx);
            writeln!(
                out,
                "{pad}for (size_t _i{idx} = 0; _i{idx} < {}; ++_i{idx}) {{",
                point.bins.len()
            )
            .ok();
        }
        let pad_if = INDENT.repeat(2 + cross.points.len());
        let pad_body = INDENT.repeat(3 + cross.points.len());
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
            let pad = INDENT.repeat(2 + idx);
            writeln!(out, "{pad}}}").ok();
        }
    }
    writeln!(out, "{pad1}}});").ok();
}
