//! Covergroup lowering: `CovergroupDecl` → `CovgroupSchema`.
//!
//! Scope mirrors what the tbir emitter reproduces byte-compatibly from
//! v1: clock-triggered (or trigger-less) covergroups whose points
//! sample a direct DUT port and whose bins are finite value sets and/or
//! inclusive ranges, plus declared `cross` items over those points.
//! Hook triggers stay `Unsupported` — never silently mis-lowered.

use super::{LowerError, unsupported};
use crate::ast::{CoverItem, CoverTrigger, CovergroupDecl, Expr as AstExpr, ExprKind};
use crate::ir::{
    CovBinValue, CovTrigger, CoverBinSchema, CoverCrossSchema, CoverPointSchema, CovgroupSchema,
    PortAccess, PortRef,
};

pub(crate) fn lower_covergroup(g: &CovergroupDecl) -> Result<CovgroupSchema, LowerError> {
    // v1's clock-sample registration ignores the trigger expression
    // entirely — any non-hook covergroup samples once per primary-clock
    // tick. Mirror that: `@(posedge dut.clk)` and a missing trigger
    // both lower to `PosedgeDutClk`; hook triggers are post-MVP.
    let trigger = match &g.trigger {
        Some(CoverTrigger::Hook { .. }) => {
            return Err(unsupported(
                &format!("covergroup `{}` hook triggers", g.name.name),
                "",
            ));
        }
        Some(CoverTrigger::Clock(_)) | None => CovTrigger::PosedgeDutClk,
    };

    // Pass 1: points (a `cross` may reference a point declared after it).
    let mut points = Vec::new();
    for it in &g.items {
        if let CoverItem::Point(p) = it {
            let target = lower_point_target(&g.name.name, &p.name.name, &p.target)?;
            let mut bins = Vec::new();
            for b in &p.bins {
                bins.push(CoverBinSchema {
                    name: b.name.name.clone(),
                    values: lower_bin_values(&g.name.name, &b.name.name, &b.spec)?,
                });
            }
            points.push(CoverPointSchema {
                name: p.name.name.clone(),
                target,
                bins,
            });
        }
    }

    // Pass 2: declared crosses, with the same structural validation v1
    // applies in `covergroup_declared_crosses` (there those are
    // emission-time `self.errors`; here they fail lowering — same
    // compile-failure outcome).
    let mut crosses = Vec::new();
    for (item_index, it) in g.items.iter().enumerate() {
        let CoverItem::Cross(cross) = it else {
            continue;
        };
        if cross.points.len() < 2 {
            return Err(LowerError::Invalid(format!(
                "covergroup `{}` cross must name at least two coverpoints",
                g.name.name
            )));
        }
        let mut point_indices = Vec::new();
        for ident in &cross.points {
            let Some(idx) = points.iter().position(|p| p.name == ident.name) else {
                return Err(LowerError::Invalid(format!(
                    "covergroup `{}` cross references unknown coverpoint `{}`",
                    g.name.name, ident.name
                )));
            };
            if points[idx].bins.is_empty() {
                return Err(LowerError::Invalid(format!(
                    "covergroup `{}` cross references coverpoint `{}` with no bins",
                    g.name.name, ident.name
                )));
            }
            point_indices.push(idx);
        }
        let mut total_bins = 1usize;
        for &idx in &point_indices {
            total_bins = total_bins
                .checked_mul(points[idx].bins.len())
                .ok_or_else(|| {
                    LowerError::Invalid(format!(
                        "covergroup `{}` cross `{}` has too many bins",
                        g.name.name,
                        cross
                            .points
                            .iter()
                            .map(|p| p.name.as_str())
                            .collect::<Vec<_>>()
                            .join(" x ")
                    ))
                })?;
        }
        crosses.push(CoverCrossSchema {
            item_index,
            point_indices,
        });
    }

    Ok(CovgroupSchema {
        name: g.name.name.clone(),
        trigger,
        points,
        crosses,
    })
}

/// A cover-point target must be a direct DUT port read (`dut.<port>`).
fn lower_point_target(
    group: &str,
    point: &str,
    target: &AstExpr,
) -> Result<PortRef, LowerError> {
    let mut cur = target;
    loop {
        match &*cur.kind {
            ExprKind::Paren(inner) => cur = inner,
            ExprKind::Field { target: ft, name } => {
                if let ExprKind::Ident(root) = &*ft.kind {
                    if root.name == "dut" {
                        return Ok(PortRef {
                            testbench_field: "dut".to_string(),
                            port_path: vec![name.name.clone()],
                            direction: None,
                            width: None,
                            access: PortAccess::Port,
                            lane: None,
                        });
                    }
                }
                return Err(unsupported(
                    &format!(
                        "covergroup `{group}` point `{point}` with a non-`dut.<port>` target"
                    ),
                    "",
                ));
            }
            _ => {
                return Err(unsupported(
                    &format!(
                        "covergroup `{group}` point `{point}` with a non-`dut.<port>` target"
                    ),
                    "",
                ));
            }
        }
    }
}

/// Lower a bin spec into its membership set. Supported shapes:
///   `{v}` / `{a, b, c}`  — set literals of integer literals
///   `v`                  — a bare integer literal
///   `[a..b]`             — inclusive range; either bound may be open
///   `{[1..3], 7}`        — set-of-ranges (recursion)
///   `(inner)`            — parenthesized recursion
/// Non-literal members/bounds are post-MVP.
pub(crate) fn lower_bin_values(
    group: &str,
    bin: &str,
    spec: &AstExpr,
) -> Result<Vec<CovBinValue>, LowerError> {
    match &*spec.kind {
        ExprKind::SetLit(items) => {
            let mut values = Vec::with_capacity(items.len());
            for it in items {
                values.extend(lower_bin_values(group, bin, it)?);
            }
            Ok(values)
        }
        ExprKind::Paren(inner) => lower_bin_values(group, bin, inner),
        ExprKind::Int(s) => {
            let v = parse_bound(group, bin, s)?;
            Ok(vec![CovBinValue::Eq(v)])
        }
        ExprKind::RangeLit { lo, hi } => {
            let lo = lo
                .as_ref()
                .map(|e| lower_bin_bound(group, bin, e))
                .transpose()?;
            let hi = hi
                .as_ref()
                .map(|e| lower_bin_bound(group, bin, e))
                .transpose()?;
            Ok(vec![CovBinValue::Range { lo, hi }])
        }
        _ => Err(unsupported(
            &format!("covergroup `{group}` bin `{bin}` with a non-literal spec"),
            "",
        )),
    }
}

/// A range bound must reduce to a plain integer literal (parens ok).
fn lower_bin_bound(group: &str, bin: &str, e: &AstExpr) -> Result<u64, LowerError> {
    match &*e.kind {
        ExprKind::Paren(inner) => lower_bin_bound(group, bin, inner),
        ExprKind::Int(s) => parse_bound(group, bin, s),
        _ => Err(unsupported(
            &format!("covergroup `{group}` bin `{bin}` with a non-literal range bound"),
            "",
        )),
    }
}

fn parse_bound(group: &str, bin: &str, s: &str) -> Result<u64, LowerError> {
    super::exprs::parse_int_literal(s).ok_or_else(|| {
        unsupported(
            &format!("covergroup `{group}` bin `{bin}`"),
            format!("`{s}` is not a plain integer literal"),
        )
    })
}
