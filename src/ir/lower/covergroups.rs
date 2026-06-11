//! Covergroup lowering: `CovergroupDecl` → `CovgroupSchema`.
//!
//! Scope mirrors what the tbir emitter reproduces byte-compatibly from
//! v1: clock-triggered (or trigger-less) covergroups whose points
//! sample a direct DUT port and whose bins are finite value sets.
//! Hook triggers, declared `cross` items, and range bin specs stay
//! `Unsupported` — never silently mis-lowered.

use super::{LowerError, unsupported};
use crate::ast::{CoverItem, CoverTrigger, CovergroupDecl, Expr as AstExpr, ExprKind};
use crate::ir::{CovTrigger, CoverBinSchema, CoverPointSchema, CovgroupSchema, PortAccess, PortRef};

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

    let mut points = Vec::new();
    for it in &g.items {
        match it {
            CoverItem::Point(p) => {
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
            CoverItem::Cross(_) => {
                return Err(unsupported(
                    &format!("covergroup `{}` declared cross bins", g.name.name),
                    "",
                ));
            }
        }
    }

    Ok(CovgroupSchema {
        name: g.name.name.clone(),
        trigger,
        points,
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

/// Lower a bin spec into its finite value set. Supported shapes:
///   `{v}` / `{a, b, c}`  — set literals of integer literals
///   `v`                  — a bare integer literal
///   `(inner)`            — parenthesized recursion
/// Ranges (`[a..b]`) and non-literal members are post-MVP.
pub(crate) fn lower_bin_values(
    group: &str,
    bin: &str,
    spec: &AstExpr,
) -> Result<Vec<u64>, LowerError> {
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
            let v = super::exprs::parse_int_literal(s).ok_or_else(|| {
                unsupported(
                    &format!("covergroup `{group}` bin `{bin}`"),
                    format!("`{s}` is not a plain integer literal"),
                )
            })?;
            Ok(vec![v])
        }
        ExprKind::RangeLit { .. } => Err(unsupported(
            &format!("covergroup `{group}` bin `{bin}` range specs"),
            "",
        )),
        _ => Err(unsupported(
            &format!("covergroup `{group}` bin `{bin}` with a non-literal spec"),
            "",
        )),
    }
}
