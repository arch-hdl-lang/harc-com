//! Covergroup lowering: `CovergroupDecl` → `CovgroupSchema`.
//!
//! Scope mirrors what the tbir emitter reproduces byte-compatibly from
//! v1: clock-triggered (or trigger-less) covergroups whose points
//! sample a direct DUT port and whose bins are finite value sets and/or
//! inclusive ranges, plus declared `cross` items over those points.
//! Hook triggers stay `Unsupported` — never silently mis-lowered.

use super::{helpers::HelperRegistry, unsupported, LowerError};
use crate::ast::{
    CoverItem, CoverTrigger, CovergroupDecl, Expr as AstExpr, ExprKind, ExternFnDecl, UnaryOp,
};
use crate::ir::{
    CallTarget, CovBinBound, CovBinValue, CovTrigger, CoverBinSchema, CoverCrossSchema,
    CoverPointSchema, CovgroupSchema, Expr, IrType, PortAccess, PortRef, UnOp, WidthCastKind,
};
use std::collections::HashMap;

pub(crate) fn lower_covergroup(
    g: &CovergroupDecl,
    helpers: &HelperRegistry<'_>,
    extern_fns: &HashMap<String, &ExternFnDecl>,
    consts: &HashMap<String, u64>,
) -> Result<CovgroupSchema, LowerError> {
    // v1's clock-sample registration ignores the trigger expression
    // entirely — any non-hook covergroup samples once per primary-clock
    // tick. Mirror that: `@(posedge dut.clk)` and a missing trigger
    // both lower to `PosedgeDutClk`; hook triggers are post-MVP.
    //
    // For a hook trigger, also collect the trigger argument names (the
    // bare idents `t` in `@(drv.send(t) post)`). A coverpoint target may
    // sample `cover t.<field>` over the hook parameter, which lowers to a
    // record-field read on the hook-method's by-value closure arg rather
    // than a DUT port — `hook_params` is the in-scope set of such names.
    let mut hook_params: Vec<String> = Vec::new();
    let trigger = match &g.trigger {
        Some(CoverTrigger::Hook { call, side }) => {
            // Covergroup schemas lower before the testbench/transactor
            // tables exist, so the hook target cannot be resolved here.
            // Stash the receiver field-access path + method name; the
            // `covergroup_hooks` pass resolves it once transactors are
            // lowered (mirrors v1's late covergroup-hook registration).
            let (receiver_path, method) = lower_hook_call(&g.name.name, call)?;
            hook_params = hook_call_arg_names(&g.name.name, call)?;
            CovTrigger::Hook {
                receiver_path,
                method,
                param_names: hook_params.clone(),
                side: *side,
            }
        }
        Some(CoverTrigger::Clock(_)) | None => CovTrigger::PosedgeDutClk,
    };

    // Pass 1: points (a `cross` may reference a point declared after it).
    let mut points = Vec::new();
    for it in &g.items {
        if let CoverItem::Point(p) = it {
            let target = lower_point_target(
                &g.name.name,
                &p.name.name,
                &p.target,
                &hook_params,
                helpers,
                extern_fns,
                consts,
            )?;
            let mut bins = Vec::new();
            for b in &p.bins {
                bins.push(CoverBinSchema {
                    name: b.name.name.clone(),
                    values: lower_bin_values(
                        &g.name.name,
                        &b.name.name,
                        &b.spec,
                        consts,
                        &hook_params,
                        helpers,
                        extern_fns,
                    )?,
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

/// Collect the hook trigger argument names (`["t"]` for `drv.send(t)`).
/// The parser's `validate_cover_hook_trigger` already guarantees the
/// args are bare identifiers, so this is a straightforward extraction;
/// any non-ident is a parser/lowering invariant break, surfaced as a
/// clear unsupported error rather than silently dropped.
fn hook_call_arg_names(group: &str, call: &AstExpr) -> Result<Vec<String>, LowerError> {
    let ExprKind::Call { args, .. } = &*call.kind else {
        return Err(unsupported(
            &format!("covergroup `{group}` hook trigger must be a method call"),
            "",
        ));
    };
    let mut names = Vec::with_capacity(args.len());
    for arg in args {
        let crate::ast::CallArg::Expr(e) = arg else {
            return Err(unsupported(
                &format!("covergroup `{group}` hook trigger arguments must be identifiers"),
                "",
            ));
        };
        let ExprKind::Ident(id) = &*e.kind else {
            return Err(unsupported(
                &format!("covergroup `{group}` hook trigger arguments must be identifiers"),
                "",
            ));
        };
        names.push(id.name.clone());
    }
    Ok(names)
}

/// Recognize a hook-param cover target: `<param>` (scalar), `<param>.<field>`
/// (record field), or `<param>.<field>[const]` (a `Vec<T, N>` field element),
/// where `<param>` is one of the covergroup hook trigger's argument names.
/// Returns `Ok(None)` when the target is not rooted at a hook param so
/// the caller falls through to DUT-port / literal lowering. A nested
/// path (`t.a.b`) or a non-constant index is out of subset and errors.
fn lower_hook_param_field(
    group: &str,
    point: &str,
    target: &AstExpr,
    hook_params: &[String],
    consts: &HashMap<String, u64>,
) -> Result<Option<Expr>, LowerError> {
    if hook_params.is_empty() {
        return Ok(None);
    }
    let mut bare = target;
    while let ExprKind::Paren(inner) = &*bare.kind {
        bare = inner;
    }
    if let ExprKind::Ident(id) = &*bare.kind {
        if hook_params.iter().any(|p| p == &id.name) {
            return Ok(Some(Expr::CovHookArg {
                param: id.name.clone(),
            }));
        }
    }
    // Optional trailing `[const]` index over a record-field read.
    let mut cur = target;
    let mut index: Option<Box<Expr>> = None;
    if let ExprKind::Index {
        target: t,
        index: i,
    } = &*cur.kind
    {
        // Covergroup schemas lower before any runtime scope, so a Vec
        // lane index can only be a constant literal here (same rule as
        // DUT-port lanes). Represent it as a constant `Expr::Literal`.
        let v = cover_const_u64(group, point, i, consts)?;
        index = Some(Box::new(Expr::Literal {
            value: v,
            ty: IrType::Unknown,
        }));
        cur = t;
    }
    // Peel parens, then require exactly `<ident>.<field>`.
    while let ExprKind::Paren(inner) = &*cur.kind {
        cur = inner;
    }
    let ExprKind::Field { target: recv, name } = &*cur.kind else {
        return Ok(None);
    };
    let mut rcur: &AstExpr = recv;
    while let ExprKind::Paren(inner) = &*rcur.kind {
        rcur = inner;
    }
    let ExprKind::Ident(id) = &*rcur.kind else {
        // A deeper receiver path (`t.a.b`) is not in subset.
        return Ok(None);
    };
    if !hook_params.iter().any(|p| p == &id.name) {
        // Not a hook param — let DUT-port / literal lowering handle it.
        return Ok(None);
    }
    Ok(Some(Expr::CovHookParam {
        param: id.name.clone(),
        field: name.name.clone(),
        index,
    }))
}

/// A cover-point target is a pure DUT-bound scalar expression. Keep this
/// subset deliberately smaller than general expression lowering because
/// covergroup schemas lower before test/run scopes exist: no locals,
/// helper calls, regblock reads, or transactor edges.
///
/// Also serves bin range BOUNDS and, since the exact-value widening, bin
/// VALUES — which is where `point` becomes a misnomer: those callers
/// pass the BIN name, so a rejected `{dut.q}` reports "point `en0`" for
/// something that is a bin. Two follow-ups, deliberately not folded into
/// the widening:
///
///   * the label wants to be a caller-supplied phrase ("point `cp_en`" /
///     "bin `en0`"), which also means threading it through the nested
///     `cover_const_u32` / `cover_infer_expr_width` / `cover_width_arg`
///     helpers that build their own "point `{point}`" messages;
///   * `unsupported_target` is very likely mis-CLASSIFIED. v1 emits an
///     out-of-subset bin value verbatim — `_v == NOPE`,
///     `_v == undefined_fn(1)`, `_v == "x"` — none of which compiles, so
///     this looks like `EmitsUncompilable` rather than a v1 escape
///     hatch. It is shared across four landings (point targets, hook
///     params, bin bounds, bin values), and reclassifying a shared arm
///     without probing every one of them is exactly the mistake
///     divergences 43 and 44 record, so it wants its own slice.
fn lower_point_target(
    group: &str,
    point: &str,
    target: &AstExpr,
    hook_params: &[String],
    helpers: &HelperRegistry<'_>,
    extern_fns: &HashMap<String, &ExternFnDecl>,
    consts: &HashMap<String, u64>,
) -> Result<Expr, LowerError> {
    let unsupported_target = || {
        unsupported(
            &format!("covergroup `{group}` point `{point}` with an unsupported target expression"),
            "supported: dut.<port>, dut.<port>[idx], hook params, pure helper calls, expr[hi:lo], literals, unary/binary/ternary expressions",
        )
    };

    // A hook-param field read (`cover t.burst` / `cover t.data[idx]`): the
    // receiver ident names a hook trigger parameter, so the target samples
    // the hookable method's by-value argument record, not a DUT port. The
    // hook param has no resolvable `LocalId` here (transactors lower
    // later), so carry the param name + field; codegen renders it as the
    // closure arg `param.field` (mirrors v1 emitting `t.burst` over the
    // by-value closure param).
    if let Some(hp) = lower_hook_param_field(group, point, target, hook_params, consts)? {
        return Ok(hp);
    }

    fn lower_port(
        group: &str,
        point: &str,
        e: &AstExpr,
        consts: &HashMap<String, u64>,
    ) -> Result<Option<PortRef>, LowerError> {
        let mut cur = e;
        let mut lane = None;
        if let ExprKind::Index { target, index } = &*cur.kind {
            // Covergroup schemas lower before any test/run scope exists,
            // so a lane index has no runtime locals to reference: only a
            // constant is in subset (`cover_const_u64` rejects anything
            // else). Kept as `LaneIndex::Const`.
            lane = Some(crate::ir::LaneIndex::Const(cover_const_u64(
                group, point, index, consts,
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
                    segments.reverse();
                    return Ok(Some(PortRef {
                        testbench_field: "dut".to_string(),
                        port_path: segments,
                        aggregate_path: true,
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

    if let Some(port) = lower_port(group, point, target, consts)? {
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
                let inner = lower_point_target(
                    group,
                    point,
                    expr,
                    hook_params,
                    helpers,
                    extern_fns,
                    consts,
                )?;
                let op = match op {
                    UnaryOp::Neg => UnOp::Neg,
                    UnaryOp::Not | UnaryOp::NotKw => UnOp::Not,
                    UnaryOp::BitNot => UnOp::BitNot,
                };
                return Ok(Expr::Unary(op, Box::new(inner)));
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let op = super::exprs::lower_bin_op(*op)?;
                let lhs = lower_point_target(
                    group,
                    point,
                    lhs,
                    hook_params,
                    helpers,
                    extern_fns,
                    consts,
                )?;
                let rhs = lower_point_target(
                    group,
                    point,
                    rhs,
                    hook_params,
                    helpers,
                    extern_fns,
                    consts,
                )?;
                return Ok(Expr::Binary(op, Box::new(lhs), Box::new(rhs)));
            }
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => {
                let cond = lower_point_target(
                    group,
                    point,
                    cond,
                    hook_params,
                    helpers,
                    extern_fns,
                    consts,
                )?;
                let then_branch = lower_point_target(
                    group,
                    point,
                    then_branch,
                    hook_params,
                    helpers,
                    extern_fns,
                    consts,
                )?;
                let else_branch = lower_point_target(
                    group,
                    point,
                    else_branch,
                    hook_params,
                    helpers,
                    extern_fns,
                    consts,
                )?;
                return Ok(Expr::Ternary(
                    Box::new(cond),
                    Box::new(then_branch),
                    Box::new(else_branch),
                ));
            }
            ExprKind::BitSlice { target, hi, lo } => {
                let hi = cover_const_u32(group, point, hi, consts)?;
                let lo = cover_const_u32(group, point, lo, consts)?;
                if hi < lo {
                    return Err(LowerError::Invalid(format!(
                        "covergroup `{group}` point `{point}` has invalid bit slice [{hi}:{lo}]"
                    )));
                }
                let target = lower_point_target(
                    group,
                    point,
                    target,
                    hook_params,
                    helpers,
                    extern_fns,
                    consts,
                )?;
                return Ok(Expr::BitSlice {
                    target: Box::new(target),
                    hi,
                    lo,
                });
            }
            ExprKind::Cast { expr, ty } => {
                let width =
                    cover_cast_width(group, point, ty, consts)?.ok_or_else(unsupported_target)?;
                let inner = lower_point_target(
                    group,
                    point,
                    expr,
                    hook_params,
                    helpers,
                    extern_fns,
                    consts,
                )?;
                return Ok(Expr::WidthCast {
                    kind: WidthCastKind::Resize,
                    width,
                    src_width: cover_infer_expr_width(group, point, expr, consts)?,
                    inner: Box::new(inner),
                });
            }
            ExprKind::Call { callee, args } => {
                if let ExprKind::Ident(id) = &*callee.kind {
                    if let Some(entry) = helpers.get(&id.name) {
                        if !entry.pure {
                            return Err(unsupported(
                                &format!(
                                    "covergroup `{group}` point `{point}` impure helper call `{}`",
                                    id.name
                                ),
                                "only pure file-scope helper functions can be sampled in coverpoints",
                            ));
                        }
                        if args.len() != entry.decl.params.len() {
                            return Err(LowerError::Invalid(format!(
                                "covergroup `{group}` point `{point}` helper `{}` takes {} argument(s), call passes {}",
                                id.name,
                                entry.decl.params.len(),
                                args.len()
                            )));
                        }
                        let args = lower_point_call_args(
                            group,
                            point,
                            &id.name,
                            args,
                            hook_params,
                            helpers,
                            extern_fns,
                            consts,
                        )?;
                        return Ok(Expr::Call(CallTarget::Helper(id.name.clone()), args));
                    }
                    if let Some(decl) = extern_fns.get(&id.name) {
                        if args.len() != decl.params.len() {
                            return Err(LowerError::Invalid(format!(
                                "covergroup `{group}` point `{point}` extern function `{}` takes {} argument(s), call passes {}",
                                id.name,
                                decl.params.len(),
                                args.len()
                            )));
                        }
                        let args = lower_point_call_args(
                            group,
                            point,
                            &id.name,
                            args,
                            hook_params,
                            helpers,
                            extern_fns,
                            consts,
                        )?;
                        return Ok(Expr::Call(CallTarget::ExternFn(id.name.clone()), args));
                    }
                }
                if let ExprKind::Field { target: recv, name } = &*callee.kind {
                    if matches!(name.name.as_str(), "trunc" | "zext" | "sext" | "resize") {
                        // Width-method calls are parsed with the width as the first argument.
                        let width_expr = match args.first() {
                            Some(crate::ast::CallArg::Expr(e)) if args.len() == 1 => e,
                            _ => return Err(unsupported_target()),
                        };
                        let width = cover_width_arg(group, point, &name.name, width_expr, consts)?;
                        let src_width = cover_infer_expr_width(group, point, recv, consts)?;
                        let inner = lower_point_target(
                            group,
                            point,
                            recv,
                            hook_params,
                            helpers,
                            extern_fns,
                            consts,
                        )?;
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

// Eight parameters, one over the lint's threshold, and the last five are
// the same lowering context every function in this chain forwards
// unchanged (`hook_params`, `helpers`, `extern_fns`, `consts`). Bundling
// them into a context struct is the right cleanup and touches the whole
// file; it is deliberately not folded into a diagnostics change.
#[allow(clippy::too_many_arguments)]
fn lower_point_call_args(
    group: &str,
    point: &str,
    name: &str,
    args: &[crate::ast::CallArg],
    hook_params: &[String],
    helpers: &HelperRegistry<'_>,
    extern_fns: &HashMap<String, &ExternFnDecl>,
    consts: &HashMap<String, u64>,
) -> Result<Vec<Expr>, LowerError> {
    let mut lowered = Vec::with_capacity(args.len());
    for arg in args {
        match arg {
            crate::ast::CallArg::Expr(e) => lowered.push(lower_point_target(
                group,
                point,
                e,
                hook_params,
                helpers,
                extern_fns,
                consts,
            )?),
            crate::ast::CallArg::Named { .. } => {
                return Err(unsupported(
                    &format!("named arguments in covergroup helper call `{name}(...)`"),
                    "",
                ))
            }
        }
    }
    Ok(lowered)
}

/// A constant in a coverpoint target — a `Vec` lane index or a slice
/// bound. Covergroup schemas lower before any runtime scope exists, so
/// these reference no locals; a file-scope `const` or enum variant is
/// still fair game, and v1 emits one as `(uint32_t)(K)` against its own
/// `static constexpr K` — identical semantics to the literal. Verified
/// by mutating `cov_expr_targets_test`, which BOTH backends pass, one
/// bound at a time.
fn cover_const_u64(
    group: &str,
    point: &str,
    e: &AstExpr,
    consts: &HashMap<String, u64>,
) -> Result<u64, LowerError> {
    match &*e.kind {
        ExprKind::Paren(inner) => cover_const_u64(group, point, inner, consts),
        ExprKind::Ident(id) => consts.get(&id.name).copied().ok_or_else(|| {
            unsupported(
                &format!("covergroup `{group}` point `{point}` non-constant index/slice bound"),
                format!("`{}` is not a file-scope const or enum variant", id.name),
            )
        }),
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

fn cover_const_u32(
    group: &str,
    point: &str,
    e: &AstExpr,
    consts: &HashMap<String, u64>,
) -> Result<u32, LowerError> {
    let v = cover_const_u64(group, point, e, consts)?;
    u32::try_from(v).map_err(|_| {
        LowerError::Invalid(format!(
            "covergroup `{group}` point `{point}` constant {v} does not fit in u32"
        ))
    })
}

fn cover_width_arg(
    group: &str,
    point: &str,
    method: &str,
    e: &AstExpr,
    consts: &HashMap<String, u64>,
) -> Result<u32, LowerError> {
    let width = cover_const_u32(group, point, e, consts)?;
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
    consts: &HashMap<String, u64>,
) -> Result<Option<u32>, LowerError> {
    let width = match ty {
        crate::ast::TypeExpr::Builtin {
            name:
                crate::ast::BuiltinTy::UInt | crate::ast::BuiltinTy::SInt | crate::ast::BuiltinTy::Bits,
            args,
            ..
        } => match args.first() {
            Some(crate::ast::TypeArg::Expr(e)) => Some(cover_const_u32(group, point, e, consts)?),
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
    consts: &HashMap<String, u64>,
) -> Result<Option<u32>, LowerError> {
    match &*e.kind {
        ExprKind::Paren(inner) => cover_infer_expr_width(group, point, inner, consts),
        ExprKind::BitSlice { hi, lo, .. } => {
            let hi = cover_const_u32(group, point, hi, consts)?;
            let lo = cover_const_u32(group, point, lo, consts)?;
            if hi < lo {
                return Err(LowerError::Invalid(format!(
                    "covergroup `{group}` point `{point}` has invalid bit slice [{hi}:{lo}]"
                )));
            }
            Ok(Some(hi - lo + 1))
        }
        ExprKind::Cast { ty, .. } => cover_cast_width(group, point, ty, consts),
        ExprKind::Call { callee, args } => {
            if let ExprKind::Field { name, .. } = &*callee.kind {
                if matches!(name.name.as_str(), "trunc" | "zext" | "sext" | "resize") {
                    let width_expr = match args.first() {
                        Some(crate::ast::CallArg::Expr(e)) if args.len() == 1 => e,
                        _ => return Ok(None),
                    };
                    return Ok(Some(cover_width_arg(
                        group, point, &name.name, width_expr, consts,
                    )?));
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
///   `{v}` / `{a, b, c}`  — set literals
///   `v`                  — a bare member
///   `[a..b]`             — inclusive range; either bound may be open
///   `{[1..3], 7}`        — set-of-ranges (recursion)
///   `(inner)`            — parenthesized recursion
///
/// A member or bound may be a compile-time constant (an integer
/// literal, a file-scope `const`, an enum variant) or a genuine RUNTIME
/// expression over the sampler subset — a DUT port, a hook param, a
/// pure helper call, or arithmetic over those. Both forms carry a
/// [`CovBinBound`], and both go through `lower_bin_bound`, so a member
/// and a range end accept exactly the same thing.
pub(crate) fn lower_bin_values(
    group: &str,
    bin: &str,
    spec: &AstExpr,
    consts: &HashMap<String, u64>,
    hook_params: &[String],
    helpers: &HelperRegistry<'_>,
    extern_fns: &HashMap<String, &ExternFnDecl>,
) -> Result<Vec<CovBinValue>, LowerError> {
    match &*spec.kind {
        ExprKind::SetLit(items) => {
            let mut values = Vec::with_capacity(items.len());
            for it in items {
                values.extend(lower_bin_values(
                    group,
                    bin,
                    it,
                    consts,
                    hook_params,
                    helpers,
                    extern_fns,
                )?);
            }
            Ok(values)
        }
        ExprKind::Paren(inner) => {
            lower_bin_values(group, bin, inner, consts, hook_params, helpers, extern_fns)
        }
        ExprKind::RangeLit { lo, hi } => {
            let end = |e: &AstExpr| {
                lower_bin_bound(group, bin, e, consts, hook_params, helpers, extern_fns)
            };
            let lo = lo.as_ref().map(&end).transpose()?;
            let hi = hi.as_ref().map(end).transpose()?;
            Ok(vec![CovBinValue::Range { lo, hi }])
        }
        // Every other element is an exact value, and takes the SAME
        // path a range bound does. A literal or a file-scope const name
        // folds; anything else (`{dut.en}`, `{dut.en + 1}`, a hook
        // param, a pure helper call) becomes a runtime expression
        // compared per-sample, which is what v1 has always done here —
        // it emits `_v == harc_rt::harc_read(dut->en)` and the bin
        // counts correctly.
        //
        // Ranges got runtime bounds first and exact values were left
        // behind; there was never a reason for the two to differ, since
        // v1 renders both with the same `emit_expr`. `lower_bin_bound`
        // is the shared implementation, so every member and every range
        // end accepts exactly the same thing and fails the same way.
        _ => Ok(vec![CovBinValue::Eq(lower_bin_bound(
            group,
            bin,
            spec,
            consts,
            hook_params,
            helpers,
            extern_fns,
        )?)]),
    }
}

/// Rewrite `lower_point_target`'s "point `X`" as "bin `X`".
///
/// That function serves coverpoint targets, hook params, bin bounds and
/// bin values, but names only the first in its diagnostics, so a
/// rejected bin spec reports a "point" the source never declared. The
/// real fix is a caller-supplied label threaded through it and the
/// nested `cover_const_u32` / `cover_infer_expr_width` / `cover_width_arg`
/// helpers that build their own messages — see the note on
/// `lower_point_target`. This keeps the user-visible noun honest until
/// then, and is a no-op on any message that does not contain the phrase.
fn relabel_point_as_bin(e: LowerError, bin: &str) -> LowerError {
    let from = format!("point `{bin}`");
    let to = format!("bin `{bin}`");
    match e {
        LowerError::Unsupported { construct, detail } => LowerError::Unsupported {
            construct: construct.replace(&from, &to),
            detail,
        },
        LowerError::NotImplemented {
            construct,
            detail,
            v1,
        } => LowerError::NotImplemented {
            construct: construct.replace(&from, &to),
            detail,
            v1,
        },
        LowerError::Invalid(m) => LowerError::Invalid(m.replace(&from, &to)),
    }
}

/// Lower ONE bin member or range end — the two are the same thing, and
/// this is the single implementation of both, so `{x}` and `[x .. y]`
/// accept the same shapes and fail the same way.
///
/// A plain integer literal or file-scope const/enum variant folds to
/// `CovBinBound::Const`. A genuine runtime value (a DUT port, hook
/// param, or any expression over those) lowers via `lower_point_target`
/// — the same sampler-subset expression lowerer used for point targets —
/// to `CovBinBound::Runtime`, mirroring v1, which emits the raw
/// expression per sample.
fn lower_bin_bound(
    group: &str,
    bin: &str,
    e: &AstExpr,
    consts: &HashMap<String, u64>,
    hook_params: &[String],
    helpers: &HelperRegistry<'_>,
    extern_fns: &HashMap<String, &ExternFnDecl>,
) -> Result<CovBinBound, LowerError> {
    match &*e.kind {
        ExprKind::Paren(inner) => {
            lower_bin_bound(group, bin, inner, consts, hook_params, helpers, extern_fns)
        }
        ExprKind::Int(s) => Ok(CovBinBound::Const(parse_bound(group, bin, s)?)),
        // A bare ident that names a file-scope const/enum variant folds
        // to a constant — but a HOOK PARAM of the same name wins, which
        // is the precedence a coverpoint target already uses. Without
        // this order, adding an unrelated `const cmd = 1` silently turns
        // `one = {cmd}` into `_v == 1` while `cover cmd.ticks` in the
        // same covergroup still means the hook argument: one name, two
        // meanings, no diagnostic.
        ExprKind::Ident(id) if consts.contains_key(&id.name) && !hook_params.contains(&id.name) => {
            Ok(CovBinBound::Const(consts[&id.name]))
        }
        // A bare name that is neither is a TYPO, and gets a message that
        // says so rather than `lower_point_target`'s generic subset
        // list.
        ExprKind::Ident(id) if !hook_params.contains(&id.name) => Err(unsupported(
            &format!("covergroup `{group}` bin `{bin}` with a non-constant spec"),
            format!(
                "`{}` is not a file-scope const, enum variant, or hook parameter",
                id.name
            ),
        )),
        _ => {
            // Reuse the point-target expression lowerer for the runtime
            // case so the emitted per-sample value matches v1 (v1 renders
            // it with the same `emit_expr` it uses for point targets). It
            // rejects anything outside the sampler subset with a precise
            // diagnostic — but names it a "point", so relabel: the source
            // declared a bin.
            let expr = lower_point_target(group, bin, e, hook_params, helpers, extern_fns, consts)
                .map_err(|err| relabel_point_as_bin(err, bin))?;
            Ok(CovBinBound::Runtime(expr))
        }
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
