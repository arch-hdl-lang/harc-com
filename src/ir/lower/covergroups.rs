//! Covergroup lowering: `CovergroupDecl` → `CovgroupSchema`.
//!
//! Scope mirrors what the tbir emitter reproduces byte-compatibly from
//! v1: clock-triggered (or trigger-less) covergroups whose points
//! sample a direct DUT port and whose bins are finite value sets and/or
//! inclusive ranges, plus declared `cross` items over those points.
//! Hook triggers stay `Unsupported` — never silently mis-lowered.

use super::{
    fold_const, helpers::HelperRegistry, not_implemented, unsupported, ConstFoldErr, ConstVal,
    LowerError, V1Status,
};
use crate::ast::{
    BinaryOp, CoverItem, CoverTrigger, CovergroupDecl, Expr as AstExpr, ExprKind, ExternFnDecl,
    UnaryOp,
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
    consts: &HashMap<String, ConstVal>,
    ambiguous_variants: &HashMap<String, String>,
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

    // harc#666: an enum-variant name declared by more than one enum has
    // no correct index as a VALUE. `consts` folded it first-wins, so a
    // bin bound or coverpoint target naming one would silently compare
    // against the wrong enum's index — `{OKAY}` sampling `_v == 0` when
    // `WrResp.OKAY` is 1, with no diagnostic and (since v1 rejects it) a
    // v1/tbir divergence. Reject a value use here, the same as the
    // general expression path does, so the two backends agree.
    //
    // Every value expression a covergroup can hold is a coverpoint
    // `target` or a bin `spec`; validate both up front rather than
    // threading the map through the recursive point-target lowerer and
    // its ~18 call sites. A hook parameter of the same name shadows the
    // variant (bins defer to it — see `mentions_hook_param`), so a name
    // in `hook_params` is skipped, matching that precedence.
    for it in &g.items {
        if let CoverItem::Point(p) = it {
            reject_ambiguous_variant_use(
                &g.name.name,
                &format!("coverpoint `{}`", p.name.name),
                &p.target,
                ambiguous_variants,
                &hook_params,
            )?;
            for b in &p.bins {
                reject_ambiguous_variant_use(
                    &g.name.name,
                    &format!("bin `{}`", b.name.name),
                    &b.spec,
                    ambiguous_variants,
                    &hook_params,
                )?;
            }
        }
    }

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
        // PARSER-GUARDED, so unreachable: `validate_cover_hook_trigger`
        // refuses a non-call trigger first, with "covergroup hook
        // trigger must be a method call before `pre` or `post`" —
        // measured with `@(drv.step post)`, which never reaches
        // lowering. Kept as an invariant guard, and `Invalid` rather
        // than a v1 suggestion because if it ever did fire the program
        // would be malformed, not merely outside TB-IR's subset.
        return Err(LowerError::Invalid(format!(
            "covergroup `{group}` hook trigger must be a method call"
        )));
    };
    let ExprKind::Field { target, name } = &*callee.kind else {
        // REACHABLE — the parser's `validate_cover_hook_trigger` checks
        // that the trigger is a CALL and that its args are bare
        // identifiers, but not that the callee is a field access. So a
        // bare `@(step(n) post)` lands here.
        //
        // v1's matching refusal ("must resolve to a `hookable` on a
        // known component type") comes from
        // `emit_covergroup_hook_sample_registration`, which runs at the
        // INSTANTIATION site — per `cov : StepCov` field or `let cov :
        // G`. A covergroup that is declared and never instantiated
        // never reaches it, and v1 emits the whole testbench: measured
        // on `covergroup_hook_trigger_test` with the `cov` field and
        // its readers removed, 298 lines out and g++ `-fsyntax-only`
        // clean. TB-IR refuses at DECLARATION, which is why the two
        // disagree.
        //
        // So this is a subset gap with a working escape hatch, not a
        // program error. The first version measured only the
        // instantiated position and called it `Invalid` — the same
        // mistake made on `connect` and on the built-in predicates: an
        // arm is only `Invalid` if NO backend runs it in ANY reachable
        // configuration, and "nothing instantiates it" is one.
        return Err(unsupported(
            &format!("covergroup `{group}` hook trigger `<name>(args)` without a receiver"),
            "write `<obj>.<method>(args)`; v1 only checks the trigger where the covergroup \
             is instantiated, so an uninstantiated one builds under `--codegen v1`",
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
                // REACHABLE, and the second of exactly two arms in this
                // function that are: `@((drv.x + 1).step(n) post)` has a
                // callee that IS a field access, so the arm above lets
                // it through, and a receiver that is not a path.
                //
                // Same instantiation-site split as the arm above, and
                // measured the same way: v1's refusal only fires where
                // the covergroup is instantiated.
                return Err(unsupported(
                    &format!("covergroup `{group}` hook trigger receiver"),
                    // NOT "`drv` or `env.drv`", which the first
                    // version suggested: a nested receiver is refused
                    // by `covergroup_hooks` ("nested receiver paths are
                    // not supported by the tbir backend") and by
                    // `tbir::emit`'s single-segment rule. Advice has to
                    // be something the compiler accepts.
                    "the receiver must be a single component name (`drv`); v1 only checks \
                     the trigger where the covergroup is instantiated, so an uninstantiated \
                     one builds under `--codegen v1`",
                ));
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
        // PARSER-GUARDED, same as the sibling in `lower_hook_call`.
        return Err(LowerError::Invalid(format!(
            "covergroup `{group}` hook trigger must be a method call"
        )));
    };
    let mut names = Vec::with_capacity(args.len());
    for arg in args {
        // PARSER-GUARDED, both of these: `validate_cover_hook_trigger`
        // walks the same args and refuses a non-identifier first —
        // measured with `@(drv.step(n + 1) post)`, which reports the
        // parser's own "covergroup hook trigger arguments must be
        // identifiers" and never reaches lowering.
        let crate::ast::CallArg::Expr(e) = arg else {
            return Err(LowerError::Invalid(format!(
                "covergroup `{group}` hook trigger arguments must be identifiers"
            )));
        };
        let ExprKind::Ident(id) = &*e.kind else {
            return Err(LowerError::Invalid(format!(
                "covergroup `{group}` hook trigger arguments must be identifiers"
            )));
        };
        names.push(id.name.clone());
    }
    Ok(names)
}

/// Recognize a hook-param cover target: `<param>` (scalar), `<param>.<field>`
/// (record field), or `<param>.<field>[index]` (a `Vec<T, N>` field element),
/// where `<param>` is one of the covergroup hook trigger's argument names.
/// Returns `Ok(None)` when the target is not rooted at a hook param so
/// the caller falls through to DUT-port / literal lowering. A nested
/// field path (`t.a.b`) remains out of subset; an index may itself be a
/// supported per-sample cover expression.
fn lower_hook_param_field(
    group: &str,
    point: &str,
    target: &AstExpr,
    hook_params: &[String],
    helpers: &HelperRegistry<'_>,
    extern_fns: &HashMap<String, &ExternFnDecl>,
    consts: &HashMap<String, ConstVal>,
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
        let lowered =
            match cover_const_u64(group, point, ConstRole::LaneIndex, i, consts, hook_params) {
                Ok(Some(value)) => Expr::Literal {
                    value,
                    ty: IrType::Unknown,
                },
                Ok(None) => {
                    lower_point_target(group, point, i, hook_params, helpers, extern_fns, consts)?
                }
                Err(err) => return Err(err),
            };
        index = Some(Box::new(lowered));
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

fn cover_wrap_operand_width(expr: &AstExpr) -> Option<u32> {
    match &*expr.kind {
        ExprKind::Paren(inner) => cover_wrap_operand_width(inner),
        ExprKind::Cast { ty, .. } => cast_literal_width(ty),
        ExprKind::BitSlice { hi, lo, .. } => {
            let hi = literal_width(hi)?;
            let lo = literal_width(lo)?;
            (hi >= lo).then_some(hi - lo + 1)
        }
        ExprKind::Call { callee, args } => {
            let ExprKind::Field { name, .. } = &*callee.kind else {
                return None;
            };
            if !matches!(name.name.as_str(), "trunc" | "zext" | "sext" | "resize") {
                return None;
            }
            let [crate::ast::CallArg::Expr(width)] = args.as_slice() else {
                return None;
            };
            literal_width(width)
        }
        ExprKind::Binary { op, lhs, rhs }
            if matches!(
                op,
                BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap
            ) =>
        {
            Some(cover_wrap_operand_width(lhs)?.max(cover_wrap_operand_width(rhs)?))
        }
        ExprKind::Int(lit) => super::exprs::parse_int_literal(lit).map(|value| {
            if value == 0 {
                1
            } else {
                64 - value.leading_zeros()
            }
        }),
        ExprKind::Bool(_) => Some(1),
        _ => None,
    }
}

/// A cover-point target is a pure sampled scalar expression. Keep this
/// subset deliberately smaller than general expression lowering because
/// covergroup schemas lower before test/run scopes exist: no run locals,
/// regblock reads, or transactor edges. Pure file-scope helpers and hook
/// parameters are available because they do not depend on a run scope.
///
/// Also serves bin range BOUNDS and bin VALUES, so a rejection here
/// reaches four landings. Out-of-subset expressions are classified by
/// `classify_out_of_subset_target` rather than by a single blanket arm —
/// v1 does four different things to them.
///
/// One follow-up remains: the `point` label is a misnomer for the bin
/// callers, which pass the BIN name. `lower_bin_bound` relabels the
/// message on the way out; the real fix threads a caller-supplied
/// phrase through the nested `cover_const_u32` /
/// `cover_infer_expr_width` / `cover_width_arg` helpers, which build
/// their own "point `{point}`" messages.
fn lower_point_target(
    group: &str,
    point: &str,
    target: &AstExpr,
    hook_params: &[String],
    helpers: &HelperRegistry<'_>,
    extern_fns: &HashMap<String, &ExternFnDecl>,
    consts: &HashMap<String, ConstVal>,
) -> Result<Expr, LowerError> {
    // Takes the node the walk FAILED on, not the top-level `target`.
    // The loop below unwraps `Paren` as it descends, so a closure over
    // `target` would classify `cover ([1..2])` by the parenthesis and
    // miss every arm — the range case included, which is the one this
    // classifier exists for.
    let unsupported_target =
        |at: &AstExpr| classify_out_of_subset_target(group, point, at, helpers, extern_fns);

    // A hook-param field read (`cover t.burst` / `cover t.data[idx]`): the
    // receiver ident names a hook trigger parameter, so the target samples
    // the hookable method's by-value argument record, not a DUT port. The
    // hook param has no resolvable `LocalId` here (transactors lower
    // later), so carry the param name + field; codegen renders it as the
    // closure arg `param.field` (mirrors v1 emitting `t.burst` over the
    // by-value closure param).
    if let Some(hp) = lower_hook_param_field(
        group,
        point,
        target,
        hook_params,
        helpers,
        extern_fns,
        consts,
    )? {
        return Ok(hp);
    }

    fn lower_port(
        group: &str,
        point: &str,
        e: &AstExpr,
        consts: &HashMap<String, ConstVal>,
        hook_params: &[String],
        helpers: &HelperRegistry<'_>,
        extern_fns: &HashMap<String, &ExternFnDecl>,
    ) -> Result<Option<PortRef>, LowerError> {
        let mut cur = e;
        let mut lane = None;
        if let ExprKind::Index { target, index } = &*cur.kind {
            // A constant stays compact in the port handle. A per-sample
            // DUT/hook expression reuses the existing runtime lane-index
            // variant, exactly like a general expression-position port.
            lane = Some(
                match cover_const_u64(
                    group,
                    point,
                    ConstRole::LaneIndex,
                    index,
                    consts,
                    hook_params,
                ) {
                    Ok(Some(value)) => crate::ir::LaneIndex::Const(value),
                    Ok(None) => {
                        let index = lower_point_target(
                            group,
                            point,
                            index,
                            hook_params,
                            helpers,
                            extern_fns,
                            consts,
                        )?;
                        crate::ir::LaneIndex::Var(Box::new(index))
                    }
                    Err(err) => return Err(err),
                },
            );
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

    if let Some(port) = lower_port(
        group,
        point,
        target,
        consts,
        hook_params,
        helpers,
        extern_fns,
    )? {
        return Ok(Expr::Port(port));
    }

    let mut cur = target;
    loop {
        match &*cur.kind {
            ExprKind::Paren(inner) => cur = inner,
            ExprKind::Int(s) => {
                let (value, ty) = if let Some(value) = super::exprs::parse_int_literal(s) {
                    (value, IrType::Unknown)
                } else if let Some((width, value)) =
                    super::exprs::parse_sized_int_literal_with_width(s)
                {
                    (value, IrType::UInt(Some(width)))
                } else {
                    return Err(unsupported_target(cur));
                };
                return Ok(Expr::Literal { value, ty });
            }
            // A file-scope `const` / enum variant sampled directly.
            // Degenerate — the coverpoint reads the same value forever —
            // but legal, and v1 supports it: it emits its own
            // `static constexpr uint64_t K = 7;` and samples
            // `(uint64_t)(K)`, which compiles and is correct. So the gap
            // is closed rather than classified, folding through the same
            // table `lower_bin_bound` already uses for a const bin
            // member.
            ExprKind::Ident(id) if consts.contains_key(&id.name) => {
                return Ok(Expr::Literal {
                    value: consts[&id.name].bits,
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
                if matches!(
                    op,
                    BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap
                ) {
                    let symbol = match op {
                        BinaryOp::AddWrap => "+%",
                        BinaryOp::SubWrap => "-%",
                        BinaryOp::MulWrap => "*%",
                        _ => unreachable!(),
                    };
                    let lhs_width = cover_wrap_operand_width(lhs);
                    let rhs_width = cover_wrap_operand_width(rhs);
                    if let (Some(lhs_width), Some(rhs_width)) = (lhs_width, rhs_width) {
                        let width = lhs_width.max(rhs_width);
                        if width > 64 {
                            return Err(not_implemented(
                                &format!(
                                    "covergroup `{group}` point `{point}` wrapping operator \
                                     `{symbol}` at width {width}"
                                ),
                                "v1 rejects wrapping arithmetic above 64 bits because its \
                                 wrapping mask is limited to a uint64_t carrier",
                                V1Status::Rejects,
                            ));
                        }

                        let ir_op = super::exprs::lower_bin_op(*op)?;
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
                        return Ok(Expr::WidthCast {
                            kind: WidthCastKind::Trunc,
                            width,
                            src_width: None,
                            inner: Box::new(Expr::Binary(ir_op, Box::new(lhs), Box::new(rhs))),
                        });
                    }
                    return Err(not_implemented(
                        &format!(
                            "covergroup `{group}` point `{point}` wrapping operator `{symbol}`"
                        ),
                        "v1 rejects this form because it cannot establish a supported wrapping width",
                        V1Status::Rejects,
                    ));
                }
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
                let target = lower_point_target(
                    group,
                    point,
                    target,
                    hook_params,
                    helpers,
                    extern_fns,
                    consts,
                )?;
                let bound = |e: &AstExpr| match cover_const_u32(
                    group,
                    point,
                    ConstRole::SliceBound,
                    e,
                    consts,
                    hook_params,
                ) {
                    Ok(Some(value)) => Ok(Ok(value)),
                    Ok(None) => {
                        let expr = lower_point_target(
                            group,
                            point,
                            e,
                            hook_params,
                            helpers,
                            extern_fns,
                            consts,
                        )?;
                        Ok(Err(expr))
                    }
                    Err(err) => Err(err),
                };
                let hi = bound(hi)?;
                let lo = bound(lo)?;
                return match (hi, lo) {
                    (Ok(hi), Ok(lo)) => {
                        if hi < lo {
                            return Err(LowerError::Invalid(format!(
                                "covergroup `{group}` point `{point}` has invalid bit slice [{hi}:{lo}]"
                            )));
                        }
                        Ok(Expr::BitSlice {
                            target: Box::new(target),
                            hi,
                            lo,
                        })
                    }
                    (hi, lo) => Ok(Expr::BitSliceDyn {
                        target: Box::new(target),
                        hi: Box::new(match hi {
                            Ok(value) => Expr::Literal {
                                value: value.into(),
                                ty: IrType::Unknown,
                            },
                            Err(expr) => expr,
                        }),
                        lo: Box::new(match lo {
                            Ok(value) => Expr::Literal {
                                value: value.into(),
                                ty: IrType::Unknown,
                            },
                            Err(expr) => expr,
                        }),
                    }),
                };
            }
            ExprKind::Cast { expr, ty } => {
                let width = cover_cast_width(group, point, ty, consts, hook_params)?
                    .ok_or_else(|| unsupported_target(cur))?;
                let inner = lower_point_target(
                    group,
                    point,
                    expr,
                    hook_params,
                    helpers,
                    extern_fns,
                    consts,
                )?;
                let signed = matches!(
                    ty,
                    crate::ast::TypeExpr::Builtin {
                        name: crate::ast::BuiltinTy::SInt | crate::ast::BuiltinTy::SIntCap,
                        ..
                    }
                );
                return Ok(Expr::WidthCast {
                    // Like the general expression path, `as sint<W>` is
                    // a signedness relabel rather than a value-changing
                    // sign extension. `Sext` carries the signed marker;
                    // `src_width = W` prevents the emitter from filling.
                    kind: if signed {
                        WidthCastKind::Sext
                    } else {
                        WidthCastKind::Resize
                    },
                    width,
                    src_width: if signed {
                        Some(width)
                    } else {
                        cover_infer_expr_width(group, point, expr, consts, hook_params)?
                    },
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
                        let declared: Vec<String> = entry
                            .decl
                            .params
                            .iter()
                            .map(|p| p.name.name.clone())
                            .collect();
                        let args = lower_point_call_args(
                            group,
                            point,
                            &id.name,
                            args,
                            &declared,
                            hook_params,
                            helpers,
                            extern_fns,
                            consts,
                        )?;
                        return Ok(Expr::Call(
                            CallTarget::Helper {
                                name: id.name.clone(),
                                ret: super::helpers::ir_type_of_with_records(
                                    entry.decl.return_ty.as_ref(),
                                    &HashMap::new(),
                                ),
                            },
                            args,
                        ));
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
                        let declared: Vec<String> =
                            decl.params.iter().map(|p| p.name.name.clone()).collect();
                        let args = lower_point_call_args(
                            group,
                            point,
                            &id.name,
                            args,
                            &declared,
                            hook_params,
                            helpers,
                            extern_fns,
                            consts,
                        )?;
                        return Ok(Expr::Call(
                            CallTarget::ExternFn {
                                name: id.name.clone(),
                                ret: super::helpers::slot_ir_type(
                                    decl.return_ty.as_ref(),
                                    &HashMap::new(),
                                ),
                            },
                            args,
                        ));
                    }
                }
                if let ExprKind::Field { target: recv, name } = &*callee.kind {
                    if matches!(name.name.as_str(), "trunc" | "zext" | "sext" | "resize") {
                        // Width-method calls are parsed with the width as the first argument.
                        // Its own message rather than the out-of-subset
                        // classifier: the construct IS a width method, it
                        // just has the wrong arguments, and a program error
                        // under every backend is `Invalid`.
                        let width_expr = match args.first() {
                            Some(crate::ast::CallArg::Expr(e)) if args.len() == 1 => e,
                            // Names the actual problem: a NAMED argument
                            // is the other way to get here, and reporting
                            // "got 1" for `.trunc(w = 1)` would read as a
                            // contradiction.
                            _ => {
                                let got = if args.len() == 1 {
                                    "a named argument".to_string()
                                } else {
                                    format!("{} arguments", args.len())
                                };
                                return Err(LowerError::Invalid(format!(
                                    "covergroup `{group}` point `{point}` `.{}()` takes exactly \
                                     one positional width argument, got {got}",
                                    name.name
                                )));
                            }
                        };
                        // Source width FIRST. It is what the direction
                        // check below rests on, and computing it second
                        // meant a receiver TB-IR cannot model — an
                        // `as uint<128>` — was reported as a problem
                        // with the width ARGUMENT. Now the cast names
                        // itself, and `--codegen v1` takes the user to
                        // v1's own direction error if there is one.
                        let src_width =
                            cover_infer_expr_width(group, point, recv, consts, hook_params)?;
                        let width = cover_width_arg(
                            group,
                            point,
                            &name.name,
                            src_width,
                            width_expr,
                            consts,
                            hook_params,
                        )?;
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
                        // (The direction check lives in
                        // `cover_width_arg` now, so it runs for every
                        // width rather than only the ones that get past
                        // the 64-bit refusal.)
                        return Ok(Expr::WidthCast {
                            kind,
                            width,
                            src_width,
                            inner: Box::new(inner),
                        });
                    }
                }
                return Err(unsupported_target(cur));
            }
            _ => return Err(unsupported_target(cur)),
        }
    }
}

/// Classify an expression the coverpoint/bin subset does not lower, by
/// what `--codegen v1` actually does with it.
///
/// This was one blanket `Unsupported` arm — "re-run with `--codegen v1`"
/// for everything. Probing every `ExprKind` the arm can receive, at all
/// four landings it serves (point target, hook param, bin range bound,
/// bin exact value), found **four** different v1 behaviours, and only
/// one of them makes v1 an escape hatch. It is not the common one.
///
/// v1 has no subset gate here at all: it renders the expression with
/// `emit_expr` and casts the result to `uint64_t`, so whatever the user
/// wrote lands in the sampler verbatim. What varies is whether that
/// text compiles, and whether it means anything.
///
/// Every claim below was compile-tested against the real runtime header
/// rather than a stand-in — `dut.en.nope()` looks like it compiles until
/// `harc_read`'s actual return type is in scope.
fn classify_out_of_subset_target(
    group: &str,
    point: &str,
    e: &AstExpr,
    helpers: &HelperRegistry<'_>,
    extern_fns: &HashMap<String, &ExternFnDecl>,
) -> LowerError {
    let what = format!("covergroup `{group}` point `{point}`");
    let subset = "supported: dut.<port>, dut.<port>[idx], hook params, pure helper calls, \
                  expr[hi:lo], literals, unary/binary/ternary expressions";

    match &*e.kind {
        // Defensive unwrap. `lower_point_target` descends through
        // parentheses before failing, so it hands over the inner node
        // already — but a caller that does not would otherwise classify
        // `([1..2])` by the parenthesis and reach the catch-all, losing
        // every specific arm below.
        ExprKind::Paren(inner) => {
            classify_out_of_subset_target(group, point, inner, helpers, extern_fns)
        }

        // A literal `parse_int_literal` cannot read — a Verilog-sized
        // literal (`32'h18`) or one over 64 bits. TB-IR does not lower
        // either in a coverpoint, while v1's `c_int_literal` handles a
        // BARE sized one correctly, so this is the one arm here where
        // `--codegen v1` is a real escape hatch. Same split, and the
        // same reason, as the addrmap/regblock address folds
        // (divergence 44).
        //
        // The escape hatch is for the SIZED case only, so the arm splits
        // on the same `'` the address site uses. An over-wide literal
        // reaches here too and v1 does the opposite with it: a
        // `_harc_u128` composite that narrows — `0x10000000000000000`
        // samples 0 — so pointing there would hand the user a coverpoint
        // that reads zero forever.
        //
        // The previous version documented this split in a comment and
        // still returned one classification for both.
        ExprKind::Int(lit) if lit.contains('\'') => not_implemented(
            &format!("{what} sampling the sized literal `{lit}`"),
            "the sized literal's value is wider than the 64-bit cover sample; v1 narrows it",
            V1Status::SilentlyMisLowers,
        ),
        ExprKind::Int(lit) => not_implemented(
            &format!("{what} sampling the over-wide integer literal `{lit}`"),
            "a coverpoint samples 64 bits; v1 emits a `_harc_u128` composite that narrows, so \
             the point would sample a truncated value with no diagnostic",
            V1Status::SilentlyMisLowers,
        ),

        // ── v1 compiles it and SAMPLES THE WRONG THING ───────────────
        // The worst outcome, so it sets the classification wherever it
        // is reachable. A range in scalar position is the load-bearing
        // case: v1's own emitter leaves the comment admitting it dropped
        // the bounds.
        //
        // (`{[1..2]}` — a range NESTED in a set — never reaches here.
        // `lower_bin_values` has its own `RangeLit` arm and lowers it
        // correctly today. Only a range in SCALAR position lands here.)
        ExprKind::RangeLit { .. } => not_implemented(
            &format!("{what} sampling a range"),
            "v1 emits `(uint64_t)(/* range a..b */ 0)` — the coverpoint samples 0 on every \
             cycle, with no diagnostic. Sample a scalar and put the range in `bins` instead",
            V1Status::SilentlyMisLowers,
        ),
        ExprKind::Float(_) => not_implemented(
            &format!("{what} sampling a float literal"),
            "a coverpoint samples a 64-bit integer; v1 casts the literal, so the sample is \
             the truncated value rather than the one written",
            V1Status::SilentlyMisLowers,
        ),
        // Position-dependent, and classified by the WORSE half: in a bin
        // value or range bound v1 emits `_v == "x"`, which g++ rejects as
        // a pointer/int comparison; in a TARGET the C-style cast
        // reinterprets the pointer, so `(uint64_t)("x")` compiles and
        // samples an address.
        ExprKind::String(_) => not_implemented(
            &format!("{what} sampling a string literal"),
            "a coverpoint samples a 64-bit integer; in a target position v1 reinterprets the \
             pointer and samples an address, in a bin it emits a pointer/int comparison that \
             does not compile",
            V1Status::SilentlyMisLowers,
        ),

        // A system function in its real spelling (`$clog2(x)`). v1
        // rejects it outright — "expression not supported in v0 cpp_tb".
        //
        // Worth recording how this was nearly mis-classified: probed as
        // `clog2(dut.en)`, without the `$`, it parses as a plain call to
        // an undefined function, which v1 emits verbatim and g++
        // rejects. That made it look like an EmitsUncompilable gap in
        // legitimate surface. It is neither — the wrong spelling was
        // being probed, and the construct it actually names is one v1
        // refuses.
        ExprKind::SystemCall { name, .. } => {
            // The SOURCE spelling, not the Rust variant: a user who
            // wrote `$clog2` should not be told about `Clog2`.
            let spelled = match name {
                crate::ast::SystemFn::Rose => "$rose",
                crate::ast::SystemFn::Fell => "$fell",
                crate::ast::SystemFn::Stable => "$stable",
                crate::ast::SystemFn::Past => "$past",
                crate::ast::SystemFn::Clog2 => "$clog2",
            };
            not_implemented(
                &format!("{what} sampling the system function `{spelled}`"),
                format!("v1 rejects it as well. {subset}"),
                V1Status::Rejects,
            )
        }
        // Defensive: `.field` parses as `Field { target: ImplicitSelf }`
        // and is classified by the arm below, so a bare `ImplicitSelf`
        // is not reachable from a coverpoint position today. Kept with
        // the same status the reachable spelling has.
        ExprKind::ImplicitSelf => not_implemented(
            &format!("{what} sampling the implicit-self shorthand `.field`"),
            "v1 emits `(uint64_t)(.field)`, which is not an expression",
            V1Status::EmitsUncompilable,
        ),
        // A field or index path that is neither a DUT port nor a hook
        // param read: the earlier arms already declined it, so the root
        // names nothing v1 declares either.
        ExprKind::Field { .. } | ExprKind::Index { .. } | ExprKind::Ident(_) => not_implemented(
            &format!("{what} sampling a name that is not a DUT port or hook parameter"),
            format!(
                "v1 emits the path verbatim (`(uint64_t)(foo.bar)`), naming a symbol it never \
                 declares, so the generated C++ does not compile. {subset}"
            ),
            V1Status::EmitsUncompilable,
        ),
        // A call that reached here is not a pure helper, an extern fn, or
        // a width method. A method call on a sampled value
        // (`dut.en.nope()`) becomes a member access on `harc_read`'s
        // return type; a bare unknown callee becomes an undeclared
        // function. Both fail to compile.
        ExprKind::Call { callee, .. } => {
            let detail = match &*callee.kind {
                ExprKind::Field { name, .. } => format!(
                    "`.{}()` is not a width method (`trunc`/`zext`/`sext`/`resize`); v1 emits it \
                     as a member call on the sampled value, which has no such member",
                    name.name
                ),
                ExprKind::Ident(id)
                    if !helpers.contains(&id.name) && !extern_fns.contains_key(&id.name) =>
                {
                    format!(
                        "`{}` is not a declared helper or extern function; v1 emits the call \
                         verbatim, naming a function it never declares",
                        id.name
                    )
                }
                _ => format!("v1 emits the call verbatim. {subset}"),
            };
            not_implemented(
                &format!("{what} sampling an unsupported call"),
                &detail,
                V1Status::EmitsUncompilable,
            )
        }

        // ── v1 refuses too ───────────────────────────────────────────
        ExprKind::SetLit(_) | ExprKind::Randomize { .. } => not_implemented(
            &format!("{what} sampling a set literal or `randomize`"),
            format!("v1 rejects this shape as well. {subset}"),
            V1Status::Rejects,
        ),
        ExprKind::ForkCall { .. } | ExprKind::HashHash { .. } => not_implemented(
            &format!("{what} sampling a temporal or fork expression"),
            format!(
                "a coverpoint samples one value per trigger, so there is no cycle to span. v1 \
                 rejects this shape as well. {subset}"
            ),
            V1Status::Rejects,
        ),

        // Nothing else reaches here: the remaining `ExprKind`s
        // (`DistLit`, `Send`, `SeqRepeat`) are parse errors in a
        // coverpoint position under BOTH backends, so the arm is
        // unreachable for them. `SilentlyMisLowers` is the conservative
        // default for anything a future parser change lets through — it
        // never sends the user to v1, which is the only claim that could
        // waste their time.
        _ => not_implemented(
            &format!("{what} with an unsupported target expression"),
            subset,
            V1Status::SilentlyMisLowers,
        ),
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
    declared: &[String],
    hook_params: &[String],
    helpers: &HelperRegistry<'_>,
    extern_fns: &HashMap<String, &ExternFnDecl>,
    consts: &HashMap<String, ConstVal>,
) -> Result<Vec<Expr>, LowerError> {
    // `declared` is the callee's parameter names, passed in by the
    // caller that already resolved the declaration and checked arity.
    //
    // An earlier version re-queried the two registries here and fell
    // back to refusing every named argument when neither resolved. That
    // fallback was DEAD: both call sites are inside `if let Some(...)`
    // on those same registries, so the lookup could not fail. Taking
    // the names as an argument makes that structural rather than
    // accidental, and deletes an arm that documented a behaviour it did
    // not implement. (A callee in neither registry is refused earlier,
    // by the coverpoint call classifier.)
    //
    // v1 drops argument names and binds by position. Measured in a
    // coverpoint target: `pick(a = .., b = 1)` emits the same
    // `pick(<slice>, 1)` v1 emits positionally, and
    // `pick(b = 1, a = ..)` emits `pick(1, <slice>)` — the values
    // swapped, silently, inside the sampler that decides which bin gets
    // hit.
    super::reject_misplaced_named_args(
        args,
        declared,
        &format!("covergroup helper call `{name}(...)`"),
    )?;
    let mut lowered = Vec::with_capacity(args.len());
    for arg in args {
        let (crate::ast::CallArg::Expr(e) | crate::ast::CallArg::Named { value: e, .. }) = arg;
        lowered.push(lower_point_target(
            group,
            point,
            e,
            hook_params,
            helpers,
            extern_fns,
            consts,
        )?);
    }
    Ok(lowered)
}

/// What a folded constant is standing in for. The four roles do NOT
/// share a v1 behaviour, so they cannot share a classification — this
/// is the difference between naming the sites and measuring them.
///
/// Measured by mutating `cov_expr_targets_test` / `packed_vec_lane_test`
/// (fixtures BOTH backends pass and the equivalence harness trace-diffs)
/// one bound at a time, and compiling v1's emission against the runtime
/// headers with a stub `VTop`:
///
/// | role | `[N:0]` / `<N>` with `N` undeclared | v1 emits |
/// |---|---|---|
/// | `Vec` lane index | `dut->lane_id_out[EOF]` — **compiles**, indexes at -1 | the name verbatim |
/// | bit-slice bound | `harc_bits(v, (uint32_t)(EOF), 0)` — **compiles**, slices at 4294967295 | the name verbatim |
/// | width-method arg | — | v1 REJECTS: "`.trunc<N>()` requires a constant integer width" |
/// | cast width | `(uint64_t)(...)` — **compiles**, width ignored entirely | v1 never resolves a cast width |
///
/// `EOF` is the load-bearing input, and it is why the first two roles
/// are `SilentlyMisLowers` rather than `EmitsUncompilable`: v1 pastes
/// the HARC identifier into C++ unexamined, so a name that happens to
/// be a macro or an object in scope compiles clean and samples a bound
/// nobody wrote. A name that happens to be a `FILE*` (`stderr`) does
/// fail to compile — but the arm's status is the worst thing v1 does
/// anywhere under it, and silently sampling the wrong bit is worse
/// than a compiler error.
#[derive(Clone, Copy)]
enum ConstRole {
    LaneIndex,
    SliceBound,
    WidthArg,
    CastWidth,
}

impl ConstRole {
    fn name(self) -> &'static str {
        match self {
            ConstRole::LaneIndex => "`Vec` lane index",
            ConstRole::SliceBound => "bit-slice bound",
            ConstRole::WidthArg => "width-method width",
            ConstRole::CastWidth => "cast width",
        }
    }

    /// What v1 does with a bound at this role that TB-IR will not use
    /// — `None` meaning "v1 is not the problem at this role", which the
    /// two callers turn into different verdicts because they are asking
    /// different questions:
    ///
    ///   * hook parameter — TB-IR cannot FOLD it, and at the lane and
    ///     slice roles v1 emits the argument correctly, so `None` there
    ///     really is a working escape hatch (`Unsupported`).
    ///   * negative fold — TB-IR folded it fine and the VALUE is out of
    ///     range. v1 emits the negative bound and compiles, so `None`
    ///     there is `SilentlyMisLowers`, not a way out.
    ///
    /// The per-role answers happen to coincide, and for unrelated
    /// reasons: v1 refuses `.sext<0 - 8>()` because `eval_const_width`
    /// rejects binary operators, not because the result is negative.
    /// The alignment is measured, not structural — if a role is added,
    /// both call sites need their own probe.
    ///
    /// Shared by every refusal path, because they kept drifting: the
    /// unresolved-name path was role-aware from the start, and the
    /// hook-parameter guard added later returned a flat `Unsupported`
    /// for all four, which is a false escape hatch at two of them —
    /// `cover cmd.ticks.trunc<k>()` is something v1 refuses outright.
    fn v1_on_unfoldable(self) -> Option<V1Status> {
        match self {
            // v1 emits the bound verbatim and it works.
            ConstRole::LaneIndex | ConstRole::SliceBound => None,
            // "`.trunc<N>()` requires a constant integer width".
            ConstRole::WidthArg => Some(V1Status::Rejects),
            // v1 never resolves a cast width; it emits a plain 64-bit
            // cast and the bad width reaches no diagnostic.
            ConstRole::CastWidth => Some(V1Status::SilentlyMisLowers),
        }
    }
}

/// A constant in a coverpoint target — a `Vec` lane index, a slice
/// bound, a `.trunc<N>()`-family width, or an `as uint<N>` cast width.
///
/// Covergroup schemas lower before any runtime scope exists, so these
/// reference no locals; a file-scope `const` or enum variant is still
/// fair game, and v1 emits one as `(uint32_t)(K)` against its own
/// `static constexpr K` — identical semantics to the literal.
///
/// The whole initializer grammar folds, not just a bare name: `[1 + 2:0]`
/// is emitted by v1 as `(uint32_t)(1 + 2)` and means exactly `[3:0]`, so
/// refusing it was a subset gap with no reason behind it. `fold_const`
/// is the same evaluator `const` declarations use, which is what keeps
/// the two spellings in agreement.
fn cover_const_u64(
    group: &str,
    point: &str,
    role: ConstRole,
    e: &AstExpr,
    consts: &HashMap<String, ConstVal>,
    hook_params: &[String],
) -> Result<Option<u64>, LowerError> {
    // Sized literals are values in lane/slice positions: v1 normalizes
    // their radix spelling and uses the numeric value. Width-method and
    // type-cast widths are different — v1 requires a plain integer token
    // there — so keep those roles on their existing rejection path.
    let mut bare = e;
    while let ExprKind::Paren(inner) = &*bare.kind {
        bare = inner;
    }
    if matches!(role, ConstRole::LaneIndex | ConstRole::SliceBound) {
        if let ExprKind::Int(lit) = &*bare.kind {
            if let Some(value) = super::exprs::parse_sized_int_literal(lit) {
                return Ok(Some(value));
            }
        }
    }
    // A HOOK PARAMETER beats a file-scope `const` of the same name,
    // and this is where that rule was missing. `mentions_hook_param`
    // guarded the bin path only, so with `hookable run_for(cmd, k)` and
    // an unrelated `const k = 7` in the file:
    //
    //   cover cmd.ticks[k:0]
    //     v1   : harc_bits(cmd.ticks, (uint32_t)(k), 0)  — the ARGUMENT
    //     TB-IR: (cmd.ticks >> 0) & 0xFF                 — a fixed [7:0]
    //
    // Both compile, both run, and they sample different bits with no
    // diagnostic. The bare-`k` spelling predates the fold; `k + 0` is
    // one the fold newly reaches, so leaving this to the bin path would
    // have widened the hole while documenting the guard.
    //
    // A per-call lane/slice bound is dynamic rather than erroneous. Width
    // roles remain static and keep their measured v1 classifications.
    if let Some(name) = first_hook_param(e, hook_params) {
        let what = format!(
            "covergroup `{group}` point `{point}` {} `{name}`",
            role.name()
        );
        if role.v1_on_unfoldable().is_none() {
            return Ok(None);
        }
        return Err(match role.v1_on_unfoldable() {
            None => unreachable!(),
            Some(V1Status::Rejects) => not_implemented(
                &what,
                format!(
                    "`{name}` is a hook parameter, so the width is only known per call; v1 \
                     refuses it too (\"requires a constant integer width\")"
                ),
                V1Status::Rejects,
            ),
            Some(status) => not_implemented(
                &what,
                format!(
                    "`{name}` is a hook parameter, so the width is only known per call; v1 \
                     never resolves a cast width at all and emits a plain 64-bit cast, so \
                     the per-call width reaches no diagnostic"
                ),
                status,
            ),
        });
    }
    match fold_const(e, consts, "") {
        // A NEGATIVE fold is not a bound at any of the four roles —
        // there is no lane -1, no bit -1, and no cast to -1 bits. Left
        // unchecked, `[0 - 1]` folded to `u64::MAX` and TB-IR emitted
        // `dut->lane_id_out[18446744073709551615]` with no diagnostic
        // and a clean `verify_program`. Before the fold went in, `0 -
        // 1` was not a constant expression at all, so this is the
        // fold's own hole.
        //
        // This is also the only reader of `ConstVal::signed` on this
        // path: the bit pattern alone cannot tell -1 from `u64::MAX`.
        //
        // NOT `Invalid`, though a first version said so — and it said
        // so 20 lines above a table recording that v1 COMPILES the
        // equivalent. `EOF` is `(-1)` on glibc, so `[0 - 1]` and
        // `[EOF]` are the same C++ after preprocessing: v1 emits
        // `dut->lane_id_out[0 - 1]` and `dut->lane_id_out[EOF]`, both
        // compile, both index at -1. Calling one a program error and
        // the other a silent mis-lowering could not both be right.
        Ok(v) if v.is_negative() => Err(match role.v1_on_unfoldable() {
            None => not_implemented(
                &format!(
                    "covergroup `{group}` point `{point}` {} {}",
                    role.name(),
                    v.as_i64()
                ),
                "v1 emits the negative bound verbatim and compiles, so the point samples at \
                 a position nobody wrote",
                V1Status::SilentlyMisLowers,
            ),
            Some(status) => not_implemented(
                &format!(
                    "covergroup `{group}` point `{point}` {} {}",
                    role.name(),
                    v.as_i64()
                ),
                "a width is never negative",
                status,
            ),
        }),
        Ok(v) => Ok(Some(v.bits)),
        Err(ConstFoldErr::Unsupported(_))
            if matches!(role, ConstRole::LaneIndex | ConstRole::SliceBound)
                && first_unresolved_name(e, consts).is_none()
                && first_unfoldable_int_literal(e).is_none() =>
        {
            Ok(None)
        }
        Err(err) => Err(cover_const_refusal(group, point, role, e, consts, err)),
    }
}

/// Classify a bound that did not fold, on the SHAPE that stopped it
/// rather than on `fold_const`'s message — the message is written for
/// `const` declarations and its verdicts do not carry over. Three
/// shapes, and they take three different verdicts:
///
///   * a name nothing declares — v1 pastes it into C++; see
///     [`ConstRole`] for the per-role measurement.
///   * an integer literal outside the 64-bit domain — a SIZED literal
///     (`4'd3`) is one v1 lowers correctly (`(uint32_t)(3)`), so that
///     stays a plain subset gap; an over-wide decimal is one g++ takes
///     with a warning ("integer constant is too large for its type")
///     and truncates, so the coverpoint silently samples a bound
///     nobody wrote.
///   * anything else — a runtime lane/slice bound is represented directly;
///     width roles keep v1's own reject/silent-fallback classification.
fn cover_const_refusal(
    group: &str,
    point: &str,
    role: ConstRole,
    e: &AstExpr,
    consts: &HashMap<String, ConstVal>,
    err: ConstFoldErr,
) -> LowerError {
    let what = format!("covergroup `{group}` point `{point}` {}", role.name());
    if let Some(name) = first_unresolved_name(e, consts) {
        // Same per-role table as `v1_on_unfoldable`, spelled out here
        // because each row also carries its own DETAIL.
        return match role {
            ConstRole::LaneIndex | ConstRole::SliceBound => not_implemented(
                &format!("{what} `{name}`"),
                format!(
                    "`{name}` is not a file-scope const, enum variant, or hook parameter, \
                     and v1 pastes the name into its C++ unexamined — a name that also \
                     names a macro or an object in scope (`EOF` is one) compiles clean and \
                     samples a bound nobody wrote"
                ),
                V1Status::SilentlyMisLowers,
            ),
            ConstRole::WidthArg => not_implemented(
                &format!("{what} `{name}`"),
                format!(
                    "`{name}` is not a file-scope const, enum variant, or hook parameter; v1 refuses \
                     it too (\"requires a constant integer width\")"
                ),
                V1Status::Rejects,
            ),
            ConstRole::CastWidth => not_implemented(
                &format!("{what} `{name}`"),
                format!(
                    "`{name}` is not a file-scope const, enum variant, or hook parameter; v1 never \
                     resolves a cast width at all and emits a plain 64-bit cast, so the \
                     bad name reaches no diagnostic"
                ),
                V1Status::SilentlyMisLowers,
            ),
        };
    }
    if let Some(lit) = first_unfoldable_int_literal(e) {
        if lit.contains('\'') {
            return match role {
                ConstRole::LaneIndex | ConstRole::SliceBound => not_implemented(
                    &format!("{what} written as the sized literal `{lit}`"),
                    "the literal does not fit the TB-IR bound domain; v1 narrows it into a \
                     C++ index/bound",
                    V1Status::SilentlyMisLowers,
                ),
                ConstRole::WidthArg => not_implemented(
                    &format!("{what} written as the sized literal `{lit}`"),
                    "v1 requires a plain integer width token and rejects a sized one too",
                    V1Status::Rejects,
                ),
                ConstRole::CastWidth => not_implemented(
                    &format!("{what} written as the sized literal `{lit}`"),
                    "v1 does not resolve sized cast widths and emits its 64-bit fallback",
                    V1Status::SilentlyMisLowers,
                ),
            };
        }
        return not_implemented(
            &format!("{what} written as the over-wide literal `{lit}`"),
            "v1 emits the literal verbatim and g++ takes it with a warning — \"integer \
             constant is too large for its type\" — so the coverpoint samples a truncated \
             bound with no error",
            V1Status::SilentlyMisLowers,
        );
    }
    match err {
        ConstFoldErr::Invalid(detail) => LowerError::Invalid(format!("{what}: {detail}")),
        ConstFoldErr::Unsupported(_) => match role {
            ConstRole::LaneIndex | ConstRole::SliceBound => not_implemented(
                &format!("{what} that cannot be represented as a runtime scalar"),
                "the expression is outside the coverpoint sampler subset",
                V1Status::SilentlyMisLowers,
            ),
            ConstRole::WidthArg => not_implemented(
                &format!("{what} that is not a plain integer literal"),
                "v1 rejects non-literal width-method arguments too",
                V1Status::Rejects,
            ),
            ConstRole::CastWidth => not_implemented(
                &format!("{what} that is not a plain integer literal"),
                "v1 drops the written cast width and emits its 64-bit fallback",
                V1Status::SilentlyMisLowers,
            ),
        },
    }
}

/// The first identifier in `e` that is neither a file-scope `const` nor
/// an enum variant, if any. Walks the whole expression, because a name
/// buried in `[K + N:0]` is the reason the fold stopped just as much as
/// a bare one is.
/// Reject a bare enum-variant name declared by more than one enum
/// (`ambiguous_variants`) used anywhere inside a covergroup value
/// expression — a coverpoint target or a bin spec (harc#666). Mirrors the
/// general expression path's rejection so `--codegen tbir` and v1 agree
/// instead of v1 rejecting while tbir silently folds first-wins.
///
/// Walks the whole expression, not just its fold-subset, so an ambiguous
/// name reaches this check wherever it appears — inside a `{...}` set, a
/// `[lo..hi]` range, or an arithmetic bound. Only bare `Ident` nodes are
/// checked, so a field NAME (`t.OKAY`) is never mistaken for a variant
/// reference. A `hook_params` name shadows the variant and is skipped,
/// matching `mentions_hook_param`'s precedence in the fold path.
fn reject_ambiguous_variant_use(
    group: &str,
    location: &str,
    e: &AstExpr,
    ambiguous_variants: &HashMap<String, String>,
    hook_params: &[String],
) -> Result<(), LowerError> {
    fn walk(
        e: &AstExpr,
        ambiguous_variants: &HashMap<String, String>,
        hook_params: &[String],
        found: &mut Option<(String, String)>,
    ) {
        if found.is_some() {
            return;
        }
        match &*e.kind {
            ExprKind::Ident(id) => {
                if !hook_params.contains(&id.name) {
                    if let Some(owners) = ambiguous_variants.get(&id.name) {
                        *found = Some((id.name.clone(), owners.clone()));
                    }
                }
            }
            ExprKind::Field { target, .. }
            | ExprKind::Cast { expr: target, .. }
            | ExprKind::Unary { expr: target, .. }
            | ExprKind::HashHash { expr: target, .. }
            | ExprKind::SeqRepeat { expr: target, .. }
            | ExprKind::ForkCall { call: target } => {
                walk(target, ambiguous_variants, hook_params, found)
            }
            ExprKind::Index { target, index } => {
                walk(target, ambiguous_variants, hook_params, found);
                walk(index, ambiguous_variants, hook_params, found);
            }
            ExprKind::BitSlice { target, hi, lo } => {
                walk(target, ambiguous_variants, hook_params, found);
                walk(hi, ambiguous_variants, hook_params, found);
                walk(lo, ambiguous_variants, hook_params, found);
            }
            ExprKind::Send { target, value } => {
                walk(target, ambiguous_variants, hook_params, found);
                walk(value, ambiguous_variants, hook_params, found);
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                walk(lhs, ambiguous_variants, hook_params, found);
                walk(rhs, ambiguous_variants, hook_params, found);
            }
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => {
                walk(cond, ambiguous_variants, hook_params, found);
                walk(then_branch, ambiguous_variants, hook_params, found);
                walk(else_branch, ambiguous_variants, hook_params, found);
            }
            ExprKind::RangeLit { lo, hi } => {
                if let Some(e) = lo {
                    walk(e, ambiguous_variants, hook_params, found);
                }
                if let Some(e) = hi {
                    walk(e, ambiguous_variants, hook_params, found);
                }
            }
            ExprKind::SetLit(items) => {
                for it in items {
                    walk(it, ambiguous_variants, hook_params, found);
                }
            }
            ExprKind::Call { callee, args } => {
                walk(callee, ambiguous_variants, hook_params, found);
                for a in args {
                    let (crate::ast::CallArg::Expr(ex) | crate::ast::CallArg::Named { value: ex, .. }) = a;
                    walk(ex, ambiguous_variants, hook_params, found);
                }
            }
            ExprKind::Paren(inner) => walk(inner, ambiguous_variants, hook_params, found),
            ExprKind::SystemCall { args, .. } => {
                for a in args {
                    walk(a, ambiguous_variants, hook_params, found);
                }
            }
            // Literals, `ImplicitSelf`, `DistLit`, `Randomize`,
            // `SoftConstraint` — no bare-variant value reference reaches a
            // bin bound or coverpoint target through these, and a missed
            // descent costs only a diagnostic that does not fire (the
            // pre-#666 behaviour), never a false rejection.
            _ => {}
        }
    }

    let mut found = None;
    walk(e, ambiguous_variants, hook_params, &mut found);
    if let Some((name, owners)) = found {
        return Err(LowerError::Invalid(format!(
            "covergroup `{group}` {location}: enum variant `{name}` is \
             declared by more than one enum (`{owners}`), so no single \
             index is correct for a bare `{name}`. HARC has no qualified \
             `Enum.VARIANT` form, so rename one of them.",
        )));
    }
    Ok(())
}

fn first_unresolved_name(e: &AstExpr, consts: &HashMap<String, ConstVal>) -> Option<String> {
    let mut found = None;
    walk_expr(e, &mut |x| {
        if found.is_none() {
            if let ExprKind::Ident(id) = &*x.kind {
                if !consts.contains_key(&id.name) {
                    found = Some(id.name.clone());
                }
            }
        }
    });
    found
}

/// An integer literal in `e` that `parse_int_literal` cannot read — a
/// Verilog-style sized literal, or a decimal past `u64`.
///
/// The two kinds take OPPOSITE verdicts (v1 folds a sized literal
/// correctly; it pastes an over-wide one in and truncates), so when a
/// bound holds both, the worse one has to win — an arm's status is the
/// worst thing v1 does anywhere under it. Returning the first
/// pre-order hit made the verdict depend on operand order:
/// `[4'd3 + 999…:0]` promised `--codegen v1` while
/// `[999… + 4'd3:0]` refused to, for the same program. v1 emits
/// `(uint32_t)(3 + 999…)` either way and g++ takes it with "integer
/// constant is too large for its type".
fn first_unfoldable_int_literal(e: &AstExpr) -> Option<String> {
    let mut sized = None;
    let mut over_wide = None;
    walk_expr(e, &mut |x| {
        if let ExprKind::Int(lit) = &*x.kind {
            if super::exprs::parse_int_literal(lit).is_none() {
                let slot = if lit.contains('\'') {
                    &mut sized
                } else {
                    &mut over_wide
                };
                if slot.is_none() {
                    *slot = Some(lit.clone());
                }
            }
        }
    });
    over_wide.or(sized)
}

/// Pre-order walk over the constant-expression subset `fold_const`
/// accepts. Deliberately NOT a general AST walk: it visits exactly the
/// node kinds that can appear inside a foldable bound, so a shape it
/// does not know falls through to the "not a compile-time constant"
/// arm rather than being mis-attributed to a name or a literal inside
/// it.
fn walk_expr(e: &AstExpr, f: &mut impl FnMut(&AstExpr)) {
    f(e);
    match &*e.kind {
        ExprKind::Paren(inner) | ExprKind::Unary { expr: inner, .. } => walk_expr(inner, f),
        ExprKind::Cast { expr, .. } => walk_expr(expr, f),
        ExprKind::Binary { lhs, rhs, .. } => {
            walk_expr(lhs, f);
            walk_expr(rhs, f);
        }
        _ => {}
    }
}

fn cover_const_u32(
    group: &str,
    point: &str,
    role: ConstRole,
    e: &AstExpr,
    consts: &HashMap<String, ConstVal>,
    hook_params: &[String],
) -> Result<Option<u32>, LowerError> {
    cover_const_u64(group, point, role, e, consts, hook_params)?
        .map(|v| {
            u32::try_from(v).map_err(|_| {
                LowerError::Invalid(format!(
                    "covergroup `{group}` point `{point}` {} {v} does not fit in u32",
                    role.name()
                ))
            })
        })
        .transpose()
}

fn cover_width_arg(
    group: &str,
    point: &str,
    method: &str,
    src_width: Option<u32>,
    e: &AstExpr,
    consts: &HashMap<String, ConstVal>,
    hook_params: &[String],
) -> Result<u32, LowerError> {
    let width = cover_const_u32(group, point, ConstRole::WidthArg, e, consts, hook_params)?
        .expect("width roles never lower as dynamic bounds");
    // The spec says LITERAL — "Width `N` must be a positive constant
    // integer literal in `1..=1024`" — and v1 enforces it with
    // `eval_const_width`, refusing anything else with "requires a
    // constant integer width (saw a non-const expression)". The fold
    // above accepts a `const` name and arithmetic over one, so
    // `.zext<1 + 7>()` and `.trunc<KW - 8>()` LOWERED here while v1
    // refused them.
    //
    // Placed AFTER the fold so a name that resolves to nothing, or to a
    // hook parameter, still reaches its own role-classified arm — those
    // say something more useful than "not a literal".
    if literal_width(e).is_none() {
        return Err(LowerError::Invalid(format!(
            "covergroup `{group}` point `{point}` `.{method}<...>()`: the width must be a \
             plain integer literal (v1: \"requires a constant integer width\")"
        )));
    }
    // The zero-width, 1024-bit-limit and direction rules come from
    // `exprs::width_method_violation` — the SAME function the general
    // expression path uses — rather than being restated here. Three
    // separate statements of these rules is what this file had, and
    // the third drifted on every one of them: `resize` wrongly in the
    // direction set, the language limit missing, the receiver width
    // inferred through a fold where v1 uses literals only.
    //
    // Only the prefix is this path's own.
    if let Some(why) = super::exprs::width_method_violation(method, width, src_width) {
        return Err(LowerError::Invalid(format!(
            "covergroup `{group}` point `{point}` {why}"
        )));
    }
    Ok(width)
}

/// A width written as a plain integer LITERAL, the only spelling v1's
/// `eval_const_width` reads. Used for width INFERENCE so TB-IR's
/// direction check fires exactly where v1's does; the folding
/// `cover_const_u32` still lowers the bound itself.
fn literal_width(e: &AstExpr) -> Option<u32> {
    match &*e.kind {
        ExprKind::Paren(inner) => literal_width(inner),
        ExprKind::Int(s) => super::exprs::parse_int_literal(s).and_then(|v| u32::try_from(v).ok()),
        _ => None,
    }
}

/// The literal width of an `as uint<N>` / `sint<N>` / `bits<N>` cast,
/// on the same literal-only rule as [`literal_width`]. A bare `as uint`
/// with no argument is 64, matching `cover_cast_width`.
fn cast_literal_width(ty: &crate::ast::TypeExpr) -> Option<u32> {
    let crate::ast::TypeExpr::Builtin {
        name:
            crate::ast::BuiltinTy::UInt | crate::ast::BuiltinTy::SInt | crate::ast::BuiltinTy::Bits,
        args,
        ..
    } = ty
    else {
        return None;
    };
    match args.first() {
        Some(crate::ast::TypeArg::Expr(e)) => literal_width(e),
        Some(_) => None,
        None => Some(64),
    }
}

fn cover_cast_width(
    group: &str,
    point: &str,
    ty: &crate::ast::TypeExpr,
    consts: &HashMap<String, ConstVal>,
    hook_params: &[String],
) -> Result<Option<u32>, LowerError> {
    let width = match ty {
        crate::ast::TypeExpr::Builtin {
            name:
                crate::ast::BuiltinTy::UInt | crate::ast::BuiltinTy::SInt | crate::ast::BuiltinTy::Bits,
            args,
            ..
        } => match args.first() {
            Some(crate::ast::TypeArg::Expr(e)) => Some(
                cover_const_u32(group, point, ConstRole::CastWidth, e, consts, hook_params)?
                    .expect("cast-width roles never lower as dynamic bounds"),
            ),
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
    }
    Ok(width)
}

fn cover_infer_expr_width(
    group: &str,
    point: &str,
    e: &AstExpr,
    consts: &HashMap<String, ConstVal>,
    hook_params: &[String],
) -> Result<Option<u32>, LowerError> {
    match &*e.kind {
        ExprKind::Paren(inner) => cover_infer_expr_width(group, point, inner, consts, hook_params),
        ExprKind::BitSlice { hi, lo, .. } => {
            // LITERAL-only, deliberately, and not the folding
            // `cover_const_u32` that lowers the slice itself.
            //
            // This feeds the direction check, which has to fire exactly
            // where v1's does or it invents program errors. v1 infers a
            // receiver width with `eval_const_width` (literal only), as
            // does TB-IR's own general expression path with
            // `const_eval_width`. Folding here made `const HI = 100` +
            // `[HI:0].zext<70>()` an `Invalid` that v1 compiles and
            // runs, while the literal spelling `[100:0].zext<70>()` is
            // correctly `Invalid` under both — the same program, two
            // verdicts, decided by whether the bound was written with a
            // name.
            let (Some(hi), Some(lo)) = (literal_width(hi), literal_width(lo)) else {
                return Ok(None);
            };
            if hi < lo {
                return Err(LowerError::Invalid(format!(
                    "covergroup `{group}` point `{point}` has invalid bit slice [{hi}:{lo}]"
                )));
            }
            Ok(Some(hi - lo + 1))
        }
        // Same rule for a cast width: v1 reads it with
        // `eval_const_width`, so a `const K = 128` receiver has no
        // inferred width under v1 and must have none here either.
        ExprKind::Cast { ty, .. } => Ok(cast_literal_width(ty)),
        ExprKind::Call { callee, args } => {
            if let ExprKind::Field { target, name } = &*callee.kind {
                if matches!(name.name.as_str(), "trunc" | "zext" | "sext" | "resize") {
                    let width_expr = match args.first() {
                        Some(crate::ast::CallArg::Expr(e)) if args.len() == 1 => e,
                        _ => return Ok(None),
                    };
                    // The receiver's width IS available here — it is
                    // `callee`'s own target — and passing `None`
                    // instead meant a NESTED width method lost the
                    // direction check: `[3:0].trunc<128>()` is
                    // `Invalid` on its own and became "re-run with
                    // `--codegen v1`" the moment anything wrapped it
                    // (`.trunc<128>().zext<128>()`), for the same inner
                    // program v1 refuses either way. A slice or a cast
                    // wrapper kept the right answer; only this path
                    // lost it.
                    let recv_width =
                        cover_infer_expr_width(group, point, target, consts, hook_params)?;
                    return Ok(Some(cover_width_arg(
                        group,
                        point,
                        &name.name,
                        recv_width,
                        width_expr,
                        consts,
                        hook_params,
                    )?));
                }
            }
            Ok(None)
        }
        ExprKind::Int(s) => Ok(
            if let Some((width, _)) = super::exprs::parse_sized_int_literal_with_width(s) {
                Some(width)
            } else {
                super::exprs::parse_int_literal(s).map(|v| {
                    if v == 0 {
                        1
                    } else {
                        64 - v.leading_zeros()
                    }
                })
            },
        ),
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
    consts: &HashMap<String, ConstVal>,
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
    consts: &HashMap<String, ConstVal>,
    hook_params: &[String],
    helpers: &HelperRegistry<'_>,
    extern_fns: &HashMap<String, &ExternFnDecl>,
) -> Result<CovBinBound, LowerError> {
    match &*e.kind {
        ExprKind::Paren(inner) => {
            lower_bin_bound(group, bin, inner, consts, hook_params, helpers, extern_fns)
        }
        ExprKind::Int(s) => Ok(CovBinBound::Const(parse_bound(group, bin, s)?)),
        // A constant EXPRESSION over literals and file-scope names folds
        // like a bare one — v1 emits `_v == 1 - 1`, which is `_v == 0`.
        // Checked before the runtime arm so `{1 - 1}` becomes a `Const`
        // bin rather than a per-sample expression, matching how the
        // literal spelling lowers. A hook param anywhere inside keeps
        // its precedence, so the fold is skipped when one appears.
        _ if !mentions_hook_param(e, hook_params) => match fold_const(e, consts, "") {
            Ok(v) => Ok(CovBinBound::Const(v.bits)),
            Err(_) => bin_bound_fallback(group, bin, e, consts, hook_params, helpers, extern_fns),
        },
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

/// Whether any bare identifier in `e` names a hook parameter. Bins and
/// point targets share one precedence rule — a hook param beats a
/// file-scope `const` of the same name — and this is what keeps the
/// constant fold from breaking it. Without the check, adding an
/// unrelated `const cmd = 1` would silently turn `one = {cmd}` into
/// `_v == 1` while `cover cmd.ticks` in the same covergroup still meant
/// the hook argument: one name, two meanings, no diagnostic.
fn mentions_hook_param(e: &AstExpr, hook_params: &[String]) -> bool {
    first_hook_param(e, hook_params).is_some()
}

/// The first hook-parameter name in `e`, for the diagnostic that says
/// which name is being deferred to.
fn first_hook_param(e: &AstExpr, hook_params: &[String]) -> Option<String> {
    let mut found = None;
    walk_expr(e, &mut |x| {
        if found.is_none() {
            if let ExprKind::Ident(id) = &*x.kind {
                if hook_params.contains(&id.name) {
                    found = Some(id.name.clone());
                }
            }
        }
    });
    found
}

/// A bin bound that did not fold. Three outcomes, and only one of them
/// is an error, because "did not fold" covers a legitimate RUNTIME
/// bound as well as two mistakes.
///
/// Measured by emitting the whole testbench under v1 and diffing
/// against the literal spelling, which is how the comparison line
/// (`if (((_v == <bound>)))`) was found rather than guessed:
///
/// | bound | v1 emits | outcome |
/// |---|---|---|
/// | `dut.en` | `_v == harc_rt::harc_read(dut->en)` | correct, per-sample — lowers as `Runtime` |
/// | `N` (undeclared) | `_v == N` | "'N' was not declared in this scope" |
/// | `EOF` | `_v == EOF` | **compiles**; the bin can never match, and nothing says so |
/// | `4'd0` | folded to `0` | correct — v1 lowers sized literals here |
/// | `99999999999999999999999` | verbatim | compiles with a warning, truncates |
///
/// So an unresolvable NAME is `SilentlyMisLowers` — `EOF` is the input
/// that decides it, exactly as for a slice bound (divergence 87) — and
/// an over-wide literal is too, while a sized literal keeps the v1
/// suggestion because v1 folds it correctly.
#[allow(clippy::too_many_arguments)]
fn bin_bound_fallback(
    group: &str,
    bin: &str,
    e: &AstExpr,
    consts: &HashMap<String, ConstVal>,
    hook_params: &[String],
    helpers: &HelperRegistry<'_>,
    extern_fns: &HashMap<String, &ExternFnDecl>,
) -> Result<CovBinBound, LowerError> {
    if let Some(name) = first_unresolved_name(e, consts) {
        return Err(not_implemented(
            &format!("covergroup `{group}` bin `{bin}` spec `{name}`"),
            format!(
                "`{name}` is not a file-scope const, enum variant, or hook parameter, and v1 \
                 pastes the name into its `_v == {name}` comparison unexamined — a name that \
                 also names a macro in scope (`EOF` is one) compiles and gives a bin that \
                 can never match"
            ),
            V1Status::SilentlyMisLowers,
        ));
    }
    if let Some(lit) = first_unfoldable_int_literal(e) {
        if lit.contains('\'') {
            let expr = lower_point_target(group, bin, e, hook_params, helpers, extern_fns, consts)
                .map_err(|err| relabel_point_as_bin(err, bin))?;
            return Ok(CovBinBound::Runtime(expr));
        }
        return Err(not_implemented(
            &format!("covergroup `{group}` bin `{bin}` spec `{lit}`"),
            "v1 emits the literal verbatim into `_v == <lit>` and g++ takes it with a \
             warning — \"integer constant is too large for its type\" — so the bin compares \
             against a truncated value with no error",
            V1Status::SilentlyMisLowers,
        ));
    }
    // A genuine runtime bound. Reuse the point-target expression
    // lowerer so the emitted per-sample value matches v1 (v1 renders it
    // with the same `emit_expr` it uses for point targets). It rejects
    // anything outside the sampler subset with a precise diagnostic —
    // but names it a "point", so relabel: the source declared a bin.
    let expr = lower_point_target(group, bin, e, hook_params, helpers, extern_fns, consts)
        .map_err(|err| relabel_point_as_bin(err, bin))?;
    Ok(CovBinBound::Runtime(expr))
}

/// A bin bound written as a bare integer literal. Splits the same way
/// [`bin_bound_fallback`] does, and for the same measured reasons — it
/// is reached first because `fold_const` is not consulted for a shape
/// that is already a literal.
fn parse_bound(group: &str, bin: &str, s: &str) -> Result<u64, LowerError> {
    super::exprs::parse_int_literal(s)
        .or_else(|| super::exprs::parse_sized_int_literal(s))
        .ok_or_else(|| {
            if s.contains('\'') {
                return not_implemented(
                &format!("covergroup `{group}` bin `{bin}` spec `{s}`"),
                "the sized literal's value is wider than the 64-bit cover sample; v1 narrows it",
                V1Status::SilentlyMisLowers,
            );
            }
            not_implemented(
                &format!("covergroup `{group}` bin `{bin}` spec `{s}`"),
                "v1 emits the literal verbatim into `_v == <lit>` and g++ takes it with a \
             warning — \"integer constant is too large for its type\" — so the bin compares \
             against a truncated value with no error",
                V1Status::SilentlyMisLowers,
            )
        })
}
