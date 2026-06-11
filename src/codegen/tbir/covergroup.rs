//! Covergroup struct + auto-sampler emission for the TB-IR backend.
//!
//! Byte-mirrors v1's `emit_covergroup_struct` / clock-sample
//! registration (cpp_tb.rs) for the schema subset that lowering
//! accepts: clock-triggered groups, value-set bins, and the implicit
//! pairwise auto-crosses. The `report()` text and the per-cycle sample
//! counts are runtime-observable (log lines + check-phase asserts), so
//! both must match v1 exactly for the trace-equivalence gate.

use super::expr::port_read;
use crate::ir::CovgroupSchema;
use std::fmt::Write as _;

const INDENT: &str = "    ";
/// Mirrors v1's `COVERGROUP_AUTO_CROSS_BIN_CAP`.
const AUTO_CROSS_BIN_CAP: usize = 64;
/// Mirrors v1's `COVERGROUP_CROSS_MISSING_DETAIL_LIMIT`.
const CROSS_MISSING_DETAIL_LIMIT: usize = 16;

/// Pairwise auto-crosses: indices into `schema.points` of binned-point
/// pairs whose bin product fits the cap. Declared crosses never reach
/// emission (lowering rejects them), so no exclusion set is needed —
/// this matches v1's `covergroup_auto_crosses` on the accepted subset.
fn auto_cross_pairs(schema: &CovgroupSchema) -> Vec<(usize, usize)> {
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
            if schema.points[a].bins.len() * schema.points[b].bins.len() <= AUTO_CROSS_BIN_CAP {
                pairs.push((a, b));
            }
        }
    }
    pairs
}

/// `struct <Name> { ... bin counters ... void report() const { ... } };`
pub(super) fn covgroup_struct(out: &mut String, schema: &CovgroupSchema) {
    let crosses = auto_cross_pairs(schema);
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
    let pad1 = INDENT;
    let pad2 = INDENT.repeat(2);
    let pad3 = INDENT.repeat(3);
    let pad4 = INDENT.repeat(4);
    writeln!(out, "{pad1}_checkers.push_back([&]() {{").ok();
    if !crosses.is_empty() {
        for p in schema.points.iter().filter(|p| !p.bins.is_empty()) {
            writeln!(out, "{pad2}bool _cg_hit_{}[{}] = {{}};", p.name, p.bins.len()).ok();
        }
    }
    for p in &schema.points {
        writeln!(out, "{pad2}{{").ok();
        writeln!(
            out,
            "{pad3}uint64_t _v = (uint64_t)({});",
            port_read(&p.target)
        )
        .ok();
        for (bin_idx, b) in p.bins.iter().enumerate() {
            let membership = if b.values.is_empty() {
                "(false)".to_string()
            } else {
                format!(
                    "({})",
                    b.values
                        .iter()
                        .map(|v| format!("(_v == {v})"))
                        .collect::<Vec<_>>()
                        .join(" || ")
                )
            };
            if crosses.is_empty() {
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
    writeln!(out, "{pad1}}});").ok();
}
