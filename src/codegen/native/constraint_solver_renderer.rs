//! Native constrained-randomization Z3 rendering.
//!
//! The TBIR planner owns constraint-site selection; this module preserves the
//! legacy C++ solver rendering while that backend seam is incrementally retired.

use super::*;

impl Emitter {
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
    /// literals are polymorphic: they take the signedness of a typed operand.
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
                    self.consts.get(&id.name).map(|c| {
                        if c.signedness == crate::constraints::Signedness::Signed {
                            ConstraintOperandSignedness::Signed
                        } else {
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
            ExprKind::Call { .. } => match sum_call_elem_signed(e, field_info, target_root) {
                true => ConstraintOperandSignedness::Signed,
                false => ConstraintOperandSignedness::Unsigned,
            },
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
        if l == ConstraintOperandSignedness::Unsigned || r == ConstraintOperandSignedness::Unsigned
        {
            let (signed_expr, unsigned_expr) = if l == ConstraintOperandSignedness::Signed
                && r == ConstraintOperandSignedness::Unsigned
            {
                (lhs, rhs)
            } else if r == ConstraintOperandSignedness::Signed
                && l == ConstraintOperandSignedness::Unsigned
            {
                (rhs, lhs)
            } else {
                return false;
            };
            return self
                .constraint_expr_native_width(signed_expr, field_info, target_root)
                .unwrap_or(0)
                > self
                    .constraint_expr_native_width(unsigned_expr, field_info, target_root)
                    .unwrap_or(u32::MAX);
        }
        l == ConstraintOperandSignedness::Signed || r == ConstraintOperandSignedness::Signed
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
            ExprKind::Binary { lhs, rhs, .. } => Some(
                self.constraint_expr_native_width(lhs, field_info, target_root)?
                    .max(self.constraint_expr_native_width(rhs, field_info, target_root)?),
            ),
            _ => None,
        }
    }

    /// Signedness of a constraint expression, picking `<` vs `z3::ult`,
    /// `/` vs `z3::udiv` and `%` vs `z3::urem`.
    ///
    /// Resolves *transaction fields* the way `constraint_expr_width` does
    /// — dotted paths through `expr_field_path` — and for the same reason:
    /// these are different Z3 predicates over the *same* variable, so a
    /// wrong hit is a wrong solved value, not a cosmetic difference. A
    /// field whose width equals `solver_width` carries no
    /// `ult(_z_x, 1<<W)` range assumption at all (that bound is only
    /// emitted when the field is narrower), so under `bvslt` the solver is
    /// free to return a value the source constraint forbids.
    ///
    /// It does NOT have the width oracle's `const`/enum fallbacks, and
    /// that is correct: both are emitted as non-negative `uint64_t`, so
    /// `false` is the right answer for them.
    ///
    /// A signed `let` under `blocking randomize` and a signed list element
    /// were the two gaps harc#563 tracked; both are closed below. A
    /// SHADOWED local is the one case still answered `false` — see the
    /// `shadowed_lets` guard on the `Ident` arm for why guessing there is
    /// worse than under-reporting.

    /// A `${...}` capture that does not parse as an expression.
    ///
    /// This used to `write!(self.out, "{}", cap.expr)` — the raw HARC text,
    /// straight into the generated C++, so `log(info, "b=${1 +}")` emitted
    /// `harc_printf_ll(1 +)` and the user's first sign of trouble was their
    /// C++ compiler complaining about a line they never wrote (harc#593).
    ///
    /// The parser now rejects such a capture, so this should be
    /// unreachable. It stays as a fail-closed backstop: an error plus a
    /// value that at least compiles, rather than a silent paste that does
    /// not.
    pub(super) fn reject_unparseable_capture(&mut self, expr: &str) {
        self.errors.push(format!(
            "`${{{expr}}}` is not an expression; an interpolation holds one complete \
             expression, optionally followed by `:` and a format spec"
        ));
        write!(self.out, "0").ok();
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
                    // C++'s z3::expr `%` overload is bvsmod (the sign follows
                    // the divisor). HARC signed remainder truncates toward
                    // zero, so use the explicit bvsrem helper instead.
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
                    self.emit_constraint_expr_w(
                        lhs,
                        field_info,
                        width,
                        target_root,
                        blocking,
                        list_unroll_bounds,
                    );
                    write!(self.out, "{sep}").ok();
                    self.emit_constraint_expr_w(
                        rhs,
                        field_info,
                        width,
                        target_root,
                        blocking,
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
