//! Covergroup lowering: `CovergroupDecl` → `CovgroupSchema`.
//!
//! Scope mirrors what the tbir emitter reproduces byte-compatibly from
//! v1: clock-triggered (or trigger-less) covergroups whose points
//! sample a direct DUT port and whose bins are finite value sets and/or
//! inclusive ranges, plus declared `cross` items over those points.
//! Hook triggers stay `Unsupported` — never silently mis-lowered.

use super::{unsupported, LowerError};
use crate::ast::{CoverItem, CoverTrigger, CovergroupDecl, Expr as AstExpr, ExprKind, UnaryOp};
use crate::ir::{
    CovBinValue, CovTrigger, CoverBinSchema, CoverCrossSchema, CoverPointSchema, CovgroupSchema,
    Expr, IrType, PortAccess, PortRef, UnOp, WidthCastKind,
};

pub(crate) fn lower_covergroup(g: &CovergroupDecl) -> Result<CovgroupSchema, LowerError> {
    // v1's clock-sample registration ignores the trigger expression
    // entirely — any non-hook covergroup samples once per primary-clock
    // tick. Mirror that: `@(posedge dut.clk)` and a missing trigger
    // both lower to `PosedgeDutClk`; hook triggers are post-MVP.
    let trigger = match &g.trigger {
        Some(CoverTrigger::Hook { call, side }) => {
            // Covergroup schemas lower before the testbench/transactor
            // tables exist, so the hook target cannot be resolved here.
            // Stash the receiver field-access path + method name; the
            // `covergroup_hooks` pass resolves it once transactors are
            // lowered (mirrors v1's late covergroup-hook registration).
            let (receiver_path, method) = lower_hook_call(&g.name.name, call)?;
            CovTrigger::Hook {
                receiver_path,
                method,
                side: *side,
            }
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

/// Extract `(receiver_path, method_name)` from a covergroup hook
/// trigger call `recv.path.method(args)`. The args were already
/// validated to be bare identifiers by the parser
/// (`validate_cover_hook_trigger`); the receiver path is resolved later
/// by the `covergroup_hooks` pass. Returns the field-access path
/// leading to the method (e.g. `["drv"]` for `drv.send(t)`).
fn lower_hook_call(group: &str, call: &AstExpr) -> Result<(Vec<String>, String), LowerError> {
    let ExprKind::Call { callee, .. } = &*call.kind else {
        return Err(unsupported(
            &format!("covergroup `{group}` hook trigger must be a method call"),
            "",
        ));
    };
    let ExprKind::Field { target, name } = &*callee.kind else {
        return Err(unsupported(
            &format!("covergroup `{group}` hook trigger must be `<obj>.<method>(args)`"),
            "",
        ));
    };
    let method = name.name.clone();
    let mut path: Vec<String> = Vec::new();
    let mut cur: &AstExpr = target;
    loop {
        match &*cur.kind {
            ExprKind::Paren(inner) => cur = inner,
            ExprKind::Field {
                target: inner,
                name,
            } => {
                path.push(name.name.clone());
                cur = inner;
            }
            ExprKind::Ident(id) => {
                path.push(id.name.clone());
                break;
            }
            _ => {
                return Err(unsupported(
                    &format!(
                        "covergroup `{group}` hook trigger receiver must be a field-access path"
                    ),
                    "",
                ))
            }
        }
    }
    path.reverse();
    Ok((path, method))
}

/// A cover-point target is a pure DUT-bound scalar expression. Keep this
/// subset deliberately smaller than general expression lowering because
/// covergroup schemas lower before test/run scopes exist: no locals,
/// helper calls, regblock reads, or transactor edges.
fn lower_point_target(group: &str, point: &str, target: &AstExpr) -> Result<Expr, LowerError> {
    let unsupported_target = || {
        unsupported(
            &format!("covergroup `{group}` point `{point}` with an unsupported target expression"),
            "supported: dut.<port>, dut.<port>[idx], expr[hi:lo], literals, unary/binary/ternary expressions",
        )
    };

    fn lower_port(group: &str, point: &str, e: &AstExpr) -> Result<Option<PortRef>, LowerError> {
        let mut cur = e;
        let mut lane = None;
        if let ExprKind::Index { target, index } = &*cur.kind {
            // Covergroup schemas lower before any test/run scope exists,
            // so a lane index has no runtime locals to reference: only a
            // constant is in subset (`cover_const_u64` rejects anything
            // else). Kept as `LaneIndex::Const`.
            lane = Some(crate::ir::LaneIndex::Const(cover_const_u64(
                group, point, index,
            )?));
            cur = target;
        }
        let mut segments: Vec<String> = Vec::new();
        loop {
            match &*cur.kind {
                ExprKind::Paren(inner) => cur = inner,
                ExprKind::Field { target: ft, name } => {
                    segments.push(name.name.clone());
                    cur = ft;
                }
                ExprKind::Ident(root) if root.name == "dut" && !segments.is_empty() => {
                    if segments.len() > 1 {
                        return Err(unsupported(
                            &format!(
                                "covergroup `{group}` point `{point}` with nested DUT port paths"
                            ),
                            "",
                        ));
                    }
                    segments.reverse();
                    return Ok(Some(PortRef {
                        testbench_field: "dut".to_string(),
                        port_path: segments,
                        direction: None,
                        width: None,
                        access: PortAccess::Port,
                        lane,
                    }));
                }
                _ => return Ok(None),
            }
        }
    }

    if let Some(port) = lower_port(group, point, target)? {
        return Ok(Expr::Port(port));
    }

    let mut cur = target;
    loop {
        match &*cur.kind {
            ExprKind::Paren(inner) => cur = inner,
            ExprKind::Int(s) => {
                let value = super::exprs::parse_int_literal(s).ok_or_else(unsupported_target)?;
                return Ok(Expr::Literal {
                    value,
                    ty: IrType::Unknown,
                });
            }
            ExprKind::Bool(b) => {
                return Ok(Expr::Literal {
                    value: *b as u64,
                    ty: IrType::Bool,
                });
            }
            ExprKind::Unary { op, expr } => {
                let inner = lower_point_target(group, point, expr)?;
                let op = match op {
                    UnaryOp::Neg => UnOp::Neg,
                    UnaryOp::Not | UnaryOp::NotKw => UnOp::Not,
                    UnaryOp::BitNot => UnOp::BitNot,
                };
                return Ok(Expr::Unary(op, Box::new(inner)));
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let op = super::exprs::lower_bin_op(*op)?;
                let lhs = lower_point_target(group, point, lhs)?;
                let rhs = lower_point_target(group, point, rhs)?;
                return Ok(Expr::Binary(op, Box::new(lhs), Box::new(rhs)));
            }
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => {
                let cond = lower_point_target(group, point, cond)?;
                let then_branch = lower_point_target(group, point, then_branch)?;
                let else_branch = lower_point_target(group, point, else_branch)?;
                return Ok(Expr::Ternary(
                    Box::new(cond),
                    Box::new(then_branch),
                    Box::new(else_branch),
                ));
            }
            ExprKind::BitSlice { target, hi, lo } => {
                let hi = cover_const_u32(group, point, hi)?;
                let lo = cover_const_u32(group, point, lo)?;
                if hi < lo {
                    return Err(LowerError::Invalid(format!(
                        "covergroup `{group}` point `{point}` has invalid bit slice [{hi}:{lo}]"
                    )));
                }
                let target = lower_point_target(group, point, target)?;
                return Ok(Expr::BitSlice {
                    target: Box::new(target),
                    hi,
                    lo,
                });
            }
            ExprKind::Cast { expr, ty } => {
                let width = cover_cast_width(group, point, ty)?.ok_or_else(unsupported_target)?;
                let inner = lower_point_target(group, point, expr)?;
                return Ok(Expr::WidthCast {
                    kind: WidthCastKind::Resize,
                    width,
                    src_width: cover_infer_expr_width(group, point, expr)?,
                    inner: Box::new(inner),
                });
            }
            ExprKind::Call { callee, args } => {
                if let ExprKind::Field { target: recv, name } = &*callee.kind {
                    if matches!(name.name.as_str(), "trunc" | "zext" | "sext" | "resize") {
                        // Width-method calls are parsed with the width as the first argument.
                        let width_expr = match args.first() {
                            Some(crate::ast::CallArg::Expr(e)) if args.len() == 1 => e,
                            _ => return Err(unsupported_target()),
                        };
                        let width = cover_width_arg(group, point, &name.name, width_expr)?;
                        let src_width = cover_infer_expr_width(group, point, recv)?;
                        let inner = lower_point_target(group, point, recv)?;
                        let kind = match name.name.as_str() {
                            "trunc" => WidthCastKind::Trunc,
                            "zext" => WidthCastKind::Zext,
                            "sext" => WidthCastKind::Sext,
                            "resize" => WidthCastKind::Resize,
                            _ => unreachable!(),
                        };
                        if let Some(sw) = src_width {
                            match kind {
                                WidthCastKind::Trunc if width >= sw => {
                                    return Err(LowerError::Invalid(format!(
                                        "covergroup `{group}` point `{point}` `.trunc<{width}>()` on a {sw}-bit value: width must be strictly less than the source width"
                                    )));
                                }
                                WidthCastKind::Zext | WidthCastKind::Sext if width < sw => {
                                    return Err(LowerError::Invalid(format!(
                                        "covergroup `{group}` point `{point}` `.{method}<{width}>()` on a {sw}-bit value: width must be >= the source width",
                                        method = name.name
                                    )));
                                }
                                _ => {}
                            }
                        }
                        return Ok(Expr::WidthCast {
                            kind,
                            width,
                            src_width,
                            inner: Box::new(inner),
                        });
                    }
                }
                return Err(unsupported_target());
            }
            _ => return Err(unsupported_target()),
        }
    }
}

fn cover_const_u64(group: &str, point: &str, e: &AstExpr) -> Result<u64, LowerError> {
    match &*e.kind {
        ExprKind::Paren(inner) => cover_const_u64(group, point, inner),
        ExprKind::Int(s) => super::exprs::parse_int_literal(s).ok_or_else(|| {
            unsupported(
                &format!("covergroup `{group}` point `{point}` constant"),
                format!("`{s}` is not a plain integer literal"),
            )
        }),
        _ => Err(unsupported(
            &format!("covergroup `{group}` point `{point}` non-constant index/slice bound"),
            "",
        )),
    }
}

fn cover_const_u32(group: &str, point: &str, e: &AstExpr) -> Result<u32, LowerError> {
    let v = cover_const_u64(group, point, e)?;
    u32::try_from(v).map_err(|_| {
        LowerError::Invalid(format!(
            "covergroup `{group}` point `{point}` constant {v} does not fit in u32"
        ))
    })
}

fn cover_width_arg(group: &str, point: &str, method: &str, e: &AstExpr) -> Result<u32, LowerError> {
    let width = cover_const_u32(group, point, e)?;
    if width == 0 {
        return Err(LowerError::Invalid(format!(
            "covergroup `{group}` point `{point}` `.{method}<0>()`: width must be greater than zero"
        )));
    }
    if width > 64 {
        return Err(unsupported(
            &format!("covergroup `{group}` point `{point}` `.{method}<{width}>()`"),
            "the TB-IR covergroup expression model is 64-bit",
        ));
    }
    Ok(width)
}

fn cover_cast_width(
    group: &str,
    point: &str,
    ty: &crate::ast::TypeExpr,
) -> Result<Option<u32>, LowerError> {
    let width = match ty {
        crate::ast::TypeExpr::Builtin {
            name:
                crate::ast::BuiltinTy::UInt | crate::ast::BuiltinTy::SInt | crate::ast::BuiltinTy::Bits,
            args,
            ..
        } => match args.first() {
            Some(crate::ast::TypeArg::Expr(e)) => Some(cover_const_u32(group, point, e)?),
            Some(_) => None,
            None => Some(64),
        },
        _ => None,
    };
    if let Some(width) = width {
        if width == 0 {
            return Err(LowerError::Invalid(format!(
                "covergroup `{group}` point `{point}` cast width must be greater than zero"
            )));
        }
        if width > 64 {
            return Err(unsupported(
                &format!("covergroup `{group}` point `{point}` cast to {width} bits"),
                "the TB-IR covergroup expression model is 64-bit",
            ));
        }
    }
    Ok(width)
}

fn cover_infer_expr_width(
    group: &str,
    point: &str,
    e: &AstExpr,
) -> Result<Option<u32>, LowerError> {
    match &*e.kind {
        ExprKind::Paren(inner) => cover_infer_expr_width(group, point, inner),
        ExprKind::BitSlice { hi, lo, .. } => {
            let hi = cover_const_u32(group, point, hi)?;
            let lo = cover_const_u32(group, point, lo)?;
            if hi < lo {
                return Err(LowerError::Invalid(format!(
                    "covergroup `{group}` point `{point}` has invalid bit slice [{hi}:{lo}]"
                )));
            }
            Ok(Some(hi - lo + 1))
        }
        ExprKind::Cast { ty, .. } => cover_cast_width(group, point, ty),
        ExprKind::Call { callee, args } => {
            if let ExprKind::Field { name, .. } = &*callee.kind {
                if matches!(name.name.as_str(), "trunc" | "zext" | "sext" | "resize") {
                    let width_expr = match args.first() {
                        Some(crate::ast::CallArg::Expr(e)) if args.len() == 1 => e,
                        _ => return Ok(None),
                    };
                    return Ok(Some(cover_width_arg(group, point, &name.name, width_expr)?));
                }
            }
            Ok(None)
        }
        ExprKind::Int(s) => Ok(super::exprs::parse_int_literal(s).map(|v| {
            if v == 0 {
                1
            } else {
                64 - v.leading_zeros()
            }
        })),
        _ => Ok(None),
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
