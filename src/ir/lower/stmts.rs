//! Statement lowering — one IR form per AST construct (design doc
//! §"Statements within a run / check / transactor body").

use super::{not_implemented, unsupported, FuncBuilder, LowerError, V1Status};
use crate::ast::{
    BuiltinTy, CallArg, Expr as AstExpr, ExprKind, Ident, Stmt as AstStmt, StmtKind, TypeArg,
    TypeExpr,
};
use crate::ir::{
    ComponentBase, ComponentFieldKind, EventChannelRef, Expr, FileLogLevel, FmtArg, FmtArgs,
    IrType, LogLevel, MethodHookTarget, Stmt, TbFunction, Terminator, TypedParam,
};

/// Resolve a strict (no-parentheses) method-hook path against the current
/// testbench. Returns `None` for a path v1's method-hook resolver rejects.
fn resolve_statement_method_hook_target(
    b: &super::FuncBuilder<'_>,
    h: &crate::ast::OnHandler,
) -> Result<Option<(MethodHookTarget, Vec<TypedParam>)>, LowerError> {
    let Some(raw) = super::strict_method_hook_path(&h.event) else {
        return Ok(None);
    };
    let segs: &[String] = if raw.len() >= 2
        && Some(raw[0].as_str()) == b.ctx.tb_field.as_deref()
        && (b.ctx.transactor_fields.contains_key(&raw[1])
            || b.ctx.component_fields.contains_key(&raw[1]))
    {
        &raw[1..]
    } else {
        &raw
    };
    if segs.len() < 2 {
        return Ok(None);
    }
    let receiver = &segs[..segs.len() - 1];
    let method = segs.last().expect("hook path has method").clone();
    if let [field] = receiver {
        let Some(xid) = b.ctx.transactor_fields.get(field).copied() else {
            // A component field with the same one-segment receiver is
            // handled below.
            if !b.ctx.component_fields.contains_key(field) {
                return Ok(None);
            }
            return resolve_component_hook_target(b, receiver, method);
        };
        let x = &b.ctx.transactors[xid.index()];
        let Some(m) = x.methods.iter().find(|m| m.name == method && m.hookable) else {
            return Ok(None);
        };
        if m.active_only && b.ctx.passive_transactor_fields.contains(field) {
            return Ok(None);
        }
        let params = m
            .param_names
            .iter()
            .cloned()
            .zip(m.param_tys.iter().cloned())
            .map(|(name, ty)| TypedParam { name, ty })
            .collect();
        return Ok(Some((
            MethodHookTarget::Transactor {
                field: field.clone(),
                transactor: xid,
                method,
            },
            params,
        )));
    }
    resolve_component_hook_target(b, receiver, method)
}

fn resolve_component_hook_target(
    b: &super::FuncBuilder<'_>,
    receiver: &[String],
    method: String,
) -> Result<Option<(MethodHookTarget, Vec<TypedParam>)>, LowerError> {
    let Some((head, tail)) = receiver.split_first() else {
        return Ok(None);
    };
    let Some(&head_cid) = b.ctx.component_fields.get(head) else {
        return Ok(None);
    };
    let cid = b.resolve_component_recv(head_cid, tail)?;
    let comp = &b.ctx.components[cid.index()];
    let Some(m) = comp.methods.iter().find(|m| m.name == method && m.hookable) else {
        return Ok(None);
    };
    b.require_component_activation(
        head,
        head_cid,
        b.binding_mode(head),
        tail,
        m.activation,
        "method hook",
        &method,
    )?;
    let params = m
        .param_names
        .iter()
        .cloned()
        .zip(m.param_tys.iter().cloned())
        .map(|(name, ty)| TypedParam { name, ty })
        .collect();
    Ok(Some((
        MethodHookTarget::Component {
            base: ComponentBase::Path(receiver.to_vec()),
            component: cid,
            method,
        },
        params,
    )))
}

impl FuncBuilder<'_> {
    pub(crate) fn lower_stmt(&mut self, s: &AstStmt) -> Result<(), LowerError> {
        self.ensure_open_block();
        match &s.kind {
            StmtKind::Let(l) => self.lower_let(l),
            StmtKind::Assign { target, value } | StmtKind::Send { target, value } => {
                self.lower_assign(target, value)
            }
            StmtKind::For(f) => self.lower_for(f),
            StmtKind::Repeat(r) => self.lower_repeat(r),
            StmtKind::Loop(b) => self.lower_loop(b),
            StmtKind::While { cond, body, .. } => self.lower_while(cond, body),
            StmtKind::Break { .. } => {
                // Loops opened in a caller are off-limits to an inlined
                // helper body — same floor rule as name lookup.
                let floor = self.inline_frames.last().map_or(0, |f| f.loop_floor);
                let Some(frame) = self.loop_stack.get(floor..).and_then(|s| s.last()) else {
                    return Err(LowerError::Invalid("`break` outside a loop".to_string()));
                };
                self.terminate(Terminator::Jump(frame.break_to));
                Ok(())
            }
            StmtKind::Continue { .. } => {
                let floor = self.inline_frames.last().map_or(0, |f| f.loop_floor);
                let Some(frame) = self.loop_stack.get(floor..).and_then(|s| s.last()) else {
                    return Err(LowerError::Invalid("`continue` outside a loop".to_string()));
                };
                self.terminate(Terminator::Jump(frame.continue_to));
                Ok(())
            }
            StmtKind::If(i) => self.lower_if(i),
            StmtKind::Wait {
                duration, clock, ..
            } => {
                // Wall-clock wait (`wait 80ns`): resolved to
                // picoseconds at lowering, suspended via the inline
                // `eval_clocks_until(now_ps + N)` form. Only
                // test-scoped bodies may use it because emitted helpers
                // must capture the surrounding scheduler runtime.
                if let ExprKind::Time(s) = &*duration.kind {
                    if clock.is_some() {
                        return Err(unsupported(
                            "`wait <time> on <clock>`",
                            "clock-qualified waits take a cycle count",
                        ));
                    }
                    if !self.ctx.allow_scheduler_time_waits {
                        return Err(unsupported(
                            "wall-clock `wait <time>` in this context",
                            "only test-scoped run/check, hook, component, and \
                             transactor method bodies can capture scheduler time",
                        ));
                    }
                    let ps = super::time_literal_to_ps(s).map_err(LowerError::Invalid)?;
                    let next = self.new_block();
                    self.terminate(Terminator::WaitTimePs(ps, next));
                    self.start_block(next);
                    return Ok(());
                }
                // `wait N cycles [on <clock>]`. The clock qualifier
                // resolves against the test's declared clocks HERE —
                // v1 deferred the unknown-clock error to emission; the
                // IR pipeline rejects it at lowering with the same
                // message shape plus the declared-clock list.
                self.lower_wait_cycles(duration, clock.as_ref())
            }
            StmtKind::Return(None) => {
                if self.lower_helper_return(None)? {
                    return Ok(());
                }
                self.terminate(Terminator::Return);
                Ok(())
            }
            StmtKind::Return(Some(e)) => {
                if self.lower_helper_return(Some(e))? {
                    return Ok(());
                }
                // v1 emits a bare `return <expr>;` inside the run
                // coroutine, which a C++20 coroutine cannot do (only
                // `co_return`), so the generated TB does not compile.
                Err(not_implemented(
                    "`return <expr>`",
                    "run/check bodies do not return a value",
                    V1Status::EmitsUncompilable,
                ))
            }
            StmtKind::Assert(v) => self.lower_assert(v),
            StmtKind::Fail { msg, .. } => {
                // Unconditional failure — design doc: `fail` is a
                // synonym for the failing arm of an assert at this
                // layer, so lower as an AssertCheck with a false
                // condition (identical runtime-observable behavior).
                let on_fail = self.lower_fail_msg(msg)?;
                self.push(Stmt::AssertCheck {
                    cond: Expr::Literal {
                        value: 0,
                        ty: IrType::Bool,
                    },
                    on_fail,
                });
                Ok(())
            }
            StmtKind::Log { args, .. } => self.lower_log(args, None),
            StmtKind::LogF { args, .. } => {
                let path = args.iter().find_map(|a| match a {
                    CallArg::Expr(e) => match &*e.kind {
                        ExprKind::String(s) => Some(s.clone()),
                        _ => None,
                    },
                    _ => None,
                });
                let Some(path) = path else {
                    return Err(LowerError::Invalid(
                        "`logf` requires a string file path as its first argument".to_string(),
                    ));
                };
                self.lower_log(args, Some(path))
            }
            StmtKind::WaitUntil {
                mode,
                conditions,
                timeout,
                ..
            } => self.lower_wait_until(*mode, conditions, timeout.as_ref()),
            // `after N cycles ... end after` — suspend N cycles, then run
            // the body (§7.4 suspend primitive). Mirrors v1: the cycle wait
            // uses the exact same coroutine/sync split as a clockless
            // `wait N cycles`, after which the body statements execute. The
            // duration is a cycle count (no wall-clock `Time` form here —
            // v1's `after` emit never special-cased a time literal).
            StmtKind::After { duration, body, .. } => {
                self.lower_wait_cycles(duration, None)?;
                self.lower_block_stmts(body)
            }
            // ── Explicit unsupported stubs (MVP) ────────────────────
            StmtKind::Randomize {
                blocking,
                target,
                with_body,
            } => self.lower_randomize(*blocking, target, with_body),
            // `join_all` drains the RHS-fork TLM barrier (`let x = fork
            // bus.m(...)`); the block-form `fork ... and ... join`
            // statement is out of subset.
            StmtKind::JoinAll { .. } => self.lower_tlm_join_all(),
            // The block form (`fork branch … end branch … join_all`) has
            // no emitter in either backend — v1 hits its
            // "statement not supported in v0 cpp_tb" fallback. Only the
            // RHS-fork TLM form is implemented, and only in TB-IR.
            StmtKind::Fork(_) => Err(not_implemented(
                "block-form `fork ... branch ... join_all`",
                "only the RHS-fork TLM form (`let x = fork bus.m(...)` + `join_all`) is \
                 lowered, and only by the TB-IR backend",
                V1Status::Rejects,
            )),
            // The PSS-style activity-composition operators (spec §17.1).
            // Parsed, but no backend emits them — v1 hits its
            // "statement not supported in v0 cpp_tb" fallback for each.
            StmtKind::Parallel(_) => Err(not_implemented(
                "`parallel`",
                "the activity-composition operators (spec §17.1) are parsed but not \
                 lowered by any backend",
                V1Status::Rejects,
            )),
            StmtKind::Schedule(_) => Err(not_implemented(
                "`schedule`",
                "the activity-composition operators (spec §17.1) are parsed but not \
                 lowered by any backend",
                V1Status::Rejects,
            )),
            StmtKind::Select(_) => Err(not_implemented(
                "`select`",
                "the activity-composition operators (spec §17.1) are parsed but not \
                 lowered by any backend",
                V1Status::Rejects,
            )),
            StmtKind::On(h) => self.lower_on_handler(h),
            StmtKind::Emit { name, args, .. } => self.lower_emit(name, args),
            StmtKind::Yield(e) => self.lower_yield(e),
            // Aspect activation (spec §3.6). Parsed, but no backend
            // applies an aspect — v1 hits its "statement not supported in
            // v0 cpp_tb" fallback, so a `package`'s `extend` blocks are
            // inert under both backends.
            StmtKind::Apply(_) => Err(not_implemented(
                "`apply`",
                "aspect activation (spec §3.6) is parsed but not lowered by any backend, \
                 so a `package`'s `extend` blocks never take effect",
                V1Status::Rejects,
            )),
            StmtKind::Release(e) => self.lower_release(e),
            StmtKind::Assume(v) => self.lower_assume(v),
            StmtKind::Cover(v) => self.lower_cover(v),
            StmtKind::Expr(e) => {
                // Statement-position `fork bus.m(...)` — issue the request
                // now and discard the response at the next `join_all`.
                if self.try_lower_tlm_fork(e, super::bus::BusCallDest::Discard)? {
                    return Ok(());
                }
                // Statement-position bus calls: `mem.poke(a, d)` (call
                // edge, result-less or discarded) and bare
                // `axil.w.send(...)` / `axil.r.recv()` handshakes.
                if self.try_lower_bus_call(e, super::bus::BusCallDest::Discard)? {
                    return Ok(());
                }
                // `bitbash(regs)` — RAL walk-all over the regblock's RW
                // registers (write/read both patterns + compare).
                if self.try_lower_bitbash(e)? {
                    return Ok(());
                }
                // `regs.record_write(addr, data)` — passive mirror update
                // of an observed bus write (no bus, no callback).
                if self.try_lower_record_write(e)? {
                    return Ok(());
                }
                // Testbench helper method call (`_tb.reset()`), CFG-
                // inlined like an impure helper; statement position
                // discards the (usually void) result.
                if let ExprKind::Call { callee, args } = &*e.kind {
                    if let Some(m) = self.tb_method_call_name(callee) {
                        self.lower_tb_method_call(&m, args)?;
                        return Ok(());
                    }
                }
                // Testbench-owned queue mutation: `_tb.pending.push(x)`.
                // `.pop()` must bind its result in a `let`, matching every
                // other typed queue owner.
                if let ExprKind::Call { callee, args } = &*e.kind {
                    if let Some((field, method)) = self.as_tb_queue_call(callee) {
                        if method == "push" {
                            let [CallArg::Expr(arg)] = args.as_slice() else {
                                return Err(LowerError::Invalid(format!(
                                    "testbench queue `{field}.push` takes exactly one positional argument"
                                )));
                            };
                            let value = self.lower_expr_no_ports(arg)?;
                            let elem = self.tb_queue_elem(&field)?;
                            self.check_queue_push(
                                &value,
                                &elem,
                                &format!("testbench queue `{field}`"),
                            )?;
                            self.push(Stmt::TbQueuePush { field, value });
                            return Ok(());
                        }
                        if method == "pop" {
                            queue_pop_takes_no_arguments(
                                &format!("testbench queue `{field}`"),
                                args,
                            )?;
                            let elem = self.tb_queue_elem(&field)?;
                            let dest = self.discard_slot(elem);
                            self.push(Stmt::TbQueuePop { field, dest });
                            return Ok(());
                        }
                        if discard_queue_query_statement(
                            &format!("testbench queue `{field}`"),
                            &method,
                            args,
                        )? {
                            return Ok(());
                        }
                        return Err(queue_method_in_statement_position(&format!(
                            "testbench queue method `{field}.{method}(...)`"
                        )));
                    }
                }
                // `cov.report()` (post-desugar `_tb.cov.report()`) on a
                // covergroup-typed testbench field → CovReport.
                if let ExprKind::Call { callee, args } = &*e.kind {
                    if let ExprKind::Field { target, name } = &*callee.kind {
                        if name.name == "report" && args.is_empty() {
                            if let Some((field, rest)) = self.as_cov_field_path(target) {
                                if rest.is_empty() {
                                    let covgroup = self.ctx.cov_fields[&field];
                                    self.push(Stmt::CovReport(crate::ir::CovgroupInstance {
                                        tb_field: field,
                                        covgroup,
                                    }));
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
                // Statement-position scoreboard queue op:
                // `sb.expected.push(x)` / `sb.expected.pop()`. A bare pop
                // discards its value into an unread temp — the mutation is
                // the point of writing it that way.
                if let ExprKind::Call { callee, args } = &*e.kind {
                    if let Some((sb, field, queue, method, nested_path)) =
                        self.as_scoreboard_queue_call(callee)
                    {
                        if method == "push" {
                            // Validate `queue` is a declared queue field
                            // (a scalar/unknown field would mis-lower to
                            // `_tb.<sb>.<scalar>.push(...)` — invalid C++).
                            self.scoreboard_queue_field(sb, &queue)?;
                            let [CallArg::Expr(arg)] = args.as_slice() else {
                                return Err(LowerError::Invalid(format!(
                                    "scoreboard `{field}.{queue}.push` takes exactly one \
                                     positional argument"
                                )));
                            };
                            let value = self.lower_expr_no_ports(arg)?;
                            let elem = self.scoreboard_queue_elem(sb, &queue)?;
                            self.check_queue_push(
                                &value,
                                &elem,
                                &format!("scoreboard queue `{queue}`"),
                            )?;
                            self.push(Stmt::ScoreboardOp {
                                sb,
                                field,
                                op: crate::ir::ScoreboardOp::QueuePush { queue, value },
                                nested_path,
                            });
                            return Ok(());
                        }
                        if method == "pop" {
                            queue_pop_takes_no_arguments(
                                &format!("scoreboard queue `{field}.{queue}`"),
                                args,
                            )?;
                            let elem = self.scoreboard_queue_elem(sb, &queue)?;
                            let dest = self.discard_slot(elem);
                            self.push(Stmt::ScoreboardOp {
                                sb,
                                field,
                                op: crate::ir::ScoreboardOp::QueuePop { queue, dest },
                                nested_path,
                            });
                            return Ok(());
                        }
                        if discard_queue_query_statement(
                            &format!("scoreboard queue `{field}.{queue}`"),
                            &method,
                            args,
                        )? {
                            return Ok(());
                        }
                        return Err(queue_method_in_statement_position(&format!(
                            "scoreboard queue method `{field}.{queue}.{method}(...)`"
                        )));
                    }
                }
                // Statement-position component-queue op:
                // `errors.push(e)` (self) / `checker.sb.errors.push(e)`
                // (path), and the discarding `errors.pop()`, mirroring the
                // scoreboard form.
                if let ExprKind::Call { callee, args } = &*e.kind {
                    if let Some((base, queue, method)) = self.as_component_queue_call(callee)? {
                        if method == "push" {
                            let [CallArg::Expr(arg)] = args.as_slice() else {
                                return Err(LowerError::Invalid(format!(
                                    "component `{queue}.push` takes exactly one positional argument"
                                )));
                            };
                            let value = self.lower_expr_no_ports(arg)?;
                            let elem = self.component_queue_elem(&base, &queue)?;
                            self.check_queue_push(
                                &value,
                                &elem,
                                &format!("component queue `{queue}`"),
                            )?;
                            self.push(Stmt::ComponentQueuePush { base, queue, value });
                            return Ok(());
                        }
                        if method == "pop" {
                            queue_pop_takes_no_arguments(
                                &format!("component queue `{queue}`"),
                                args,
                            )?;
                            let elem = self.component_queue_elem(&base, &queue)?;
                            let dest = self.discard_slot(elem);
                            self.push(Stmt::ComponentQueuePop { base, queue, dest });
                            return Ok(());
                        }
                        if discard_queue_query_statement(
                            &format!("component queue `{queue}`"),
                            &method,
                            args,
                        )? {
                            return Ok(());
                        }
                        return Err(queue_method_in_statement_position(&format!(
                            "component queue method `{queue}.{method}(...)`"
                        )));
                    }
                }
                // Statement-position bound-to target-responder queue
                // state-field op: `pending.push(x)` / `pending.pop()`
                // (bare field name), mirroring the component form.
                if let ExprKind::Call { callee, args } = &*e.kind {
                    if let Some((field, method)) = self.as_state_queue_call(callee) {
                        if method == "push" {
                            let [CallArg::Expr(arg)] = args.as_slice() else {
                                return Err(LowerError::Invalid(format!(
                                    "target-state `{field}.push` takes exactly one positional \
                                     argument"
                                )));
                            };
                            let value = self.lower_expr_no_ports(arg)?;
                            // The FIFTH queue lane. Same rule as the
                            // scoreboard/component/testbench pushes; it
                            // reads its element from `target_state_fields`
                            // rather than a queue table.
                            if let Some(crate::ir::StateFieldKind::Queue { elem }) =
                                self.target_state_fields.get(&field).cloned()
                            {
                                self.check_queue_push(
                                    &value,
                                    &elem,
                                    &format!("target-state queue `{field}`"),
                                )?;
                            }
                            self.push(Stmt::TransactorStateQueuePush {
                                instance: String::new(),
                                field,
                                value,
                            });
                            return Ok(());
                        }
                        if method == "pop" {
                            queue_pop_takes_no_arguments(
                                &format!("target-state queue `{field}`"),
                                args,
                            )?;
                            let crate::ir::StateFieldKind::Queue { elem } =
                                self.target_state_fields[&field].clone()
                            else {
                                unreachable!("as_state_queue_call gated on the Queue kind");
                            };
                            let dest = self.discard_slot(elem);
                            self.push(Stmt::TransactorStateQueuePop {
                                instance: String::new(),
                                field,
                                dest,
                            });
                            return Ok(());
                        }
                        if discard_queue_query_statement(
                            &format!("target-state queue `{field}`"),
                            &method,
                            args,
                        )? {
                            return Ok(());
                        }
                        return Err(queue_method_in_statement_position(&format!(
                            "target-state queue method `{field}.{method}(...)`"
                        )));
                    }
                }
                // Statement-position TEST-SCOPE target-responder queue
                // state op: `target.pending.push(x)` /
                // `target.pending.pop()` (fully resolved instance).
                if let ExprKind::Call { callee, args } = &*e.kind {
                    if let ExprKind::Field { target, name } = &*callee.kind {
                        if let Some((instance, field, kind)) = self.as_transactor_state_any(target)
                        {
                            if matches!(kind, crate::ir::StateFieldKind::Queue { .. }) {
                                let method = name.name.clone();
                                if method == "push" {
                                    let [CallArg::Expr(arg)] = args.as_slice() else {
                                        return Err(LowerError::Invalid(format!(
                                            "target-state `{instance}.{field}.push` takes exactly \
                                             one positional argument"
                                        )));
                                    };
                                    let value = self.lower_expr_no_ports(arg)?;
                                    // The SIXTH lane — the test-scope
                                    // spelling of the same push. `kind`
                                    // is already the queue's, from the
                                    // `matches!` this branch gated on.
                                    if let crate::ir::StateFieldKind::Queue { elem } = &kind {
                                        self.check_queue_push(
                                            &value,
                                            elem,
                                            &format!("target-state queue `{instance}.{field}`"),
                                        )?;
                                    }
                                    self.push(Stmt::TransactorStateQueuePush {
                                        instance,
                                        field,
                                        value,
                                    });
                                    return Ok(());
                                }
                                if method == "pop" {
                                    queue_pop_takes_no_arguments(
                                        &format!("target-state queue `{instance}.{field}`"),
                                        args,
                                    )?;
                                    let crate::ir::StateFieldKind::Queue { elem } = kind else {
                                        unreachable!("the enclosing `matches!` gated on Queue");
                                    };
                                    let dest = self.discard_slot(elem);
                                    self.push(Stmt::TransactorStateQueuePop {
                                        instance,
                                        field,
                                        dest,
                                    });
                                    return Ok(());
                                }
                                if discard_queue_query_statement(
                                    &format!("target-state queue `{instance}.{field}`"),
                                    &method,
                                    args,
                                )? {
                                    return Ok(());
                                }
                                return Err(queue_method_in_statement_position(&format!(
                                    "target-state queue method \
                                         `{instance}.{field}.{method}(...)`"
                                )));
                            }
                        }
                    }
                }
                // Statement-position component method call:
                // `env.source.publish(3)` — call for effect, result (if
                // any) discarded.
                if let ExprKind::Call { callee, args } = &*e.kind {
                    if let Some((base, component, method)) =
                        self.as_component_method_call(callee)?
                    {
                        let declared = self.ctx.components[component.index()]
                            .method(&method)
                            .map(|m| m.param_names.clone());
                        let lowered = self.lower_component_call_args(args, declared.as_deref())?;
                        self.check_component_call_args(component, &method, &lowered)?;
                        self.push(Stmt::ComponentCall {
                            base,
                            component,
                            method,
                            args: lowered,
                            dest: None,
                        });
                        return Ok(());
                    }
                }
                // A direct transactor heartbeat predicate is a pure value,
                // not a method edge. In statement position v1 still
                // evaluates its threshold expression and discards the
                // boolean, so retain it in an unread temp rather than
                // dropping the statement or routing it through
                // `Stmt::TransactorCall`.
                if let ExprKind::Call { callee, args } = &*e.kind {
                    if let Some(idle) = self.as_transactor_idle(callee, args)? {
                        let discard = self.fresh_temp();
                        self.set_local_type(discard, crate::ir::IrType::Bool);
                        self.push(Stmt::Assign(discard, idle));
                        return Ok(());
                    }
                }
                // Statement-position transactor method call:
                // `xact.write1(2, 17, true)` — call for effect, result
                // (if any) discarded, mirroring v1.
                if let ExprKind::Call { callee, args } = &*e.kind {
                    if let Some(call) = self.lower_transactor_call(callee, args, false)? {
                        self.push(Stmt::TransactorCall { dest: None, call });
                        return Ok(());
                    }
                }
                let what = match &*e.kind {
                    ExprKind::Call { callee, args } => match &*callee.kind {
                        ExprKind::Ident(id) => {
                            if self.in_testbench_method_frame()
                                && self.ctx.tb_methods.contains_key(&id.name)
                            {
                                self.lower_tb_method_call(&id.name, args)?;
                                return Ok(());
                            }
                            if let Some(call) =
                                self.lower_transactor_self_call(&id.name, args, false)?
                            {
                                self.push(Stmt::TransactorSelfCall { dest: None, call });
                                return Ok(());
                            }
                            if self.helpers.contains(&id.name) {
                                // Statement-position helper call: lower
                                // for effect, discard the value. Impure
                                // helpers inline to an `Expr::Local`
                                // (already evaluated); pure calls keep
                                // the `Expr::Call` in a discard temp so
                                // the C++ call survives, mirroring v1.
                                let val = self.lower_helper_call(&id.name, args)?;
                                if !matches!(val, crate::ir::Expr::Local(_)) {
                                    let val = self.hoist_ports(val);
                                    let t = self.fresh_temp();
                                    self.push(Stmt::Assign(t, val));
                                }
                                return Ok(());
                            }
                            format!("helper call `{}(...)`", id.name)
                        }
                        ExprKind::Field { name, .. } => {
                            format!("method call `.{}(...)`", name.name)
                        }
                        _ => "an expression statement".to_string(),
                    },
                    _ => "an expression statement".to_string(),
                };
                Err(unsupported(&what, ""))
            }
        }
    }

    /// Lower a cycle-count wait — `wait N cycles [on <clock>]` and the
    /// suspend prefix of `after N cycles ... end after`. The clock
    /// qualifier resolves against the test's declared clocks HERE — v1
    /// deferred the unknown-clock error to emission; the IR pipeline
    /// rejects it at lowering with the same message shape plus the
    /// declared-clock list.
    fn lower_wait_cycles(
        &mut self,
        duration: &crate::ast::Expr,
        clock: Option<&crate::ast::Ident>,
    ) -> Result<(), LowerError> {
        if self.in_transactor_method && clock.is_some() {
            return Err(unsupported(
                "`wait N cycles on <clock>` inside a transactor method",
                "method bodies run synchronously and know no test clocks",
            ));
        }
        let clock = match clock {
            Some(c) => {
                let Some(index) = self.ctx.clock_names.iter().position(|n| n == &c.name) else {
                    let declared = if self.ctx.clock_names.is_empty() {
                        "none".to_string()
                    } else {
                        self.ctx.clock_names.join(", ")
                    };
                    return Err(LowerError::Invalid(format!(
                        "wait ... on {}: no clock named `{}` declared in this \
                         test (declared clocks: {declared})",
                        c.name, c.name
                    )));
                };
                Some(crate::ir::WaitClock {
                    name: c.name.clone(),
                    index,
                })
            }
            None => None,
        };
        let n = self.lower_expr_no_ports(duration)?;
        let next = self.new_block();
        // Plain waits inside an inlined helper / testbench-method body
        // take v1's synchronous lambda path (no coroutine yield) — see
        // `Terminator::WaitCyclesSync`.
        if clock.is_none() && !self.inline_frames.is_empty() {
            self.terminate(Terminator::WaitCyclesSync(n, next));
        } else {
            self.terminate(Terminator::WaitCycles(n, clock, next));
        }
        self.start_block(next);
        Ok(())
    }

    /// `Some(payload)` when this `let` declares a test-scope event
    /// channel (`let e : event<T>`), resolving `T` through the same
    /// payload rules a component's `event<T>` FIELD uses. `None` for
    /// every other `let`.
    fn event_let_payload(
        &self,
        l: &crate::ast::LetStmt,
    ) -> Result<Option<crate::ir::EventPayload>, LowerError> {
        let Some(TypeExpr::Builtin {
            name: BuiltinTy::Event,
            args,
            ..
        }) = l.ty.as_ref()
        else {
            return Ok(None);
        };
        let payload = super::components::lower_event_payload(
            "<test scope>",
            &l.name.name,
            args.first(),
            &self.ctx.record_ids,
        )?;
        Ok(Some(payload))
    }

    fn lower_let(&mut self, l: &crate::ast::LetStmt) -> Result<(), LowerError> {
        if !l.probes.is_empty() {
            return Err(unsupported("probe declarations", ""));
        }
        if l.bind {
            // Test-scope bindings (`let axil : BusAxiLite = bind ...`,
            // regblock / addrmap / transactor binds) are lowered by
            // `lower_test`, which sees the testbench surface. Reaching
            // here means a `= bind` in STATEMENT position, which v1
            // accepts and then emits as an ordinary `let` — the `bind`
            // is dropped, so the binding the user wrote never happens.
            return Err(not_implemented(
                "a `= bind ...` declaration in statement position",
                "declare the binding at test scope (as a `let` on the test or a field on \
                 the testbench), where the bind surface is resolved",
                V1Status::SilentlyMisLowers,
            ));
        }
        // `let v = _tb.pending.pop()` on a testbench-owned queue. This
        // precedes the generic scalar/record let paths so record-element
        // pops retain their record type.
        if let Some((call, args)) = pop_call_parts(&l.value) {
            if let Some((field, method)) = self.as_tb_queue_call(call) {
                if method == "pop" {
                    queue_pop_takes_no_arguments(&format!("testbench queue `{field}`"), args)?;
                    let elem = self.tb_queue_elem(&field)?;
                    self.check_pop_let_type(l, &elem, &format!("testbench queue `{field}`"))?;
                    let id = self.declare(&l.name.name);
                    match elem {
                        crate::ir::QueueElem::Record(rid) => {
                            self.set_local_type(id, IrType::Record(rid));
                        }
                        crate::ir::QueueElem::Scalar { .. } => {
                            if let Some(w) = l.ty.as_ref().and_then(typed_let_width) {
                                self.let_widths.insert(id, w);
                            }
                            if let Some(ty) = l.ty.as_ref().and_then(typed_let_ir_type) {
                                self.set_local_type(id, ty);
                            }
                        }
                    }
                    self.push(Stmt::TbQueuePop { field, dest: id });
                    return Ok(());
                }
            }
        }
        // `let v = <recv>.<queue>.pop()` on a composite-component queue —
        // pop the front into a new local. Checked before the record-typed
        // / scalar let-RHS forms: a record-element pop is `let err :
        // CheckerError = ...pop()` (a record-typed let WITH an initializer,
        // which the record-local block below rejects). The popped local's
        // type comes from the queue element (record id from the field, or a
        // scalar width from the optional `let` annotation).
        if let Some((call, args)) = pop_call_parts(&l.value) {
            if let Some((base, queue, method)) = self.as_component_queue_call(call)? {
                if method == "pop" {
                    queue_pop_takes_no_arguments(&format!("component queue `{queue}`"), args)?;
                    let elem = self.component_queue_elem(&base, &queue)?;
                    self.check_pop_let_type(l, &elem, &format!("component queue `{queue}`"))?;
                    let id = self.declare(&l.name.name);
                    match elem {
                        crate::ir::QueueElem::Record(rid) => {
                            self.set_local_type(id, IrType::Record(rid));
                        }
                        crate::ir::QueueElem::Scalar { .. } => {
                            if let Some(w) = l.ty.as_ref().and_then(typed_let_width) {
                                self.let_widths.insert(id, w);
                            }
                        }
                    }
                    self.push(Stmt::ComponentQueuePop {
                        base,
                        queue,
                        dest: id,
                    });
                    return Ok(());
                }
            }
        }
        // `let v = sb.q.pop()` on a data-only scoreboard queue — pop the
        // front into a new local. Checked here (before the record-typed /
        // scalar let-RHS forms) for the same reason as the component-queue
        // pop above: a record-element pop is `let s : Sample = ...pop()`
        // (a record-typed let WITH an initializer, which the record-local
        // block below would otherwise reject). The popped local's type
        // comes from the queue element — the record id for a value-record
        // element, or a scalar width from the optional `let` annotation.
        if let Some(value) = &l.value {
            if let Some((sb, field, queue, nested_path)) = self.as_scoreboard_pop(value)? {
                let elem = self.scoreboard_queue_elem(sb, &queue)?;
                self.check_pop_let_type(l, &elem, &format!("scoreboard queue `{queue}`"))?;
                let id = self.declare(&l.name.name);
                match elem {
                    crate::ir::QueueElem::Record(rid) => {
                        self.set_local_type(id, IrType::Record(rid));
                    }
                    crate::ir::QueueElem::Scalar { .. } => {
                        if let Some(w) = l.ty.as_ref().and_then(typed_let_width) {
                            self.let_widths.insert(id, w);
                        }
                        if let Some(ty) = l.ty.as_ref().and_then(typed_let_ir_type) {
                            self.set_local_type(id, ty);
                        }
                    }
                }
                self.push(Stmt::ScoreboardOp {
                    sb,
                    field,
                    op: crate::ir::ScoreboardOp::QueuePop { queue, dest: id },
                    nested_path,
                });
                return Ok(());
            }
        }
        // `let v = pending.pop()` on a bound-to target-responder queue
        // state field (bare field name). Same ordering rationale as the
        // component/scoreboard pops above: a record-element pop is a
        // record-typed let WITH an initializer. The popped local's type
        // comes from the queue element.
        if let Some((call, args)) = pop_call_parts(&l.value) {
            if let Some((field, method)) = self.as_state_queue_call(call) {
                if method == "pop" {
                    queue_pop_takes_no_arguments(&format!("target-state queue `{field}`"), args)?;
                    let crate::ir::StateFieldKind::Queue { elem } =
                        self.target_state_fields[&field].clone()
                    else {
                        // as_state_queue_call already gated on Queue kind.
                        unreachable!("state-queue pop on a non-queue field");
                    };
                    self.check_pop_let_type(l, &elem, &format!("target-state queue `{field}`"))?;
                    let id = self.declare(&l.name.name);
                    match elem {
                        crate::ir::QueueElem::Record(rid) => {
                            self.set_local_type(id, IrType::Record(rid));
                        }
                        crate::ir::QueueElem::Scalar { .. } => {
                            if let Some(w) = l.ty.as_ref().and_then(typed_let_width) {
                                self.let_widths.insert(id, w);
                            }
                            if let Some(ty) = l.ty.as_ref().and_then(typed_let_ir_type) {
                                self.set_local_type(id, ty);
                            }
                        }
                    }
                    self.push(Stmt::TransactorStateQueuePop {
                        instance: String::new(),
                        field,
                        dest: id,
                    });
                    return Ok(());
                }
            }
        }
        // `let v = target.pending.pop()` — TEST-SCOPE pop on a bound-to
        // responder queue state field (fully resolved instance). Same
        // ordering rationale as the responder-body state-queue pop above.
        if let Some((call, args)) = pop_call_parts(&l.value) {
            if let ExprKind::Field { target, name } = &*call.kind {
                if name.name == "pop" {
                    if let Some((instance, field, kind)) = self.as_transactor_state_any(target) {
                        if let crate::ir::StateFieldKind::Queue { elem } = kind {
                            queue_pop_takes_no_arguments(
                                &format!("target-state queue `{instance}.{field}`"),
                                args,
                            )?;
                            self.check_pop_let_type(
                                l,
                                &elem,
                                &format!("target-state queue `{instance}.{field}`"),
                            )?;
                            let id = self.declare(&l.name.name);
                            match elem {
                                crate::ir::QueueElem::Record(rid) => {
                                    self.set_local_type(id, IrType::Record(rid));
                                }
                                crate::ir::QueueElem::Scalar { .. } => {
                                    if let Some(w) = l.ty.as_ref().and_then(typed_let_width) {
                                        self.let_widths.insert(id, w);
                                    }
                                    if let Some(ty) = l.ty.as_ref().and_then(typed_let_ir_type) {
                                        self.set_local_type(id, ty);
                                    }
                                }
                            }
                            self.push(Stmt::TransactorStateQueuePop {
                                instance,
                                field,
                                dest: id,
                            });
                            return Ok(());
                        }
                    }
                }
            }
        }
        // A `let` whose declared type names a REGBLOCK or ADDRMAP is an
        // instantiation, and an instantiation without `= bind <helper>`
        // has no bus for its registers to reach. v1 states that rule and
        // refuses to emit at all — "regblock instantiation requires
        // `= bind <helper>` (a transactor with write/read methods)" —
        // for every spelling: at test scope, inside `run`, with no
        // initializer, and with a same-typed mirror on the right.
        //
        // This ran BEFORE the record arm below because a regblock's
        // mirror record shares the regblock's name, so without the guard
        // the let landed on the ordinary record-local path and lowered
        // clean. The emitted testbench then served every register access
        // from that mirror and issued NO bus traffic at all: the control
        // emits `AxilHelper_write(40, 64); v = AxilHelper_read(40);` and
        // the unbound one emits `v = regs.MM2S_LEN;`. The test passes
        // without ever touching the DUT.
        //
        // A properly bound `let regs : R = bind <helper>` never reaches
        // here — the test-item walk in `mod.rs` consumes it — and a
        // `= bind` written in statement position is rejected on its own
        // before this. Divergence 104.
        if let Some(e) = self.regblock_instantiation_error(l) {
            return Err(e);
        }
        // Record-typed local: `let t : TxnType` default-constructs (v1
        // declares the struct at the let site, so field defaults re-run
        // on every loop iteration — RecordInit mirrors that).
        if let Some(TypeExpr::Named { name, .. }) = l.ty.as_ref() {
            let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
            if let Some(&rid) = self.ctx.record_ids.get(simple) {
                if self.in_pure_helper {
                    return Err(unsupported(
                        &format!(
                            "transaction-typed local `let {}` in a pure helper function",
                            l.name.name
                        ),
                        "pure helpers emit as scalar-only file-scope functions",
                    ));
                }
                // `let r : ReadResponse = model.predict_read(addr)` — a
                // record-typed local bound from a component-method call that
                // returns that record. The dest local is record-typed and
                // the call carries it (v1's `ReadResponse r =
                // <Comp>_<method>(...)`). Any other initializer shape stays
                // rejected (field-by-field assignment is the only other
                // supported form).
                if let Some(value) = &l.value {
                    if let ExprKind::Call { callee, args } = &*value.kind {
                        if let Some((base, component, method)) =
                            self.as_component_method_call(callee)?
                        {
                            let comp = &self.ctx.components[component.index()];
                            // UNREACHABLE by construction, and kept only so the
                            // condition has one verdict everywhere it is
                            // written. `as_component_method_call` validates
                            // the method on EVERY path that returns
                            // `Ok(Some(..))` — the two that error do so
                            // themselves, the two that do not are guarded by
                            // `is_some()` — so by the time a caller holds
                            // `(base, component, method)` the method exists.
                            // Confirmed by mutation: neutering this arm
                            // fails no test, because nothing reaches it.
                            // The reachable landing is the resolver's own
                            // arm in `components.rs`, which is where the
                            // measurement lives.
                            let m = comp.method(&method).ok_or_else(|| {
                                LowerError::Invalid(format!(
                                    "component `{}` has no method `{method}`",
                                    comp.name
                                ))
                            })?;
                            if !m.has_ret {
                                // REACHED, contrary to an earlier note
                                // here. A typed `let` over a SCALAR type
                                // is claimed by the untyped handler
                                // below, which is what that note
                                // generalized from — but this arm is
                                // guarded on a RECORD type, and
                                // `let t : TinyTxn = c.noret(3)` lands
                                // here first try. v1: "conversion from
                                // 'void' to non-scalar type 'TinyTxn'
                                // requested".
                                return Err(LowerError::Invalid(format!(
                                    "`let {} : {simple} = {}.{method}(...)` — method \
                                     `{method}` returns no value",
                                    l.name.name, comp.name
                                )));
                            }
                            self.check_component_method_result_assignable(
                                m,
                                IrType::Record(rid),
                                &method,
                                &l.name.name,
                            )?;
                            let declared = m.param_names.clone();
                            let lowered = self.lower_component_call_args(args, Some(&declared))?;
                            self.check_component_call_args(component, &method, &lowered)?;
                            let id = self.declare(&l.name.name);
                            self.set_local_type(id, IrType::Record(rid));
                            self.push(Stmt::ComponentCall {
                                base,
                                component,
                                method,
                                args: lowered,
                                dest: Some(id),
                            });
                            return Ok(());
                        }
                    }
                    // `let t2 : Txn = t1` — a by-value copy from a
                    // same-typed record value: a record local, a whole
                    // nested-record field read (`s.inner`), or one
                    // `Vec<Record, N>` element (`tbl.entries[i]`). v1
                    // emits `Txn t2 = <rhs>;`, a C++ struct copy; we
                    // mirror it by declaring a record-typed local and
                    // binding it with `Stmt::Assign`, which the emitter
                    // renders as the same `name = <rhs>;` copy. This
                    // matches the discipline of the whole-record
                    // assignment path (`u = t` in `lower_assign`): only a
                    // same-typed record value copies; any other RHS stays
                    // a precise rejection rather than a verifier
                    // TypeMismatch or a C++ compile error.
                    let e = self.lower_expr_no_ports(value)?;
                    let same_record = self.record_id_of_expr(&e) == Some(rid);
                    if same_record {
                        let id = self.declare(&l.name.name);
                        self.set_local_type(id, IrType::Record(rid));
                        self.push(Stmt::Assign(id, e));
                        return Ok(());
                    }
                    // The INITIALIZER spelling of the write the
                    // assignment arms below reject. It rejected the same
                    // family under one `Unsupported`, so it promised
                    // `--codegen v1` for `let b : Beat = 5` (v1:
                    // `Beat b = 5;` — "conversion from 'int' to
                    // non-scalar type 'Beat' requested"), `= x` on an
                    // unrelated record, and `= s.n` on a scalar
                    // component field. Now that the RHS types
                    // definitively, so does the verdict.
                    return Err(self.record_assign_mismatch(
                        &e,
                        rid,
                        format!("transaction local `{}`", l.name.name),
                        "copy from a same-typed transaction local, or default-construct \
                         and assign the fields individually",
                    ));
                }
                let id = self.declare(&l.name.name);
                self.set_local_type(id, IrType::Record(rid));
                self.push(Stmt::RecordInit(id, rid));
                return Ok(());
            }
        }
        // A test-scope event channel (`let e : event<uint<8>>`, spec
        // §3.4). v1 declares it as a subscriber vector local in the run
        // coroutine; `on e(v)` pushes, `emit e(x)` fans out. It has no
        // initializer by construction, so it is decided before the
        // uninitialized-scalar arm below.
        if let Some(payload) = self.event_let_payload(l)? {
            if l.value.is_some() {
                return Err(LowerError::Invalid(format!(
                    "`let {} : event<...>` takes no initializer — an event channel starts \
                     empty and gains subscribers through `on {}(...)`",
                    l.name.name, l.name.name
                )));
            }
            let id = self.declare(&l.name.name);
            self.set_local_type(id, IrType::Event(payload));
            return Ok(());
        }
        let Some(value) = &l.value else {
            // Uninitialized scalar `let x: uint<N>;` — the declare-then-
            // assign-in-loop idiom. The local is hoisted as `<cty> x = 0;`
            // by `declare_locals` (v1 emits `int64_t x = 0;`), so we just
            // register the typed local; later `=` assignments fill it.
            // Requires a declared scalar type to size it; an untyped
            // `let x;` with no initializer cannot be sized → rejected.
            // (Uninitialized records are handled by the record arm above.)
            if let Some(ty) = l.ty.as_ref().and_then(typed_let_ir_type) {
                let id = self.declare(&l.name.name);
                if let Some(w) = l.ty.as_ref().and_then(typed_let_width) {
                    self.let_widths.insert(id, w);
                }
                self.set_local_type(id, ty.clone());
                // Explicit zero-init definition: matches v1's `<cty> x = 0;`
                // and gives the verifier's dominance check a definition that
                // dominates every later read of the declared-then-assigned
                // local.
                self.push(Stmt::Assign(id, Expr::Literal { value: 0, ty }));
                return Ok(());
            }
            // Stays an `Unsupported`: v1 emits a COMMENT for the
            // declaration (`// let x (no type / no value)`), so whether
            // its output compiles depends on whether the name is ever
            // USED — an unused `let x` builds fine there, a later
            // `x = 1` does not. The rejection fires at the declaration,
            // before that is known, so it cannot claim either outcome.
            return Err(unsupported(
                &format!("uninitialized `let {}` without a scalar type", l.name.name),
                "declare it with a scalar type (`let x: uint<N>;`) or give an initializer",
            ));
        };
        // Explicit scalar bit-width of the declaration, tracked on
        // every path for the width-method receiver inference (v1's
        // `let_widths` seeds from typed lets regardless of RHS shape).
        let declared_width = l.ty.as_ref().and_then(typed_let_width);
        let declared_scalar_ty = l.ty.as_ref().and_then(typed_let_ir_type);
        // Direct DUT-read form: `let x = dut.port` → DutRead(x, port).
        if let Some(port) = self.as_port_ref(value)? {
            let id = self.declare(&l.name.name);
            if let Some(w) = declared_width {
                self.let_widths.insert(id, w);
            }
            if let Some(ty) = declared_scalar_ty.clone() {
                self.set_local_type(id, ty);
            }
            self.push(Stmt::DutRead(id, port));
            return Ok(());
        }
        // Bus-bound signal read (`let x = axil.r.data`) — same shape.
        if let Some(port) = self.as_bus_port_ref(value)? {
            let id = self.declare(&l.name.name);
            if let Some(w) = declared_width {
                self.let_widths.insert(id, w);
            }
            if let Some(ty) = declared_scalar_ty.clone() {
                self.set_local_type(id, ty);
            }
            self.push(Stmt::DutRead(id, port));
            return Ok(());
        }
        // `let x = fork bus.<method>(a)` — issue the request now, defer
        // the response capture to the next `join_all`. Checked before the
        // plain bus call (a `ForkCall` RHS is never a plain `Call`).
        if self.try_lower_tlm_fork(value, super::bus::BusCallDest::Declare(&l.name.name))? {
            return Ok(());
        }
        // Bus call RHS: `let x = mem.read(a)` (TransactorMethod call
        // edge) or `let x = axil.r.recv()` (CFG-inlined handshake).
        // Checked before transactor fields: the namespaces are
        // disjoint (collision rejected at testbench construction).
        if self.try_lower_bus_call(value, super::bus::BusCallDest::Declare(&l.name.name))? {
            return Ok(());
        }
        // (A `let v = sb.q.pop()` scoreboard-queue pop is handled earlier,
        // before the record-typed-local block, so a record-element pop
        // `let s : Sample = sb.q.pop()` is recognized as a pop rather than
        // rejected as a record-typed let with an initializer — mirrors the
        // composite-component pop placement.)
        // `let txns = RandomTxns(5)` — a tseq generator call. The result
        // is an `IrType::RecordSeq` assigned into a fresh test-scope local;
        // the call edge is a `CallTarget::Tseq` whose args lower like any
        // scalar call. Checked before the transactor-call form (a tseq
        // name and a transactor field are disjoint namespaces).
        if let ExprKind::Call { callee, args } = &*value.kind {
            if let ExprKind::Ident(name) = &*callee.kind {
                if let Some((elem, _, _)) = self.ctx.tseqs.get(&name.name) {
                    let seq_ty = elem.seq_type();
                    let call = self.lower_tseq_call(&name.name, args)?;
                    let id = self.declare(&l.name.name);
                    self.set_local_type(id, seq_ty);
                    self.push(Stmt::Assign(id, call));
                    return Ok(());
                }
            }
        }
        // `let v = xact.method(...)` — a transactor call edge with a
        // result destination.
        if let ExprKind::Call { callee, args } = &*value.kind {
            if let Some(call) = self.lower_transactor_call(callee, args, true)? {
                let id = self.declare(&l.name.name);
                self.push(Stmt::TransactorCall {
                    dest: Some(id),
                    call,
                });
                return Ok(());
            }
        }
        // `let v = env.drv.axil_read(addr)` — a value-returning component
        // method call into a fresh local.
        if let ExprKind::Call { callee, args } = &*value.kind {
            if let Some((base, component, method)) = self.as_component_method_call(callee)? {
                let comp = &self.ctx.components[component.index()];
                // UNREACHABLE by construction, and kept only so the
                // condition has one verdict everywhere it is
                // written. `as_component_method_call` validates
                // the method on EVERY path that returns
                // `Ok(Some(..))` — the two that error do so
                // themselves, the two that do not are guarded by
                // `is_some()` — so by the time a caller holds
                // `(base, component, method)` the method exists.
                // Confirmed by mutation: neutering this arm
                // fails no test, because nothing reaches it.
                // The reachable landing is the resolver's own
                // arm in `components.rs`, which is where the
                // measurement lives.
                let m = comp.method(&method).ok_or_else(|| {
                    LowerError::Invalid(format!(
                        "component `{}` has no method `{method}`",
                        comp.name
                    ))
                })?;
                if !m.has_ret {
                    // MEASURED and reachable. `let x = c.noret(3)`
                    // lands here, and v1 emits `auto x = Calc_noret(c,
                    // 3);` — g++: "deduced type 'void' for 'x' is
                    // incomplete". (The TYPED form emits `uint64_t x =
                    // ...` and says "void value not ignored as it ought
                    // to be"; a first version of this comment paired
                    // this arm's source with that arm's emission.)
                    // Taking a value from something that produces none
                    // is a program error either way, so `Invalid` rather
                    // than a suggestion. Pinned by mutation.
                    return Err(LowerError::Invalid(format!(
                        "`let {} = {}.{method}(...)` — method `{method}` returns no value",
                        l.name.name, comp.name
                    )));
                }
                if let Some(expected) = declared_scalar_ty.clone() {
                    self.check_component_method_result_assignable(
                        m,
                        expected,
                        &method,
                        &l.name.name,
                    )?;
                }
                let declared = m.param_names.clone();
                let lowered = self.lower_component_call_args(args, Some(&declared))?;
                self.check_component_call_args(component, &method, &lowered)?;
                let id = self.declare(&l.name.name);
                if let Some(w) = declared_width {
                    self.let_widths.insert(id, w);
                }
                // The method's OWN return type when the `let` carries
                // no annotation. `ret_ty` was sitting right here unused,
                // so `let b = s.mk(1)` on a `-> Beat` method declared an
                // untyped local: tbir emitted `uint64_t b; b =
                // Src_mk(...)` — "cannot convert 'Beat' to 'uint64_t'" —
                // while v1's `auto b = Src_mk(_tb.s, 1);` compiles.
                //
                // That silent mis-emission became visible the moment the
                // slot guards below started reading the local's type:
                // they saw "a scalar" and called five well-typed
                // programs `Invalid`. Typing the local fixes both.
                // Gated on `l.ty`, NOT on `declared_scalar_ty` —
                // `typed_let_ir_type` answers `None` for `int` and for
                // `bit`, so keying on it made the annotation invisible
                // and typed the local as the method's record. The
                // default backend then emitted `Beat n{}` for
                // `let n : int = s.mk(1)` and RAN, while v1 refused to
                // build it: a silent mis-lowering, and the exact defect
                // the untyped-`let` guard a screen up keys on `l.ty` to
                // prevent.
                if let (Some(_), Some(IrType::Record(rid))) = (&l.ty, &m.ret_ty) {
                    if declared_scalar_ty.is_none() {
                        return Err(LowerError::Invalid(format!(
                            "`let {}` is declared with a non-record type and initialised \
                             from a `{}`",
                            l.name.name,
                            self.ctx.records[rid.index()].name
                        )));
                    }
                }
                // No need to re-test `l.ty`: `declared_scalar_ty` is
                // `l.ty.and_then(typed_let_ir_type)`, so `l.ty == None`
                // implies it is `None` too, and the pair
                // `(None, Some(_))` is uninhabited. (An earlier comment
                // credited the guard above for this; that was the wrong
                // reason for a right conclusion.)
                // `RecordSeq`/`Seq` alongside `Record`: a `-> TSeq<T>`
                // method result is as much a typed value as a record
                // one, and dropping it left `let ys = k.gen(xs)`
                // untyped — so the next slot the local entered read it
                // as having no known shape. `method_schema_ir_type`
                // resolves the sequence into `ret_ty`; this arm just has
                // to stop discarding it.
                let inferred = declared_scalar_ty.clone().or_else(|| match &m.ret_ty {
                    Some(ty @ (IrType::Record(_) | IrType::RecordSeq(_) | IrType::Seq(_))) => {
                        Some(ty.clone())
                    }
                    _ => None,
                });
                if let Some(ty) = inferred {
                    self.set_local_type(id, ty);
                }
                self.push(Stmt::ComponentCall {
                    base,
                    component,
                    method,
                    args: lowered,
                    dest: Some(id),
                });
                return Ok(());
            }
        }
        // RAL passive record read: `let v = regs.record_read(addr)`.
        if self.try_lower_record_read_let(&l.name.name, value)? {
            return Ok(());
        }
        // RAL frontdoor register-level read: `let v = regs.NAME`.
        if self.try_lower_regblock_read_let(&l.name.name, value)? {
            return Ok(());
        }
        // RAL frontdoor field-level read: `let v = regs.REG.FIELD`.
        if self.try_lower_regblock_subfield_read_let(&l.name.name, value)? {
            return Ok(());
        }
        // RAL addrmap read: `let v = chip.inst.REG[.FIELD]`.
        if self.try_lower_addrmap_read_let(&l.name.name, value)? {
            return Ok(());
        }
        // Any other regblock-binding access in `let`-RHS position
        // (an unknown register) is out of subset — reject precisely.
        self.reject_out_of_subset_regblock_access(value, "read")?;
        self.reject_out_of_subset_addrmap_access(value, "read")?;
        // Testbench method call RHS: `let x = _tb.m(...)`, CFG-inlined.
        if self.try_lower_tb_method_let(l, value)? {
            return Ok(());
        }
        let e = self.lower_expr_no_ports(value)?;
        // Record-valued RHS with no `: T` annotation — the bare copy form
        // `let t2 = t1`. v1's untyped fallback would mis-declare it as
        // `int64_t t2 = t1;` (broken for a struct); typing the dest as the
        // source record makes the generic record-local declare + struct-copy
        // `Stmt::Assign` carry it correctly. Scalars are untouched (only a
        // record-typed RHS opts in here).
        let record_ty = self
            .expr_type(&e)
            .filter(|t| matches!(t, IrType::Record(_)))
            // `expr_type` types only the record shapes it owns tables
            // for (`Local`, `RecordField`). It has no arm for a
            // record-valued COMPONENT field, transactor state field or
            // nested state subfield — while `record_id_of_expr` has an
            // arm for all of them — so `let c = src.cur` fell through
            // to the scalar default and the DEFAULT backend emitted
            // `uint64_t c = 0; ... c = src.cur;`: "cannot convert
            // 'Beat' to 'uint64_t' in assignment". (v1 emitted
            // `int64_t c = self.src.cur;`, uncompilable the same way.)
            .or_else(|| self.record_id_of_expr(&e).map(IrType::Record));
        // …but a record RHS under a DECLARED SCALAR type is a
        // disagreement, not an inference. `record_ty` wins the `.or`
        // chain below, so without this the annotation is discarded
        // silently: `let c : uint<8> = s.cur` declared `Beat c{}` and
        // the DEFAULT backend laundered the type with no diagnostic,
        // while v1 emitted `uint64_t c = _tb.s.cur;` — "cannot convert
        // 'Beat' to 'uint64_t' in initialization". Now that the RHS
        // types definitively, the disagreement is visible and belongs
        // to neither backend.
        if let (Some(IrType::Record(rid)), Some(_)) = (&record_ty, &l.ty) {
            // Reaching the untyped tail with an annotation still on the
            // `let` means every annotation-specific path declined it —
            // including the record-typed one above, which claims
            // `let c : Beat = <Beat>` and rejects `let c : Beat = <not
            // a Beat>`. So an annotation here disagrees with a record
            // RHS by construction.
            //
            // Keyed on `l.ty`, not on `declared_scalar_ty`:
            // `typed_let_ir_type` answers `None` for `int` and for
            // `bit` (which parses as a `Named` type), and both spellings
            // then kept discarding the annotation — tbir declared
            // `Beat c{}` while v1 emitted `uint64_t c = _tb.s.cur;` and
            // `Vbit* c = _tb.s.cur;`, neither of which compiles.
            //
            // The message does not re-render the annotation: this
            // context holds an `IrType` at best, and `bits<16>`,
            // `time` and `bit` all collapse into or past it, so any
            // reconstruction would quote a type the user did not write.
            return Err(LowerError::Invalid(format!(
                "`let {}` is declared with a non-record type and initialised from a `{}`",
                l.name.name,
                self.ctx.records[rid.index()].name
            )));
        }
        // Untyped wide-scalar RHS (`let t96 = s128.trunc<96>()`): when the
        // RHS infers to a >64-bit `uint`/`sint`, propagate that width so the
        // local declares as `_harc_u128` (`local_scalar_cty`) rather than
        // silently clamping to `uint64_t`. Mirrors v1's `auto t96 = ...`,
        // whose deduced type is `_harc_u128`. ≤64-bit RHS keeps the existing
        // `uint64_t` default untouched.
        let wide_scalar_ty = self
            .expr_type(&e)
            .filter(|t| matches!(t, IrType::UInt(Some(w)) | IrType::SInt(Some(w)) if *w > 64));
        // Untyped signed-scalar RHS (`let d = NEG` where NEG is a `sint`
        // const): v1's `auto` deduces `int64_t` from the signed
        // expression, so the local must carry signedness or `d >> 1` /
        // `d / 2` silently go unsigned (#524 adversarial-review finding
        // 6). A declared type still wins via the `.or` chain below.
        let signed_scalar_ty = self.expr_type(&e).filter(|t| {
            matches!(t, IrType::SInt(None)) || matches!(t, IrType::SInt(Some(w)) if *w <= 64)
        });
        let id = self.declare(&l.name.name);
        if let Some(w) = declared_width {
            self.let_widths.insert(id, w);
        }
        if let Some(ty) = record_ty
            .or(declared_scalar_ty)
            .or(wide_scalar_ty)
            .or(signed_scalar_ty)
        {
            self.set_local_type(id, ty);
        }
        self.check_scalar_assign_width(id, &e, &l.name.name)?;
        self.push(Stmt::Assign(id, e));
        Ok(())
    }

    /// Divergence 104's rule, stated once and asked from two places.
    ///
    /// A `let` whose declared type names a REGBLOCK or ADDRMAP is an
    /// instantiation, and one without `= bind <helper>` has no bus for
    /// its registers to reach. The declaration guard in `lower_let`
    /// enforces it — but the queue-pop lanes run BEFORE that guard, so
    /// `let z : DmaRegs = sb.q.pop()` reached a pop lane first. The
    /// pop lanes ask here so the actionable message wins wherever the
    /// spelling lands.
    fn regblock_instantiation_error(&self, l: &crate::ast::LetStmt) -> Option<LowerError> {
        let TypeExpr::Named { name, .. } = l.ty.as_ref()? else {
            return None;
        };
        let simple = name.segments.last().map(|s| s.name.as_str())?;
        self.ctx.regblock_instance_types.contains(simple).then(|| {
            LowerError::Invalid(format!(
                "`let {} : {simple}` instantiates a register block without a bus: a \
                 regblock/addrmap instantiation requires `= bind <helper>` (a \
                 transactor with write/read methods)",
                l.name.name
            ))
        })
    }

    /// A `let <name> [: T] = <queue>.pop()` types its local from the
    /// QUEUE ELEMENT, which is right — but only if the annotation
    /// agrees. All three pop lanes (component queue, scoreboard queue,
    /// target-state queue) discarded a disagreeing `T` in silence, so
    /// `let b : Other = sb.q.pop()` on a `queue<Beat>` declared `Beat`
    /// and RAN, while v1's `Other b = _tb.sb.q.pop();` gets "conversion
    /// from 'Beat' to non-scalar type 'Other' requested". Same defect
    /// as the untyped-`let` one a screen down, at the spelling that
    /// already had a type to check against.
    fn check_pop_let_type(
        &self,
        l: &crate::ast::LetStmt,
        elem: &crate::ir::QueueElem,
        what: &str,
    ) -> Result<(), LowerError> {
        let Some(ty) = l.ty.as_ref() else {
            return Ok(());
        };
        // A REGBLOCK name is in `record_ids` too — its mirror record is
        // filed under the regblock's own name — and these lanes run
        // BEFORE divergence 104's instantiation guard. Answering from
        // this function would replace its actionable message with a
        // sentence about queue element types; declining outright would
        // let the program lower. Ask the rule directly.
        if let Some(e) = self.regblock_instantiation_error(l) {
            return Err(e);
        }
        let declared_record = match ty {
            TypeExpr::Named { name, .. } => name
                .segments
                .last()
                .and_then(|seg| self.ctx.record_ids.get(seg.name.as_str()).copied()),
            _ => None,
        };
        let got = match elem {
            crate::ir::QueueElem::Record(rid) => {
                if declared_record == Some(*rid) {
                    return Ok(());
                }
                format!("a `{}`", self.ctx.records[rid.index()].name)
            }
            crate::ir::QueueElem::Scalar { .. } => {
                if declared_record.is_none() {
                    return Ok(());
                }
                "a scalar".to_string()
            }
        };
        let want = match declared_record {
            Some(rid) => format!("a `{}`", self.ctx.records[rid.index()].name),
            None => "a scalar".to_string(),
        };
        Err(LowerError::Invalid(format!(
            "`let {}` is declared as {want} and {what} yields {got}",
            l.name.name
        )))
    }

    /// Reject a narrowing scalar assignment at lowering, where it can
    /// carry a source-level fix.
    ///
    /// The verifier's invariant 15 already rejects `let b : uint<200> = a`
    /// on a 256-bit `a` — but that channel reports compiler bugs, so the
    /// user saw `internal error: TB-IR failed verification after
    /// lowering` for a program `harc check` had just accepted. v1 had no
    /// diagnostic at all and emitted `HarcWide<7> b = a;`, which does not
    /// compile (narrowing between word counts is deliberately not a
    /// conversion). Both now name the offending widths and the fix.
    fn check_scalar_assign_width(
        &self,
        dest: crate::ir::LocalId,
        e: &Expr,
        name: &str,
    ) -> Result<(), LowerError> {
        // Only the expression shapes invariant 15's own `expr_type`
        // resolves — exactly the set that would otherwise reach the
        // internal-error channel, so this adds no rejection the verifier
        // was not already making. A binary/ternary RHS is deliberately
        // excluded: lowering's `expr_type` over-approximates one as its
        // left operand's declared width, which would reject a provably
        // narrowed value such as `(wide & 0xFF) >> 4`. Pure-helper and
        // extern calls are included because their CallTarget carries the
        // exact declared return type.
        if !matches!(
            e,
            Expr::Literal { .. }
                | Expr::WideLiteral(_)
                | Expr::Local(_)
                | Expr::BitSlice { .. }
                | Expr::WidthCast { .. }
                | Expr::Call(
                    crate::ir::CallTarget::Helper { .. } | crate::ir::CallTarget::ExternFn { .. },
                    _,
                )
        ) {
            return Ok(());
        }
        // Signedness, not just width: invariant 15's `assign_compatible`
        // also requires the two to agree, so `let s : sint<8> = a +% b`
        // (a wrap's residue is unsigned per spec §2.4) reached the
        // internal-error channel for a program `harc check` accepts.
        let (dw, d_signed) = match self.local_type(dest) {
            IrType::UInt(Some(w)) => (*w, false),
            IrType::SInt(Some(w)) => (*w, true),
            _ => return Ok(()),
        };
        let (aw, a_signed) = match self.expr_type(e) {
            Some(IrType::UInt(Some(w))) => (w, false),
            Some(IrType::SInt(Some(w))) => (w, true),
            _ => return Ok(()),
        };
        if a_signed != d_signed {
            let (article, from, to) = if a_signed {
                ("a", "signed", "unsigned")
            } else {
                ("an", "unsigned", "signed")
            };
            return Err(LowerError::Invalid(format!(
                "assignment of {article} {from} {aw}-bit value to `{name}`, declared \
                 {to} {dw} bits. Signedness must match — relabel the value \
                 explicitly with `as {}<{dw}>`.",
                if d_signed { "sint" } else { "uint" }
            )));
        }
        if aw > dw {
            return Err(LowerError::Invalid(format!(
                "assignment of a {aw}-bit value to `{name}`, declared {dw} bits, \
                 narrows. Widths must not shrink implicitly — use \
                 `.trunc<{dw}>()` to narrow explicitly, or widen the \
                 declaration to {aw} bits."
            )));
        }
        Ok(())
    }

    fn check_component_method_result_assignable(
        &self,
        method_schema: &crate::ir::ComponentMethodSchema,
        expected: IrType,
        method: &str,
        local: &str,
    ) -> Result<(), LowerError> {
        if let Some(actual) = method_schema.ret_ty.clone() {
            if !component_method_result_compatible(&expected, &actual) {
                return Err(unsupported(
                    &format!(
                        "assignment of `{method}` result to local `{local}` with incompatible type"
                    ),
                    format!("expected {expected:?}, method returns {actual:?}"),
                ));
            }
        }
        Ok(())
    }

    /// The mirror of `record_assign_mismatch`: a SCALAR destination
    /// whose RHS types to a record.
    ///
    /// The record-destination direction has been guarded arm by arm;
    /// this direction was guarded at exactly one site (a scalar
    /// transactor-state field). Everywhere else — a DUT port, a
    /// testbench field, a scoreboard counter, a scalar component field,
    /// a scalar record field or `Vec` element, a record-state scalar
    /// subfield, a scalar local — `x = <record>` LOWERED, VERIFIED and
    /// emitted from the DEFAULT backend, where g++ answers "cannot
    /// convert 'Beat' to 'uint64_t' in assignment". v1 emits the same
    /// line and fails the same way, so no backend runs it.
    ///
    /// Only callable now that `record_id_of_expr` types every
    /// record-carrying RHS: before that a `None` here could mean
    /// "could not tell".
    pub(crate) fn reject_record_into_scalar(
        &self,
        e: &crate::ir::Expr,
        what: &str,
    ) -> Result<(), LowerError> {
        match self.record_id_of_expr(e) {
            Some(rid) => Err(LowerError::Invalid(format!(
                "{what} is a scalar and the value assigned to it is a `{}`",
                self.ctx.records[rid.index()].name
            ))),
            None => Ok(()),
        }
    }

    /// Each argument of a component-method call must match its
    /// parameter's type. The lambda renders as
    /// `<Comp>_<method>(Comp&, uint64_t)` or `(Comp&, <Record>)`, and
    /// handing it the other kind is "no match for call to ..." in both
    /// backends — measured on a scalar parameter given a record, and a
    /// record parameter given a scalar and given a different record.
    ///
    /// Arity IS checked here, first. The comment that used to sit in
    /// this spot claimed a wrong count was "a separate diagnostic" —
    /// there was none, and `zip` silently stopped at the shorter side,
    /// so an under-supplied call disabled the type check on every
    /// parameter past the last supplied one. Both backends refuse the
    /// wrong-arity call, so it is `Invalid`.
    pub(crate) fn check_component_call_args(
        &self,
        component: crate::ir::ComponentId,
        method: &str,
        args: &[crate::ir::Expr],
    ) -> Result<(), LowerError> {
        let Some(m) = self.ctx.components[component.index()].method(method) else {
            return Ok(());
        };
        let comp_name = &self.ctx.components[component.index()].name;
        if args.len() != m.param_tys.len() {
            return Err(LowerError::Invalid(format!(
                "component method `{comp_name}.{method}` takes {} argument(s), call passes {}",
                m.param_tys.len(),
                args.len()
            )));
        }
        for (i, (a, ty)) in args.iter().zip(m.param_tys.iter()).enumerate() {
            let pname = m
                .param_names
                .get(i)
                .cloned()
                .unwrap_or_else(|| format!("#{}", i + 1));
            // `param_tys` already carries the schema's `TSeq<T>` as a
            // `RecordSeq`/`Seq`, so reading it through `check_slot_ir`
            // rather than collapsing it to "record or not" is what lets
            // `k.feed(1)` on a `TSeq<Beat>` parameter be rejected at all
            // — and stops the `Beat` case from being told the slot takes
            // a non-record value.
            self.check_slot_ir(
                a,
                ty,
                &format!("parameter `{pname}` of `{comp_name}.{method}`"),
            )?;
        }
        Ok(())
    }

    /// A value entering a TYPED SLOT — a queue element, a method
    /// parameter, an event payload — must match that slot's type.
    ///
    /// `want` is the slot's record, or `None` for a scalar slot. Both
    /// directions are wrong and both were open: a record into a scalar
    /// slot, and a scalar (or a DIFFERENT record) into a record slot.
    ///
    /// This is the assignment rule at the other half of the surface.
    /// Every combination was measured on both backends, and only the
    /// matching one compiles — for instance on a `queue<Beat>`,
    /// `q.push(b)` compiles while `q.push(1)` gets "cannot convert
    /// 'long unsigned int' to 'Beat'" and `q.push(o)` "cannot convert
    /// 'Other' to 'Beat'". So `Invalid` throughout: no backend runs any
    /// of them, and there is nothing for a future TB-IR to implement.
    ///
    /// For a slot that is a sequence rather than a record-or-scalar, use
    /// `check_slot_ir`: this spelling would describe a `TSeq<Beat>`
    /// parameter as taking "a non-record value", which is false.
    pub(crate) fn check_slot_type(
        &self,
        value: &crate::ir::Expr,
        want: Option<crate::ir::RecordId>,
        what: &str,
    ) -> Result<(), LowerError> {
        self.check_slot_shape(value, Slot::of_record(want), what)
    }

    /// `check_slot_type` for a slot whose declared `IrType` is in hand —
    /// the method-parameter positions, where the declaration may name a
    /// `TSeq<T>` as well as a record or a scalar.
    pub(crate) fn check_slot_ir(
        &self,
        value: &crate::ir::Expr,
        want: &IrType,
        what: &str,
    ) -> Result<(), LowerError> {
        self.check_slot_shape(value, Slot::of_ir(want), what)
    }

    fn check_slot_shape(
        &self,
        value: &crate::ir::Expr,
        want: Slot,
        what: &str,
    ) -> Result<(), LowerError> {
        // A record-valued operand under an operator that does not
        // PRODUCE a record — `s.obs(b + 1)`, or a ternary whose arms name
        // different records. `expr_type` propagates its operand's type
        // through `Binary`/`Unary`/`Ternary` without asking whether the
        // operator preserves record-ness, while `record_id_of_expr` does
        // ask (its ternary arm requires both arms to name the SAME
        // record). So the two disagreeing is not noise — it is exactly
        // the signature of a malformed record expression, and reading
        // the value's shape off `expr_type` alone made the diagnostic
        // assert that `b + 1` IS a `Beat`. That is the same absence
        // dressed as a claim this rule exists to stop, mirrored onto the
        // value side.
        //
        // Measured on both backends with a compiling control
        // (`s.obs(b)`): `s.obs(b + 1)` gives "no match for `operator+`
        // (operand types are `Beat` and `int`)" and the mismatched
        // ternary "operands to `?:` have different types `Beat` and
        // `Other`", from v1 and tbir alike. No backend runs it.
        if self.record_id_of_expr(value).is_none()
            && matches!(self.expr_type(value), Some(IrType::Record(_)))
        {
            return Err(LowerError::Invalid(format!(
                "{what} was given a record-valued operand in a position that does not \
                 produce a record (an arithmetic or bitwise operator, or a `?:` whose \
                 arms name different records)"
            )));
        }
        let got = self.slot_shape_of(value);
        if got == want || !want.known() || !got.known() {
            return Ok(());
        }
        Err(LowerError::Invalid(format!(
            "{what} takes {} and was given {}",
            self.slot_name(want),
            self.slot_name(got)
        )))
    }

    /// The slot shape a lowered value presents. `record_id_of_expr`
    /// answers the record question on its own terms; everything else is
    /// read off the value's `IrType`, and a type this compiler could not
    /// infer stays `Unknown` rather than being called a scalar. An
    /// untyped local is the single most common way a correct program
    /// reached the old `Scalar` fallback and got rejected for it.
    fn slot_shape_of(&self, value: &crate::ir::Expr) -> Slot {
        if let Some(rid) = self.record_id_of_expr(value) {
            return Slot::Record(rid);
        }
        // A literal is a scalar whatever its `ty` says. Bare integer
        // literals carry `ty: Unknown` (the width is inferred at the
        // use site), so reading their shape off `expr_type` alone would
        // make `q.push(1)` into a `queue<Beat>` unknown — and that cell
        // is the one this whole family started from.
        if matches!(
            value,
            crate::ir::Expr::Literal { .. } | crate::ir::Expr::WideLiteral(_)
        ) {
            return Slot::Scalar;
        }
        match self.expr_type(value) {
            // UNREACHABLE from the one caller, and kept so the
            // condition has one verdict everywhere it is written.
            // `record_id_of_expr` is the authority on records and has
            // already said no by the time this runs, so a `Record` here
            // is the two sources disagreeing — and `check_slot_shape`
            // turns that disagreement into a rejection before it calls
            // this. Confirmed by mutation: deleting this arm fails no
            // test. It stays because the alternative is a line that
            // silently re-asserts the record-ness the caller just
            // refused, if the order of those two ever changes.
            Some(IrType::Record(_)) => Slot::Unknown,
            Some(ty) => Slot::of_ir(&ty),
            None => Slot::Unknown,
        }
    }

    fn slot_name(&self, s: Slot) -> String {
        match s {
            Slot::Record(rid) => format!("a `{}`", self.ctx.records[rid.index()].name),
            Slot::Seq(Some(rid)) => {
                format!("a `TSeq<{}>`", self.ctx.records[rid.index()].name)
            }
            Slot::Seq(None) => "a scalar `TSeq`".to_string(),
            Slot::Scalar => "a non-record value".to_string(),
            // Unreachable: `check_slot_shape` returns `Ok` before
            // building a message when either side is `Unknown`.
            Slot::Unknown => "a value of unknown type".to_string(),
        }
    }

    /// `check_slot_type` for a queue element.
    pub(crate) fn check_queue_push(
        &self,
        value: &crate::ir::Expr,
        elem: &crate::ir::QueueElem,
        what: &str,
    ) -> Result<(), LowerError> {
        let want = match elem {
            crate::ir::QueueElem::Record(rid) => Some(*rid),
            crate::ir::QueueElem::Scalar { .. } => None,
        };
        self.check_slot_type(value, want, what)
    }

    /// The verdict for a whole-record assignment whose RHS did not type
    /// to the destination's record.
    ///
    /// Always `Invalid` — but only because every RHS shape that reaches
    /// here now types DEFINITIVELY. The first version of this guard
    /// read `record_id_of_expr`, whose `None` meant two different
    /// things: an expression that is not a record (a literal, an
    /// arithmetic result), and one this context could not type. That
    /// made `drv.rec = src.cur` — a whole-record copy from a
    /// record-typed component field, which BOTH backends compile — a
    /// type error. The second version kept an escape hatch for the
    /// shapes it could not type, which promised `--codegen v1` for
    /// `b = src.n` and `b = count`, which v1 cannot compile either.
    ///
    /// The fix is upstream of the verdict: `record_id_of_expr` now
    /// types every record-carrying shape (`component_field_record`
    /// walks a dotted component-field path three-valued, `Ternary`
    /// resolves to its arms' common record), so a `None` here really
    /// does mean "not a record".
    fn record_assign_mismatch(
        &self,
        e: &crate::ir::Expr,
        want: crate::ir::RecordId,
        what: String,
        hint: &str,
    ) -> LowerError {
        let want_name = &self.ctx.records[want.index()].name;
        let got = match self.record_id_of_expr(e) {
            Some(other) => format!("a `{}`", self.ctx.records[other.index()].name),
            None => "not a record".to_string(),
        };
        LowerError::Invalid(format!(
            "{what}: it is a `{want_name}` and the value assigned to it is {got} — {hint}"
        ))
    }

    fn lower_assign(
        &mut self,
        target: &crate::ast::Expr,
        value: &crate::ast::Expr,
    ) -> Result<(), LowerError> {
        if let Some(port) = self.as_port_ref(target)? {
            // Writing a read-only `probe` is a hard error: only a
            // `probe force` declaration opts into the SV procedural-force
            // write path. Point the user at the `force` modifier (mirrors
            // v1's `emit_signal_assignment` read-only-probe rejection).
            if matches!(port.access, crate::ir::PortAccess::Probe) {
                return Err(LowerError::Invalid(format!(
                    "write to `dut.{}`: read-only probe — declare with \
                     `probe force` to enable fault injection",
                    port.port_path.join("."),
                )));
            }
            let e = self.lower_expr(value)?; // ports allowed in DutWrite values
            let e = self.hoist_transactor_calls(e);
            self.reject_record_into_scalar(&e, &format!("`dut.{}`", port.port_path.join(".")))?;
            self.push(Stmt::DutWrite(port, e));
            return Ok(());
        }
        // Bus-bound signal write: `axil.aw.valid = 1`.
        if let Some(port) = self.as_bus_port_ref(target)? {
            let e = self.lower_expr(value)?;
            let e = self.hoist_transactor_calls(e);
            self.reject_record_into_scalar(&e, &format!("`{}`", port.port_path.join(".")))?;
            self.push(Stmt::DutWrite(port, e));
            return Ok(());
        }
        // Constant-lane DUT port write: `dut.lane_id_in[1] = 9`.
        if let Some(port) = self.as_lane_port_ref(target)? {
            let e = self.lower_expr(value)?;
            let e = self.hoist_transactor_calls(e);
            self.reject_record_into_scalar(&e, &format!("`{}`", port.port_path.join(".")))?;
            self.push(Stmt::DutWrite(port, e));
            return Ok(());
        }
        // Scalar testbench field write: `_tb.expected = 3`.
        if let Some(field) = self.as_tb_scalar_field(target) {
            let e = self.lower_expr_no_ports(value)?;
            self.reject_record_into_scalar(&e, &format!("testbench field `{field}`"))?;
            self.push(Stmt::TbFieldWrite { field, value: e });
            return Ok(());
        }
        // Scalar testbench host state (`expected_checks = ...`) and
        // promoted test-scope lets write the shared `_tb` cell. Locals
        // shadow; free helpers stay fenced by the capture-scope helper.
        if let crate::ast::ExprKind::Ident(id) = &*target.kind {
            if self.lookup(&id.name).is_none() {
                if let Some(field) = self.tb_scalar_field_in_capture_scope(&id.name) {
                    let e = self.lower_expr_no_ports(value)?;
                    self.reject_record_into_scalar(&e, &format!("testbench field `{field}`"))?;
                    self.push(Stmt::TbFieldWrite { field, value: e });
                    return Ok(());
                }
            }
        }
        // Subfield write of a bound-to target responder's whole-record
        // state field: `last.addr = addr` (responder body) /
        // `responder.last.addr = ...` (test). Checked before the whole-
        // record `as_transactor_state` write lane, which only fires when
        // there is no further subfield.
        if let Some(chain) = self.as_transactor_state_record_field(target)? {
            if let Some(len) = chain.leaf_vec_len {
                let dotted = format!("{}.{}", chain.field, chain.path.join("."));
                // A matching-shape copy lowers, as it does in the other
                // two lanes: v1 emits `target.ba.data = target.bb.data;`
                // and g++ accepts it (measured, 0 errors), so refusing
                // it was a gap rather than a subset boundary. "Matching"
                // is `ir_vec_elem_class`, the C++ member type — which
                // makes `Vec<uint<8>, 4> = Vec<uint<32>, 4>` a copy too,
                // exactly as v1 renders it.
                let shape =
                    crate::codegen::cpp_tb::ir_vec_elem_class(&chain.leaf_ty).map(|cls| (len, cls));
                let rhs = match shape {
                    Some(sh) => self.whole_vec_copy_rhs(sh, value)?,
                    None => None,
                };
                let Some(rhs) = rhs else {
                    // What is left is uniformly uncompilable under v1,
                    // measured: a length mismatch and a scalar RHS each
                    // give "no match for `operator=`" on the
                    // `std::array` member. The `--codegen v1` this used
                    // to promise was a dead end.
                    return Err(not_implemented(
                        &format!(
                            "a whole-`Vec` write of record state field `{dotted}` \
                             with a non-matching RHS"
                        ),
                        // Real names: this detail was a plain string, so
                        // `{field}.{vec}` printed the braces at the user.
                        format!("assign the field element-wise (`{dotted}[i] = ...`)"),
                        V1Status::EmitsUncompilable,
                    ));
                };
                self.push(Stmt::TransactorStateRecordFieldWrite {
                    instance: chain.instance,
                    field: chain.field,
                    path: chain.path,
                    value: rhs,
                });
                return Ok(());
            }
            let e = self.lower_expr_no_ports(value)?;
            self.reject_record_into_scalar(
                &e,
                &format!("`{}.{}`", chain.field, chain.path.join(".")),
            )?;
            self.push(Stmt::TransactorStateRecordFieldWrite {
                instance: chain.instance,
                field: chain.field,
                path: chain.path,
                value: e,
            });
            return Ok(());
        }
        // Test-scope write of a bound-to target responder's persistent
        // state field: `target.read_count = 0` (scalar) or a whole-record
        // copy `target.last = <same-typed record>`.
        if let Some((instance, field)) = self.as_transactor_state(target) {
            let e = self.lower_expr_no_ports(value)?;
            // The SAME record-type check the bare-name lane below makes,
            // which this lane did not. Without it `drv.rec = 5` lowered,
            // verified and emitted `drv.rec = 5;` — as uncompilable as
            // v1's `_tb.drv.rec = 5;` ("no match for 'operator=', operand
            // types are 'Beat' and 'int'"). The bare-name spelling of the
            // same write was rejected, so the hole was reachable only
            // from test scope, which is where a user writes it.
            // The mirror of the same hole, one line away: a SCALAR
            // state field assigned a record. v1 emits `drv.st = b;` and
            // g++ refuses it ("cannot convert 'Beat' to 'uint64_t' in
            // assignment"); TB-IR lowered, verified and emitted the same
            // line.
            if self.target_state_record(&instance, &field).is_none() {
                if let Some(rid) = self.record_id_of_expr(&e) {
                    return Err(LowerError::Invalid(format!(
                        "`{instance}.{field}` is a scalar state field and the value \
                         assigned to it is a `{}`",
                        self.ctx.records[rid.index()].name
                    )));
                }
            }
            if let Some(record) = self.target_state_record(&instance, &field) {
                if self.record_id_of_expr(&e) != Some(record) {
                    return Err(self.record_assign_mismatch(
                        &e,
                        record,
                        format!("`{instance}.{field}`"),
                        "assign a value of the same record type, or set the record fields \
                         individually (`<inst>.<field>.<sub> = ...`)",
                    ));
                }
            }
            self.push(Stmt::TransactorStateWrite {
                instance,
                field,
                value: e,
            });
            return Ok(());
        }
        // Event-driven transactor DUT bind (`drv.dut = dut` /
        // `env.drv.dut = dut`): the component's `Dut` handle field is bound
        // to the test DUT. Erased like the `TransactorSchema` DUT bind —
        // the on-handler body's `DutWrite`s already resolve to the test's
        // `dut` pointer (tbir's `port_signal` emits `dut->...`), so the
        // copy is cosmetic; v1 emits `_tb.drv.dut = dut;` with no
        // observable effect on a well-formed program.
        if self.lower_component_dut_bind(target, value)? {
            return Ok(());
        }
        if let Some((_, field, _)) = self.as_component_vec_field(target)? {
            return Err(unsupported(
                &format!("whole-vector write to component field `{field}`"),
                "write one element with `<field>[index]`; whole-vector assignment is not lowered yet",
            ));
        }
        // Composite-component scalar/record-leaf field write — self-relative
        // inside a method body (`count = ...`) or a dotted path from a
        // test-scope component local (`env.src.current.value = ...`). Record
        // leaves must be claimed before whole-sub-component assignment:
        // otherwise that resolver mistakes `current` for a `Sub` receiver.
        if let Some(tgt) = self.as_component_field_target(target)? {
            let (base, field) = (tgt.base, tgt.field);
            // A whole-`Vec` component record field takes a whole-`Vec`
            // read of the same `std::array<elem, N>` shape — the third
            // spelling of the copy the record-local and responder-state
            // lanes already lower. v1 emits
            // `self.a.data = self.b.data;` and g++ accepts it (0 errors,
            // measured), so refusing it was a gap.
            if let Some((len, elem)) = tgt.leaf_vec {
                let shape = crate::codegen::cpp_tb::ir_vec_elem_class(&elem).map(|c| (len, c));
                let rhs = match shape {
                    Some(sh) => self.whole_vec_copy_rhs(sh, value)?,
                    None => None,
                };
                let Some(rhs) = rhs else {
                    // Everything else this arm covers has v1 emitting an
                    // assignment g++ refuses — `a.data = 5` gives "no
                    // match for `operator=` … `std::array<long unsigned
                    // int, 4>` and `int`", measured. The `--codegen v1`
                    // it used to promise was true only for the copy,
                    // which is no longer refused.
                    return Err(not_implemented(
                        &format!(
                            "a whole-`Vec` write of component record field `{}` with a non-matching RHS",
                            tgt.dotted
                        ),
                        format!("assign the field element-wise (`{}[i] = ...`)", tgt.dotted),
                        V1Status::EmitsUncompilable,
                    ));
                };
                self.push(Stmt::ComponentFieldWrite {
                    base,
                    field,
                    value: rhs,
                });
                return Ok(());
            }
            let e = self.lower_expr_no_ports(value)?;
            // A whole-record component field takes a record value. The
            // same rule as the transactor-state write above, at the
            // third spelling of it.
            match self.component_field_record(&base, &field) {
                Some(rid) => {
                    if self.record_id_of_expr(&e) != Some(rid) {
                        return Err(self.record_assign_mismatch(
                            &e,
                            rid,
                            format!("component record field `{field}`"),
                            "assign a value of the same record type, or set the record \
                             fields individually (`<comp>.<field>.<sub> = ...`)",
                        ));
                    }
                }
                // …and the mirror: a SCALAR component field taking a
                // record. Both directions of one rule; only the first
                // had a guard.
                None => {
                    self.reject_record_into_scalar(&e, &format!("component field `{field}`"))?
                }
            }
            self.push(Stmt::ComponentFieldWrite {
                base,
                field,
                value: e,
            });
            return Ok(());
        }
        // Composite-component whole-value copy of a sub-component:
        // `checker.sb = sb` / `responder.model = model`. The LHS terminal
        // field is a `Sub` component field; the RHS is a test-scope
        // component value.
        if self.lower_component_sub_assign(target, value)? {
            return Ok(());
        }
        // Scoreboard scalar-counter write: `sb.writes = sb.writes + 1`
        // (classic) / `_tb.sb.writes = ...` (impl-form, post-desugar).
        if let ExprKind::Field { target: ft, name } = &*target.kind {
            if let Some((sb, field, nested_path)) = self.scoreboard_root(ft) {
                let scalar = self.scoreboard_scalar_field(sb, &name.name)?;
                let e = self.lower_expr_no_ports(value)?;
                self.reject_record_into_scalar(&e, &format!("scoreboard field `{}`", name.name))?;
                self.push(Stmt::ScoreboardOp {
                    sb,
                    field,
                    op: crate::ir::ScoreboardOp::ScalarWrite { scalar, value: e },
                    nested_path,
                });
                return Ok(());
            }
        }
        // RAL frontdoor register-level write: `regs.NAME = expr`.
        if self.try_lower_regblock_write(target, value)? {
            return Ok(());
        }
        // RAL frontdoor field-level write: `regs.REG.FIELD = expr`.
        if self.try_lower_regblock_subfield_write(target, value)? {
            return Ok(());
        }
        // RAL addrmap write: `chip.inst.REG[.FIELD] = expr`.
        if self.try_lower_addrmap_write(target, value)? {
            return Ok(());
        }
        // A write to a regblock binding that is NOT a known register
        // (`regs.FOO = ...` for an undeclared register) is out of
        // subset — reject precisely rather than fall through and
        // mis-lower.
        self.reject_out_of_subset_regblock_access(target, "write")?;
        self.reject_out_of_subset_addrmap_access(target, "write")?;
        if self.lower_transactor_dut_bind(target, value)? {
            return Ok(());
        }
        if let ExprKind::Ident(id) = &*target.kind {
            if let Some(local) = self.lookup(&id.name) {
                // NOTE: `x = bus.m(...)` (bus call into an existing
                // local) is deliberately NOT lowered — v1 supports bus
                // calls only in `let`-RHS and statement position, and
                // the reference surface is v1's. The value lowering
                // below rejects it with a precise message.
                // `v = sb.q.pop()` — pop into an existing local.
                if let Some((sb, field, queue, nested_path)) = self.as_scoreboard_pop(value)? {
                    // The mirror, first: a SCALAR local taking a
                    // RECORD element. v1 emits `x = _tb.sb.q.pop();`
                    // under a `uint64_t x` — "cannot convert 'Beat' to
                    // 'uint64_t' in assignment" — and so did tbir. The
                    // `let` spelling of this exact program is already
                    // rejected by `check_pop_let_type`; the assignment
                    // spelling was the unchecked lane.
                    if self.record_of_local(local).is_none() {
                        if let crate::ir::QueueElem::Record(rid) =
                            self.scoreboard_queue_elem(sb, &queue)?
                        {
                            return Err(LowerError::Invalid(format!(
                                "local `{}` is a scalar and scoreboard queue `{queue}` \
                                 yields a `{}`",
                                id.name,
                                self.ctx.records[rid.index()].name
                            )));
                        }
                    }
                    if let Some(rid) = self.record_of_local(local) {
                        // Two very different programs under one arm.
                        // `queue<Beat>` popped into a `Beat` local is
                        // well typed, and v1 emits `b = _tb.sb.q.pop();`
                        // which compiles — a real escape hatch. Popping
                        // a `queue<uint<8>>` into the same local is a
                        // type error, and v1's identical line then gets
                        // "no match for 'operator=', operand types are
                        // 'Beat' and 'long unsigned int'".
                        if self.scoreboard_queue_elem(sb, &queue)?
                            != crate::ir::QueueElem::Record(rid)
                        {
                            return Err(LowerError::Invalid(format!(
                                "transaction local `{}` is a `{}`, and scoreboard queue \
                                 `{queue}` does not hold that record type",
                                id.name,
                                self.ctx.records[rid.index()].name
                            )));
                        }
                        return Err(unsupported(
                            &format!(
                                "assignment of a scoreboard `pop()` result to transaction \
                                 local `{}`",
                                id.name
                            ),
                            "pop into a fresh `let` instead; v1 emits the same struct copy \
                             and it compiles",
                        ));
                    }
                    self.push(Stmt::ScoreboardOp {
                        sb,
                        field,
                        op: crate::ir::ScoreboardOp::QueuePop { queue, dest: local },
                        nested_path,
                    });
                    return Ok(());
                }
                // `v = xact.method(...)` — call edge into an existing
                // local.
                if let ExprKind::Call { callee, args } = &*value.kind {
                    if let Some(call) = self.lower_transactor_call(callee, args, true)? {
                        if let Some(rid) = self.record_of_local(local) {
                            // Every method that reaches here returns a
                            // SCALAR: a record return type is refused
                            // upstream ("transactor method `<T>.<m>`
                            // return type"), so there is no well-typed
                            // program under this arm. v1 emits
                            // `b = Drv_get(_tb.drv);` and g++ refuses it
                            // ("operand types are 'Beat' and 'uint64_t'").
                            return Err(LowerError::Invalid(format!(
                                "transaction local `{}` is a `{}`, and a transactor method \
                                 returns a scalar",
                                id.name,
                                self.ctx.records[rid.index()].name
                            )));
                        }
                        self.push(Stmt::TransactorCall {
                            dest: Some(local),
                            call,
                        });
                        return Ok(());
                    }
                }
                // `v = env.source.next()` / `t = sqr.make_txn()` —
                // value-returning component method call into an existing
                // local. This mirrors the already-supported `let v =
                // env.source.next()` lowering, but preserves the destination
                // local's established scalar/record type.
                if let ExprKind::Call { callee, args } = &*value.kind {
                    if let Some((base, component, method)) =
                        self.as_component_method_call(callee)?
                    {
                        let comp = &self.ctx.components[component.index()];
                        // UNREACHABLE by construction, and kept only so the
                        // condition has one verdict everywhere it is
                        // written. `as_component_method_call` validates
                        // the method on EVERY path that returns
                        // `Ok(Some(..))` — the two that error do so
                        // themselves, the two that do not are guarded by
                        // `is_some()` — so by the time a caller holds
                        // `(base, component, method)` the method exists.
                        // Confirmed by mutation: neutering this arm
                        // fails no test, because nothing reaches it.
                        // The reachable landing is the resolver's own
                        // arm in `components.rs`, which is where the
                        // measurement lives.
                        let m = comp.method(&method).ok_or_else(|| {
                            LowerError::Invalid(format!(
                                "component `{}` has no method `{method}`",
                                comp.name
                            ))
                        })?;
                        if !m.has_ret {
                            // REACHED, contrary to an earlier note here:
                            // `let x : uint<32> = 0` then
                            // `x = c.noret(3)` lands on this arm, since
                            // an assignment is not a `let` and nothing
                            // claims it first. v1: "void value not
                            // ignored as it ought to be".
                            return Err(LowerError::Invalid(format!(
                                "assignment from `{}.{method}(...)` — method \
                                 `{method}` returns no value",
                                comp.name
                            )));
                        }
                        let expected = self.local_type(local).clone();
                        self.check_component_method_result_assignable(
                            m, expected, &method, &id.name,
                        )?;
                        let declared = m.param_names.clone();
                        let lowered = self.lower_component_call_args(args, Some(&declared))?;
                        self.check_component_call_args(component, &method, &lowered)?;
                        self.push(Stmt::ComponentCall {
                            base,
                            component,
                            method,
                            args: lowered,
                            dest: Some(local),
                        });
                        return Ok(());
                    }
                }
                let e = self.lower_expr_no_ports(value)?;
                // Whole-record assignment: only a same-typed record
                // value copies (`t = u`, `t = s.inner`, or a
                // `t = tbl.entries[i]` element read — C++ struct
                // assignment in both backends). Anything else would
                // otherwise surface as a verifier TypeMismatch (the
                // internal-bug channel) or a C++ compile error.
                match self.record_of_local(local) {
                    Some(rid) => {
                        if self.record_id_of_expr(&e) != Some(rid) {
                            // A type error rather than a subset gap: v1
                            // emits `b = o;` / `b = 5;` and g++ refuses
                            // both ("no match for 'operator='"). Nothing
                            // a future TB-IR could sensibly implement,
                            // and `--codegen v1` does not run it either.
                            return Err(self.record_assign_mismatch(
                                &e,
                                rid,
                                format!("transaction local `{}`", id.name),
                                "assign fields individually, or copy from a same-typed \
                                 transaction local",
                            ));
                        }
                    }
                    // …and the mirror. `x = b` on a scalar `x` reached
                    // the VERIFIER's `TypeMismatch`, which `main.rs`
                    // renders as "internal error: TB-IR failed
                    // verification after lowering" — a compiler-bug
                    // report for a program error. The comment above
                    // named that channel as the thing this guard exists
                    // to keep programs out of, in one direction only.
                    None => self.reject_record_into_scalar(&e, &format!("local `{}`", id.name))?,
                }
                self.check_scalar_assign_width(local, &e, &id.name)?;
                self.push(Stmt::Assign(local, e));
                return Ok(());
            }
            // Persistent state-field write inside a bound-to target
            // responder body (`read_count = read_count + 1`). The
            // instance is a placeholder filled at the test-binding stage.
            // Only a scalar field is bare-assignable; a queue field is
            // mutated via `.push`/`.pop` (rejected here).
            if let Some(kind) = self.target_state_fields.get(&id.name).cloned() {
                match kind {
                    crate::ir::StateFieldKind::Scalar { .. } => {
                        let e = self.lower_expr_no_ports(value)?;
                        // The bare-name spelling of the write whose
                        // DOTTED spelling the mirror already guards.
                        // The polarity of the hole is reversed from the
                        // record direction — there the bare name was
                        // guarded and the dotted one was not — but it
                        // is the same one-lane-checked-one-not shape.
                        self.reject_record_into_scalar(&e, &format!("state field `{}`", id.name))?;
                        self.push(Stmt::TransactorStateWrite {
                            instance: String::new(),
                            field: id.name.clone(),
                            value: e,
                        });
                        return Ok(());
                    }
                    // Bare whole-record copy (`last = beat`): the RHS must
                    // be a value of the SAME record type (a same-typed
                    // record local or a whole nested-record read). Emits a
                    // C++ struct copy via `TransactorStateWrite`. Reject any
                    // other RHS rather than emit a bad assignment.
                    crate::ir::StateFieldKind::Record { record } => {
                        let e = self.lower_expr_no_ports(value)?;
                        if self.record_id_of_expr(&e) != Some(record) {
                            // A type error, not a subset gap: v1 emits
                            // the assignment and g++ refuses it ("no
                            // match for 'operator=', operand types are
                            // 'Beat' and 'int'"), so no backend runs it
                            // and `--codegen v1` is no help. The
                            // test-scope spelling of this write is
                            // guarded by the same rule above.
                            return Err(self.record_assign_mismatch(
                                &e,
                                record,
                                format!("state field `{}`", id.name),
                                "assign a value of the same record type, or set the record \
                                 fields individually (`<field>.<sub> = ...`)",
                            ));
                        }
                        self.push(Stmt::TransactorStateWrite {
                            instance: String::new(),
                            field: id.name.clone(),
                            value: e,
                        });
                        return Ok(());
                    }
                    crate::ir::StateFieldKind::Queue { .. } => {
                        return Err(unsupported(
                            &format!("bare assignment to the `queue` state field `{}`", id.name),
                            "mutate a queue state field via `.push(x)` / `.pop()`",
                        ));
                    }
                }
            }
            if self.in_check && self.ctx.test_scope_lets.contains(&id.name) {
                return Err(unsupported(
                    &format!("test-scope `let {}` referenced in the check phase", id.name),
                    "test-scope lets lower as run-function locals; run and check are \
                     separate functions in the IR, so v1's shared-capture scoping is \
                     not representable",
                ));
            }
            // Same as the read side: v1 emits `nosuchthing = 1;`
            // against an undeclared name.
            return Err(not_implemented(
                &format!("assignment to unknown name `{}`", id.name),
                "",
                V1Status::EmitsUncompilable,
            ));
        }
        // `t.field[i] = value` on a `Vec<T, N>` record field, at any nesting
        // depth (`s.a.b[i] = v`). Resolve the field chain, then lower the
        // index and value into an indexed `RecordFieldWrite`.
        if let ExprKind::Index { target: it, index } = &*target.kind {
            if let Some((base, field, vec)) = self.as_component_vec_field(it)? {
                let index = self.lower_expr_no_ports(index)?;
                super::exprs::check_literal_component_vec_index_bounds(
                    &base, &field, &index, vec.len,
                )?;
                let value = self.lower_expr_no_ports(value)?;
                self.reject_record_into_scalar(
                    &value,
                    &format!("element of component `Vec` field `{field}`"),
                )?;
                self.push(Stmt::ComponentVecElementWrite {
                    base,
                    field,
                    index,
                    value,
                });
                return Ok(());
            }
            // `a.data[i] = v` — element write to a `Vec<T, N>` LEAF
            // inside a component RECORD field. The read side of the same
            // spelling lowers through `ComponentVecElement`; without
            // this the write fell all the way to "assignment to a target
            // that is neither a DUT port nor a local", labelled
            // `SilentlyMisLowers` — the loudest verdict in the enum, and
            // false: v1 emits `self.a.data[0] = 1;` and g++ accepts it.
            if let Some(rf) = self.as_component_record_field(it)? {
                if let Some((len, elem)) = rf.leaf_vec.clone() {
                    let index = self.lower_expr_no_ports(index)?;
                    super::exprs::check_literal_component_vec_index_bounds(
                        &rf.base, &rf.dotted, &index, len,
                    )?;
                    let value = self.lower_expr_no_ports(value)?;
                    // BOTH halves of the guard pair the record-local
                    // element write carries, not just the first. A
                    // `Vec<Record, N>` leaf takes a value of the
                    // ELEMENT's record type and nothing else: without
                    // the second half `a.kids[0] = 5` lowered into
                    // `self.a.kids[0] = 5;`, which g++ refuses ("no
                    // match for `operator=` … `Kid` and `int`") — a
                    // program cleanly refused before this lane existed.
                    if let IrType::Record(elem_rid) = elem {
                        if self.record_id_of_expr(&value) != Some(elem_rid) {
                            return Err(self.record_assign_mismatch(
                                &value,
                                elem_rid,
                                format!(
                                    "an element of component `Vec` record field `{}`",
                                    rf.dotted
                                ),
                                "assign a value of the element's record type, or set the \
                                 element's fields individually",
                            ));
                        }
                    }
                    self.reject_record_into_scalar(
                        &value,
                        &format!("element of component `Vec` field `{}`", rf.dotted),
                    )?;
                    self.push(Stmt::ComponentVecElementWrite {
                        base: rf.base,
                        field: rf.field,
                        index,
                        value,
                    });
                    return Ok(());
                }
            }
            if let Some(chain) = self.try_record_field_chain(it)? {
                if chain.leaf_vec_len.is_none() {
                    // v1 emits `b.v[1] = 3;` — a subscript on a
                    // `uint64_t` member.
                    return Err(not_implemented(
                        &format!("indexing the scalar record field `{}`", chain.dotted),
                        "only `Vec<T, N>` record fields are indexable",
                        V1Status::EmitsUncompilable,
                    ));
                }
                let idx = self.lower_expr_no_ports(index)?;
                super::exprs::check_literal_vec_index_bounds(
                    &chain.dotted,
                    &idx,
                    chain.leaf_vec_len.unwrap_or(0),
                )?;
                let e = self.lower_expr_no_ports(value)?;
                // A `Vec<Record, N>` element store (`tbl.entries[i] = e`)
                // is a C++ struct copy: the RHS must be a whole record
                // value of the MATCHING element type (a same-typed record
                // local, a nested-record field read, or another element
                // read). Reject any other RHS rather than emit
                // `Entry = <scalar>` / a mismatched struct assignment.
                if !matches!(chain.leaf_ty, IrType::Record(_)) {
                    // The mirror: a SCALAR `Vec` element taking a
                    // record. v1 and tbir both emit
                    // `h.nums[1] = b;` — "cannot convert 'Beat' to
                    // 'std::array<...>::value_type'".
                    self.reject_record_into_scalar(
                        &e,
                        &format!("element of `Vec` field `{}`", chain.dotted),
                    )?;
                }
                if let IrType::Record(elem_rid) = chain.leaf_ty {
                    if self.record_id_of_expr(&e) != Some(elem_rid) {
                        // The same verdict its sibling one screen down
                        // reaches for the same program: v1 emits
                        // `tbl.entries[1] = 5;` against a
                        // `std::array<Entry, 4>` and g++ refuses it
                        // ("no match for 'operator='"). It had been an
                        // `Unsupported`, so it promised `--codegen v1`.
                        return Err(self.record_assign_mismatch(
                            &e,
                            elem_rid,
                            format!("an element of `Vec` record field `{}`", chain.dotted),
                            "assign a value of the element's record type, or set the \
                             element's fields individually",
                        ));
                    }
                }
                self.push(Stmt::RecordFieldWrite {
                    local: chain.local,
                    field: chain.field,
                    path: chain.path,
                    mid_indices: chain.mid_indices,
                    index: Some(idx),
                    value: e,
                });
                return Ok(());
            }
        }
        // `t.field = value` on a record-typed local (and nested
        // `s.a.b = v`). Resolve the destination field chain to its leaf.
        if let ExprKind::Field { .. } = &*target.kind {
            if let Some(chain) = self.try_record_field_chain(target)? {
                // A whole-`Vec` field write (`dst.data = src.data`) lowers
                // to `Stmt::RecordFieldWrite { index: None }` — the tbir
                // backend renders it as `name.field = e`, a plain
                // `std::array` copy, mirroring v1's C++ member assignment.
                // The ONLY admissible RHS is a whole-`Vec` read of a record
                // field with a MATCHING shape (same `vec_len`, same element
                // type/width). The RHS is lowered with the whole-`Vec`
                // read permission ON (`whole_vec_copy_rhs`), because a
                // whole-`Vec` read is exactly what an admissible RHS is
                // — the read arm refuses one by default, and this is one
                // of the two landings that lift that. Verify the shape
                // FIRST and reject any other RHS (scalar,
                // mismatched-shape field, …) with a structured
                // diagnostic, so the tbir backend never emits a
                // `std::array = <scalar>` miscompile. (Element writes
                // `rec.data[i] = v` are handled earlier.)
                if let Some(dst_len) = chain.leaf_vec_len {
                    let dst_dotted = chain.dotted.clone();
                    // The suggestion uses the user's spelling; the
                    // construct name uses the record-rooted one.
                    let dst_spelled = chain.spelled.clone();
                    // The ELEMENT-WIDTH mismatch that used to land here
                    // no longer does. v1 collapses every scalar of 64
                    // bits or fewer to `uint64_t`, so
                    // `Vec<uint<32>, 4> = Vec<uint<16>, 4>` emits
                    // `std::array<uint64_t, 4> = std::array<uint64_t, 4>`
                    // and compiles — the shape check below asks
                    // `ir_vec_elem_class`, which IS that collapse, so
                    // the copy lowers instead of being refused. Both
                    // backends emit `a.data = b.w16;`, measured.
                    //
                    // With that landing gone the arm is no longer mixed,
                    // and it stopped being entitled to `Unsupported`:
                    // every mismatch that still reaches it — a length
                    // mismatch, a signedness mismatch at or below 64
                    // bits, a record-vs-scalar element, a scalar RHS —
                    // has v1 emitting an assignment g++ refuses (one
                    // error each, measured on all four). There is no
                    // `--codegen v1` left to send anyone to.
                    let mismatch = || {
                        not_implemented(
                            &format!(
                                "a whole-`Vec` write of record field `{dst_dotted}` \
                                 with a non-matching RHS"
                            ),
                            // The user's own spelling, and a real
                            // `format!`: this detail was a plain string,
                            // so `{rec}.{field}` printed the braces at
                            // the user verbatim.
                            format!("assign the field element-wise (`{dst_spelled}[i] = ...`)"),
                            V1Status::EmitsUncompilable,
                        )
                    };
                    // The RHS goes through the SAME helper the other
                    // two write lanes use. Resolving it here with
                    // `try_record_field_chain` saw record LOCALS only,
                    // so a cross-lane RHS — `r.data = c.a.data`, a
                    // component record field — was reported as
                    // "non-matching" and, since this arm now claims
                    // `EmitsUncompilable`, told the user no backend runs
                    // it. v1 emits `r.data = c.a.data;` and g++ accepts
                    // it (0 errors, measured).
                    let shape = crate::codegen::cpp_tb::ir_vec_elem_class(&chain.leaf_ty)
                        .map(|cls| (dst_len, cls));
                    let rhs = match shape {
                        Some(sh) => self.whole_vec_copy_rhs(sh, value)?,
                        None => None,
                    };
                    let Some(rhs) = rhs else {
                        return Err(mismatch());
                    };
                    self.push(Stmt::RecordFieldWrite {
                        local: chain.local,
                        field: chain.field,
                        path: chain.path,
                        mid_indices: chain.mid_indices,
                        index: None,
                        value: rhs,
                    });
                    return Ok(());
                }
                // A whole nested-record field assignment (`o.a = d`): the
                // leaf is itself a record. The RHS must be a whole record
                // value of the MATCHING record type — a same-typed record
                // local or a whole nested-record field read. v1 emits a C++
                // struct copy (`name.field.p… = <rhs>;`). Reject any other
                // RHS (scalar, mismatched record, indexed element) rather
                // than emit a bad C++ assignment.
                if let IrType::Record(dst_rid) = chain.leaf_ty {
                    let rhs = self.lower_expr_no_ports(value)?;
                    if self.record_id_of_expr(&rhs) != Some(dst_rid) {
                        // v1 emits `o.p = q;` between two unrelated
                        // structs, which has no conversion — and there
                        // is nothing for a future TB-IR to implement,
                        // so `Invalid` rather than the
                        // `EmitsUncompilable` this arm carried before
                        // the RHS could be typed definitively.
                        return Err(self.record_assign_mismatch(
                            &rhs,
                            dst_rid,
                            format!("record field `{}`", chain.dotted),
                            "assign a value of the same record type, or set the nested \
                             fields individually",
                        ));
                    }
                    self.push(Stmt::RecordFieldWrite {
                        local: chain.local,
                        field: chain.field,
                        path: chain.path,
                        mid_indices: chain.mid_indices,
                        index: None,
                        value: rhs,
                    });
                    return Ok(());
                }
                // Scalar field write: RHS is an ordinary scalar expression,
                // lowered through the normal path — and it must actually
                // be one. The mirror of the record-leaf check above.
                let e = self.lower_expr_no_ports(value)?;
                self.reject_record_into_scalar(&e, &format!("record field `{}`", chain.dotted))?;
                self.push(Stmt::RecordFieldWrite {
                    local: chain.local,
                    field: chain.field,
                    path: chain.path,
                    mid_indices: chain.mid_indices,
                    index: None,
                    value: e,
                });
                return Ok(());
            }
        }
        // A whole-record TESTBENCH field write (`tbrec = b`). It is
        // NOT the catch-all's business, and the catch-all's verdict is
        // false for it in both directions: v1 emits `_tb.tbrec = b;`
        // for a same-typed RHS and g++ ACCEPTS it (so a working escape
        // hatch was being withheld under "silently emits something
        // else"), and emits `_tb.tbrec = 5;` for a mismatched one,
        // which g++ refuses (so it is a type error, not a silent
        // mis-lowering). Same split as every other record destination.
        if let Some(rid) = self.tb_record_field_target(target) {
            let e = self.lower_expr_no_ports(value)?;
            if self.record_id_of_expr(&e) != Some(rid) {
                return Err(self.record_assign_mismatch(
                    &e,
                    rid,
                    format!(
                        "testbench record field `{}`",
                        self.ctx.records[rid.index()].name
                    ),
                    "assign a value of the same record type, or set the record fields \
                     individually (`<field>.<sub> = ...`)",
                ));
            }
            // The copy itself is not lowered yet — but the suggestion
            // is honest now, because v1 does compile this one.
            return Err(unsupported(
                "a whole-record write of a testbench record field",
                "v1 emits the struct copy `_tb.<field> = <rhs>;` and it compiles; assign \
                 the record's fields individually to stay in the TB-IR subset",
            ));
        }
        // Two measured shapes, and they are not the same failure.
        //
        //   * `5 = n` — v1 emits `5 = _tb.n;`. g++: "lvalue required as
        //     left operand of assignment".
        //   * a method PARAMETER shadowing the transactor's `dut` field
        //     (`hookable poke(dut: uint<8>)` with `dut.en = 1` in the
        //     body) — v1 ignores the shadowing entirely and emits
        //     `harc_rt::harc_assign(self.dut->en, 1)`, writing to the
        //     DUT PORT. Built and run: `dut.en=1`, and the parameter
        //     was never touched. The program says `dut` is a `uint<8>`
        //     and v1 pokes hardware.
        //
        // The second is why this is not `Invalid`: v1 runs that program,
        // just not the one that was written. Worst-under-arm, and a
        // silent write to the DUT is the worst thing here.
        self.reject_indexed_component_record_path(target, "a write through")?;
        Err(not_implemented(
            "assignment to a target that is neither a DUT port nor a local",
            "v1 either emits an assignment to a non-place, which does not compile, or — \
             when the target NAME is shadowed by a local or parameter — resolves it to \
             the shadowed DUT handle and writes to the port instead",
            V1Status::SilentlyMisLowers,
        ))
    }

    /// `release dut.<probe>` — disable an active SV procedural force on a
    /// `probe force` signal so the DUT signal returns to its natural
    /// value. Lowers to a `ProbeRelease(PortRef)`; the PortRef must
    /// resolve to a `Force` probe (releasing a read-only probe or an
    /// ordinary port is a hard error). Mirrors v1's `release` lowering
    /// (`<mangled>_en = 0`).
    fn lower_release(&mut self, target: &crate::ast::Expr) -> Result<(), LowerError> {
        let Some(port) = self.as_port_ref(target)? else {
            // MEASURED: v1 refuses this too, with "`release` target must
            // be `dut.<probe_name>`". Pointing at v1 was a dead end, and
            // `release` naming something that is not a DUT probe is a
            // program error under both backends.
            return Err(LowerError::Invalid(
                "`release` applies only to a `probe force` signal on the DUT".to_string(),
            ));
        };
        match port.access {
            crate::ir::PortAccess::Force => {
                self.push(Stmt::ProbeRelease(port));
                Ok(())
            }
            crate::ir::PortAccess::Probe => Err(LowerError::Invalid(format!(
                "`release dut.{}`: read-only probe — only a `probe force` \
                 signal can be released",
                port.port_path.join("."),
            ))),
            crate::ir::PortAccess::Port => Err(LowerError::Invalid(format!(
                "`release dut.{}`: not a probe — `release` applies only to a \
                 `probe force` signal",
                port.port_path.join("."),
            ))),
        }
    }

    /// Recognize a scoreboard queue method access `sb.<queue>.<method>`
    /// (the callee of a call expression). Returns `(sb id, field name,
    /// queue field name, method)` when `sb` is a scoreboard testbench
    /// field and `<queue>` is one of its `queue<T>` fields. Validates
    /// the queue field exists (an unknown field is a hard error, not a
    /// silent `None` fall-through — that would mis-route to v1's
    /// "method call" rejection and lose the precise message).
    pub(crate) fn as_scoreboard_queue_call(
        &self,
        callee: &crate::ast::Expr,
    ) -> Option<(
        crate::ir::ScoreboardId,
        String,
        String,
        String,
        Option<Vec<String>>,
    )> {
        let ExprKind::Field {
            target,
            name: method,
        } = &*callee.kind
        else {
            return None;
        };
        let ExprKind::Field {
            target: sb_t,
            name: queue,
        } = &*target.kind
        else {
            return None;
        };
        let (sb, field, nested) = self.scoreboard_root(sb_t)?;
        Some((sb, field, queue.name.clone(), method.name.clone(), nested))
    }

    /// Recognize a bound-to target-responder queue state-field method
    /// access `<field>.<method>` (a bare field name inside a responder
    /// body). Returns `(field, method)` when `<field>` is a `queue<T>`
    /// state field of the transactor being lowered. A local of the same
    /// name shadows the field (matching the bare-read/-write order), and a
    /// scalar state field is NOT a queue call (returns `None` so the
    /// scalar path handles it).
    pub(crate) fn as_state_queue_call(
        &self,
        callee: &crate::ast::Expr,
    ) -> Option<(String, String)> {
        let ExprKind::Field {
            target,
            name: method,
        } = &*callee.kind
        else {
            return None;
        };
        let ExprKind::Ident(id) = &*target.kind else {
            return None;
        };
        if self.lookup(&id.name).is_some() {
            return None;
        }
        matches!(
            self.target_state_fields.get(&id.name),
            Some(crate::ir::StateFieldKind::Queue { .. })
        )
        .then(|| (id.name.clone(), method.name.clone()))
    }

    /// Recognize a testbench-owned queue call after impl-form desugaring:
    /// `_tb.pending.push(x)` / `.pop()` / `.size()` / `.empty()`.
    pub(crate) fn as_tb_queue_call(&self, callee: &crate::ast::Expr) -> Option<(String, String)> {
        let ExprKind::Field {
            target,
            name: method,
        } = &*callee.kind
        else {
            return None;
        };
        let ExprKind::Field {
            target,
            name: field,
        } = &*target.kind
        else {
            return None;
        };
        let ExprKind::Ident(root) = &*target.kind else {
            return None;
        };
        (self.ctx.tb_field.as_deref() == Some(root.name.as_str())
            && self.ctx.tb_queue_fields.contains_key(&field.name))
        .then(|| (field.name.clone(), method.name.clone()))
    }

    pub(crate) fn tb_queue_elem(&self, field: &str) -> Result<crate::ir::QueueElem, LowerError> {
        self.ctx.tb_queue_fields.get(field).cloned().ok_or_else(|| {
            unsupported(
                &format!("an unknown testbench queue field `{field}`"),
                "declare the queue on the reusable testbench",
            )
        })
    }

    /// Resolve a scoreboard-field access root. Three forms:
    ///   * `sb` (classic) / `_tb.sb` (impl) — a scoreboard TESTBENCH
    ///     field; returns `nested_path = None` (emission uses `_tb.<sb>`).
    ///   * `top.sb` / `_tb.top.sb` — a data-only scoreboard held as an
    ///     ENV sub-component; returns `nested_path = Some(["top","sb"])`
    ///     (emission uses the dotted run-scope path verbatim).
    /// The returned `field` is the resolved access leg name (the last
    /// segment) — used only for the testbench-field-form diagnostics.
    pub(crate) fn scoreboard_root(
        &self,
        e: &crate::ast::Expr,
    ) -> Option<(crate::ir::ScoreboardId, String, Option<Vec<String>>)> {
        // Testbench-field form first (single-segment, possibly `_tb`-
        // prefixed): keeps the established `_tb.<field>` emission.
        match &*e.kind {
            ExprKind::Field { target, name } => {
                if let Some(tb_field) = self.ctx.tb_field.as_deref() {
                    if let ExprKind::Ident(root) = &*target.kind {
                        if root.name == tb_field {
                            if let Some(&sb) = self.ctx.scoreboard_fields.get(&name.name) {
                                return Some((sb, name.name.clone(), None));
                            }
                        }
                    }
                }
            }
            ExprKind::Ident(root) => {
                if let Some(&sb) = self.ctx.scoreboard_fields.get(&root.name) {
                    return Some((sb, root.name.clone(), None));
                }
                // Self-relative form inside a component body: the receiver
                // `sb` (in `sb.writes`) names a `ScoreboardSub` field of
                // `self_component`. The cycle-trigger / on-handler body of an
                // agent-mode transactor pokes its own sub-scoreboard. The
                // nested path is rooted at the synthetic `self` token; the
                // tbir emitter re-roots it at the running instance via
                // `self_subst`. (`scoreboard_root`'s `e` is the receiver,
                // not the full `sb.writes` field expression.)
                if self.lookup(&root.name).is_none() {
                    if let Some(cid) = self.self_component {
                        let comp = &self.ctx.components[cid.index()];
                        if let Some(crate::ir::ComponentFieldKind::ScoreboardSub { scoreboard }) =
                            comp.field(&root.name).map(|f| &f.kind)
                        {
                            return Some((
                                *scoreboard,
                                root.name.clone(),
                                Some(vec!["self".to_string(), root.name.clone()]),
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
        // Env-nested form: `<env-local>.<subs...>.<scoreboard-sub>`. The
        // impl-for desugaring prefixes a testbench-field env with `_tb`
        // (`_tb.top.sb`) — strip it so both bindings resolve through
        // `component_fields` by the bare head name.
        let raw = super::components::dotted_path(e)?;
        let path = self.strip_tb_prefix(&raw).to_vec();
        if path.len() < 2 {
            return None;
        }
        let &head_cid = self.ctx.component_fields.get(&path[0])?;
        let (sid, nested) = self.resolve_scoreboard_sub(head_cid, &path[1..], path[0].clone())?;
        Some((sid, path.last().unwrap().clone(), Some(nested)))
    }

    /// Walk a sub-component path from an env head down to a data-only
    /// `ScoreboardSub` leaf. `segs` is the path after the head local;
    /// `acc` accumulates the full dotted access path (starting with the
    /// head local name). Returns the leaf scoreboard id + the full path.
    fn resolve_scoreboard_sub(
        &self,
        head: crate::ir::ComponentId,
        segs: &[String],
        acc_head: String,
    ) -> Option<(crate::ir::ScoreboardId, Vec<String>)> {
        let mut cid = head;
        let mut acc = vec![acc_head];
        for (i, seg) in segs.iter().enumerate() {
            let comp = &self.ctx.components[cid.index()];
            match comp.field(seg).map(|f| &f.kind) {
                Some(crate::ir::ComponentFieldKind::Sub { component, .. }) => {
                    cid = *component;
                    acc.push(seg.clone());
                }
                Some(crate::ir::ComponentFieldKind::ScoreboardSub { scoreboard }) => {
                    // A scoreboard sub must be the terminal segment.
                    if i != segs.len() - 1 {
                        return None;
                    }
                    acc.push(seg.clone());
                    return Some((*scoreboard, acc));
                }
                _ => return None,
            }
        }
        None
    }

    /// Recognize `sb.<queue>.pop()` as a value expression and validate
    /// it: the field must be a declared `queue<T>` of the scoreboard.
    /// Returns `(sb id, field name, queue field name)`.
    fn as_scoreboard_pop(
        &self,
        e: &crate::ast::Expr,
    ) -> Result<Option<(crate::ir::ScoreboardId, String, String, Option<Vec<String>>)>, LowerError>
    {
        let ExprKind::Call { callee, args } = &*e.kind else {
            return Ok(None);
        };
        let Some((sb, field, queue, method, nested)) = self.as_scoreboard_queue_call(callee) else {
            return Ok(None);
        };
        if method != "pop" {
            // size()/empty() are value-producing reads, not pop — those
            // lower as `Expr::ScoreboardQuery` in expression position.
            // Anything else on a queue is unsupported here.
            return Ok(None);
        }
        if !args.is_empty() {
            return Err(LowerError::Invalid(format!(
                "scoreboard `{field}.{queue}.pop()` takes no arguments"
            )));
        }
        // Validate the queue field exists and is a queue.
        self.scoreboard_queue_field(sb, &queue)?;
        Ok(Some((sb, field, queue, nested)))
    }

    /// Validate that `<field>` is a declared scalar field of scoreboard
    /// `sb`; returns the field name (clone) on success.
    pub(crate) fn scoreboard_scalar_field(
        &self,
        sb: crate::ir::ScoreboardId,
        field: &str,
    ) -> Result<String, LowerError> {
        let schema = &self.ctx.scoreboards[sb.index()];
        match schema.field(field) {
            Some(f) => match &f.kind {
                crate::ir::ScoreboardFieldKind::Scalar { .. } => Ok(field.to_string()),
                crate::ir::ScoreboardFieldKind::Queue { .. } => Err(LowerError::Invalid(format!(
                    "scoreboard `{}` field `{field}` is a queue, not a scalar — assign via \
                     `push`/`pop`",
                    schema.name
                ))),
            },
            None => Err(LowerError::Invalid(format!(
                "scoreboard `{}` has no field `{field}`",
                schema.name
            ))),
        }
    }

    /// The `QueueElem` of scoreboard `sb`'s queue field `<field>`, so a
    /// record-element `pop()` can type its destination local as the record
    /// (mirrors the composite-component path's `component_queue_elem`).
    /// Errors identically to `scoreboard_queue_field` for a non-queue /
    /// missing field.
    pub(crate) fn scoreboard_queue_elem(
        &self,
        sb: crate::ir::ScoreboardId,
        field: &str,
    ) -> Result<crate::ir::QueueElem, LowerError> {
        let schema = &self.ctx.scoreboards[sb.index()];
        match schema.field(field) {
            Some(f) => match &f.kind {
                crate::ir::ScoreboardFieldKind::Queue { elem } => Ok(*elem),
                crate::ir::ScoreboardFieldKind::Scalar { .. } => Err(LowerError::Invalid(format!(
                    "scoreboard `{}` field `{field}` is a scalar, not a queue",
                    schema.name
                ))),
            },
            None => Err(LowerError::Invalid(format!(
                "scoreboard `{}` has no field `{field}`",
                schema.name
            ))),
        }
    }

    /// Validate that `<field>` is a declared queue field of scoreboard
    /// `sb`.
    pub(crate) fn scoreboard_queue_field(
        &self,
        sb: crate::ir::ScoreboardId,
        field: &str,
    ) -> Result<(), LowerError> {
        let schema = &self.ctx.scoreboards[sb.index()];
        match schema.field(field) {
            Some(f) => match &f.kind {
                crate::ir::ScoreboardFieldKind::Queue { .. } => Ok(()),
                crate::ir::ScoreboardFieldKind::Scalar { .. } => Err(LowerError::Invalid(format!(
                    "scoreboard `{}` field `{field}` is a scalar, not a queue",
                    schema.name
                ))),
            },
            None => Err(LowerError::Invalid(format!(
                "scoreboard `{}` has no field `{field}`",
                schema.name
            ))),
        }
    }

    /// Lower `callee(args)` into the `Expr::Call(TransactorMethod ..)`
    /// payload of a `Stmt::TransactorCall`, or `None` when `callee` is
    /// not a transactor-method access. `need_ret` enforces that a
    /// result-binding site calls a `-> T` method (v1 surfaces that as
    /// a C++ compile error; the IR rejects at lowering).
    pub(crate) fn lower_transactor_call(
        &mut self,
        callee: &crate::ast::Expr,
        args: &[CallArg],
        need_ret: bool,
    ) -> Result<Option<Expr>, LowerError> {
        let Some((tb_field, xid, method)) = self.as_transactor_call(callee)? else {
            return Ok(None);
        };
        if self.in_fmt_args {
            return Err(unsupported(
                &format!("transactor method call `{tb_field}.{method}(...)` inside a message"),
                "log/fail messages evaluate lazily; hoist the call into a `let` first",
            ));
        }
        // Detach the schema borrow from `self` (the arg loop below
        // lowers through `&mut self`).
        let ctx: &crate::ir::lower::LowerCtx = self.ctx;
        let schema = &ctx.transactors[xid.index()];
        let m = schema
            .method(&method)
            .expect("as_transactor_call validated the method");
        // A `when active` method is not callable on a `passive` instance —
        // the method structurally does not exist there. Reject the call at
        // lowering (mirroring v1's "`<m>` is declared inside `when active`"
        // diagnostic) rather than emitting an unresolved method reference.
        if m.active_only && ctx.passive_transactor_fields.contains(&tb_field) {
            return Err(LowerError::Invalid(format!(
                "method `{tb_field}.{method}(...)` is declared inside `when active` and is not \
                 callable on the passive instance `{tb_field}`; bind the instance `active` to \
                 call its `when active` methods, or move the method out of `when active` if it \
                 should exist on passive instances"
            )));
        }
        if args.len() != m.param_names.len() {
            return Err(LowerError::Invalid(format!(
                "transactor method `{}.{method}` takes {} argument(s), call passes {}",
                schema.name,
                m.param_names.len(),
                args.len()
            )));
        }
        if need_ret && !m.has_ret {
            return Err(LowerError::Invalid(format!(
                "transactor method `{}.{method}` returns no value",
                schema.name
            )));
        }
        // v1 drops argument names and binds by position, so a name in
        // its own position is inert and only a reordered one swaps the
        // values (measured: `axil_write(data = t.value, addr = t.addr)`
        // emits `AxilXactor_axil_write(_tb.env.drv, t.value, t.addr)`).
        // The names come from `TransactorMethodSchema::param_names`,
        // which used to be a bare `n_params` count — the seam had
        // nothing to check against, which is why this arm refused every
        // named argument including the working form.
        super::reject_misplaced_named_args(
            args,
            &m.param_names,
            &format!("transactor method call `{tb_field}.{method}(...)`"),
        )?;
        let mut lowered = Vec::with_capacity(args.len());
        for a in args {
            let (CallArg::Expr(e) | CallArg::Named { value: e, .. }) = a;
            lowered.push(self.lower_expr_no_ports(e)?);
        }
        // Same rule as the component-method call one screen up, at the
        // transactor spelling of it. The schema had to learn
        // `param_tys` for this — it carried only names, so there was
        // nothing here to type-check against.
        for (i, (a, ty)) in lowered.iter().zip(m.param_tys.iter()).enumerate() {
            let pname = m
                .param_names
                .get(i)
                .cloned()
                .unwrap_or_else(|| format!("#{}", i + 1));
            self.check_slot_ir(
                a,
                ty,
                &format!("parameter `{pname}` of `{}.{method}`", schema.name),
            )?;
        }
        Ok(Some(Expr::Call(
            crate::ir::CallTarget::TransactorMethod {
                bus_field: tb_field,
                method,
            },
            lowered,
        )))
    }

    /// Lower a bare sibling method call inside a DUT-poking transactor
    /// method body: `idle()` / `readv()`. This is distinct from
    /// `xact.idle()` at testbench scope: no testbench field is involved,
    /// and emission calls the sibling method lambda directly.
    pub(crate) fn lower_transactor_self_call(
        &mut self,
        name: &str,
        args: &[CallArg],
        need_ret: bool,
    ) -> Result<Option<Expr>, LowerError> {
        if !self.in_transactor_method {
            return Ok(None);
        }
        let Some(transactor) = self.self_transactor.clone() else {
            return Ok(None);
        };
        let Some((param_names, param_tys, has_ret, callee_active_only)) =
            self.self_transactor_methods.get(name).cloned()
        else {
            return Ok(None);
        };
        let n_params = param_names.len();
        if self.in_fmt_args {
            return Err(unsupported(
                &format!("transactor sibling method call `{name}(...)` inside a message"),
                "log/fail messages evaluate lazily; hoist the call into a `let` first",
            ));
        }
        if args.len() != n_params {
            return Err(LowerError::Invalid(format!(
                "transactor method `{transactor}.{name}` takes {n_params} argument(s), call passes {}",
                args.len()
            )));
        }
        if need_ret && !has_ret {
            return Err(LowerError::Invalid(format!(
                "transactor method `{transactor}.{name}` returns no value"
            )));
        }
        if callee_active_only && !self.self_transactor_method_active_only {
            let caller = self
                .current_body_name
                .as_deref()
                .unwrap_or("<current method>");
            return Err(unsupported(
                &format!(
                    "transactor sibling method call `{name}(...)` from always-on method \
                     `{caller}`",
                ),
                &format!(
                    "`{name}` is declared inside `when active`; move `{caller}` into `when active`, \
                     or call `{name}` only from active-only code",
                ),
            ));
        }
        // Same as the bound-instance arm above.
        super::reject_misplaced_named_args(
            args,
            &param_names,
            &format!("transactor sibling method call `{transactor}.{name}(...)`"),
        )?;
        let mut lowered = Vec::with_capacity(args.len());
        for a in args {
            let (CallArg::Expr(e) | CallArg::Named { value: e, .. }) = a;
            lowered.push(self.lower_expr_no_ports(e)?);
        }
        // The SIBLING spelling of the parameter rule — `inner(1)` from
        // another method of the same transactor, where the bound-
        // instance spelling is `drv.inner(1)`. Arity is checked above,
        // so the zip is total.
        for (i, (a, ty)) in lowered.iter().zip(param_tys.iter()).enumerate() {
            let pname = param_names
                .get(i)
                .cloned()
                .unwrap_or_else(|| format!("#{}", i + 1));
            self.check_slot_ir(
                a,
                ty,
                &format!("parameter `{pname}` of `{transactor}.{name}`"),
            )?;
        }
        Ok(Some(Expr::Call(
            crate::ir::CallTarget::TransactorSelfMethod {
                transactor,
                method: name.to_string(),
            },
            lowered,
        )))
    }

    /// `_tb.<xfield>.<dut_field> = dut` — the instance's DUT bind.
    /// Validated, then erased: the IR's single-DUT model makes the
    /// bind static (the method bodies' `PortRef`s already resolve to
    /// the test's DUT). v1 emits a pointer copy here; the only
    /// observable difference is on broken programs that CALL a method
    /// without ever binding — v1 dereferences null, the IR backend
    /// drives the DUT anyway. Returns `true` when the statement was
    /// consumed.
    fn lower_transactor_dut_bind(
        &mut self,
        target: &crate::ast::Expr,
        value: &crate::ast::Expr,
    ) -> Result<bool, LowerError> {
        // target = `_tb.<xfield>.<sub>` (testbench-field instance) or
        // `<xfield>.<sub>` (test-scope-let instance, bare name).
        let ExprKind::Field {
            target: mid,
            name: sub,
        } = &*target.kind
        else {
            return Ok(false);
        };
        let xfield_name = match &*mid.kind {
            ExprKind::Field {
                target: root_expr,
                name: xfield,
            } => {
                let ExprKind::Ident(root) = &*root_expr.kind else {
                    return Ok(false);
                };
                if Some(root.name.as_str()) != self.ctx.tb_field.as_deref() {
                    return Ok(false);
                }
                xfield.name.clone()
            }
            ExprKind::Ident(id)
                if self.lookup(&id.name).is_none()
                    && (self.ctx.bare_transactor_fields.contains(&id.name)
                        || self.ctx.transactor_fields.contains_key(&id.name)) =>
            {
                id.name.clone()
            }
            _ => return Ok(false),
        };
        let xfield = &xfield_name;
        let Some(&xid) = self.ctx.transactor_fields.get(xfield) else {
            return Ok(false);
        };
        let schema = &self.ctx.transactors[xid.index()];
        if sub.name != schema.dut_field {
            return Err(unsupported(
                &format!("assignment to transactor field `{}.{}`", xfield, sub.name),
                "only the module-typed DUT bind is lowered",
            ));
        }
        // RHS must be the test's DUT.
        let is_dut = match &*value.kind {
            ExprKind::Ident(id) => self.is_dut_name(&id.name),
            _ => false,
        };
        if !is_dut {
            return Err(unsupported(
                &format!(
                    "binding `{}.{}` to something other than the test DUT",
                    xfield, sub.name
                ),
                "",
            ));
        }
        Ok(true)
    }

    /// `randomize(t)` / `randomize(t) with {...}` → `Terminator::Randomize`.
    ///
    /// Resolves the target record local, builds the constraint-IR site
    /// (transaction `keep`s merged ahead of the `with {...}` body — v1's
    /// spec-§4 merge order — plus the typed-problem-id handle), records
    /// it in the program-wide constraint table, and terminates the
    /// current block with `Randomize { target, constraints, succ }`.
    fn lower_randomize(
        &mut self,
        blocking: bool,
        target: &crate::ast::Expr,
        with_body: &[crate::ast::Expr],
    ) -> Result<(), LowerError> {
        // The target must be a bare record-typed local (`let t : Txn`).
        // v1 rejects non-ident / non-record targets with the same intent;
        // mirror that as a precise lowering error.
        let ExprKind::Ident(id) = &*target.kind else {
            return Err(not_implemented(
                "`randomize` of a non-identifier target",
                "randomize a record-typed local declared with `let t : <Transaction>`",
                V1Status::Rejects,
            ));
        };
        let Some(local) = self.lookup(&id.name) else {
            return Err(LowerError::Invalid(format!(
                "randomize({}): `{}` is not in scope",
                id.name, id.name
            )));
        };
        let Some(record_id) = self.record_of_local(local) else {
            return Err(LowerError::Invalid(format!(
                "randomize({0}): `{0}` is not a transaction/struct local — declare it with \
                 `let {0} : <Transaction>`",
                id.name
            )));
        };
        let record = self.ctx.records[record_id.index()].name.clone();

        // Spec §4: transaction-level `keep`s are part of every
        // `randomize(t)` of that type. Merge them ahead of the call-site
        // `with {...}` body, exactly as v1's `StmtKind::Randomize` arm.
        let mut constraints: Vec<crate::ast::Expr> = Vec::new();
        if let Some(keeps) = self.ctx.txn_keeps.get(&record) {
            constraints.extend(keeps.iter().cloned());
        }
        constraints.extend(with_body.iter().cloned());

        // Problem-id handle (constraint-IR layer), keyed by target span
        // exactly like v1's `runtime_randomize_problem_id`.
        let problem_id = self
            .ctx
            .randomize_problem_ids
            .get(&(target.span.start, target.span.end))
            .copied();

        let constraints_ref = self.push_constraint_site(crate::ir::ConstraintSite {
            record,
            target: target.clone(),
            constraints,
            blocking,
            problem_id,
        });

        let succ = self.new_block();
        self.terminate(Terminator::Randomize {
            target: local,
            constraints: constraints_ref,
            succ,
        });
        self.start_block(succ);
        Ok(())
    }

    /// Lower a tseq generator call (`RandomTxns(5)`) into a
    /// `CallTarget::Tseq` edge. Args are scalar expressions, lowered (and
    /// port-hoisted) like any call argument; named args are rejected
    /// (tseq params are positional, matching v1).
    pub(crate) fn lower_tseq_call(
        &mut self,
        name: &str,
        args: &[crate::ast::CallArg],
    ) -> Result<Expr, LowerError> {
        // v1 drops argument names and binds by position here too:
        // measured, `RandomTxns(n = 5)` emits `RandomTxns(5)` against
        // `auto RandomTxns = [&](uint64_t n)` — byte-identical to the
        // positional call. The names ride in `ctx.tseqs` for this.
        let (param_names, param_tys) = match self.ctx.tseqs.get(name) {
            Some((_, declared, tys)) => {
                let (declared, tys) = (declared.clone(), tys.clone());
                super::reject_misplaced_named_args(
                    args,
                    &declared,
                    &format!("tseq call `{name}(...)`"),
                )?;
                (declared, tys)
            }
            None => (Vec::new(), Vec::new()),
        };
        // A RECORD-typed tseq parameter is not a slot any argument can
        // enter, so the verdict belongs to the parameter rather than to
        // what is passed. Neither emitter honours the declared type:
        // v1 renders it as a Verilated module handle
        // (`[&](VBeat* seed)`) and tbir as a scalar
        // (`[&](uint64_t seed)`). v1's spelling does not compile under
        // ANY call — `'VBeat' has not been declared` — so every call to
        // such a tseq is uncompilable there.
        //
        // Naming it here rather than at the argument is what the first
        // version of this got wrong: routing the slot through
        // `slot_ir_type` resolved the parameter to `Record(Beat)`, so a
        // `Beat` argument MATCHED and the call lowered, while a scalar
        // one was rejected for being the wrong shape. Both spellings are
        // equally unbuildable, and the comment left behind still claimed
        // the record one was refused.
        //
        // Not `Invalid`: the DECLARATION alone is fine under tbir — an
        // uncalled `tseq Wrap(seed: Beat)` compiles there (`uint64_t
        // seed` is a valid lambda parameter, it is only wrong) — so the
        // program is out of subset rather than meaningless, and the
        // verdict is what v1 does with it.
        if let Some((i, _)) = param_tys
            .iter()
            .enumerate()
            .find(|(_, t)| matches!(t, IrType::Record(_)))
        {
            let pname = param_names
                .get(i)
                .cloned()
                .unwrap_or_else(|| format!("#{}", i + 1));
            return Err(not_implemented(
                &format!("a record-typed parameter `{pname}` on `tseq {name}`"),
                format!(
                    "v1 emits `{pname}` as a Verilated module handle (`V<Record>*`), which \
                     does not compile; pass the record's fields as scalars instead"
                ),
                V1Status::EmitsUncompilable,
            ));
        }
        let mut lowered = Vec::with_capacity(args.len());
        for (i, a) in args.iter().enumerate() {
            let (crate::ast::CallArg::Expr(e) | crate::ast::CallArg::Named { value: e, .. }) = a;
            let v = self.lower_expr_no_ports(e)?;
            // A note here once claimed every REACHABLE tseq parameter is
            // a scalar, so the slot needed no type table and could be
            // hard-coded to "not a record". `TSeq<T>` disproves it:
            // `tseq Wrap(xs: TSeq<Beat>)` is compiled by v1 as
            // `[&](const std::vector<Beat>& xs) -> std::vector<Beat>`,
            // and the hard-coded slot rejected `Wrap(xs)` — which works
            // — while passing `Wrap(7)`, which v1 refuses with "no known
            // conversion from 'int' to 'const std::vector<Beat>&'".
            //
            // A record-typed parameter never reaches this loop — it is
            // refused above, on the parameter rather than the argument.
            if let Some(ty) = param_tys.get(i) {
                self.check_slot_ir(&v, ty, &format!("parameter of tseq `{name}`"))?;
            }
            lowered.push(v);
        }
        Ok(Expr::Call(
            crate::ir::CallTarget::Tseq(name.to_string()),
            lowered,
        ))
    }

    /// `yield e` inside a `tseq` body — append a value onto the sequence
    /// accumulator. The accumulator is the tseq function's `ret` local (set
    /// by `tseqs::lower_tseq` via `set_tseq_result`); v1 emits
    /// `_result.push_back(<e>)`.
    ///
    /// For a `RecordSeq` accumulator the yielded value must be a record
    /// local whose type matches the accumulator's element record (a
    /// type mismatch would fail to compile in v1's `push_back`). For a
    /// scalar `Seq` accumulator any scalar expression is yielded (lowered
    /// like a `let`-RHS scalar; DUT reads hoisted into the current block).
    fn lower_yield(&mut self, value: &crate::ast::Expr) -> Result<(), LowerError> {
        let Some(seq) = self.tseq_result else {
            // v1 raises its own "`yield` outside a `tseq` body is not
            // supported in v0 cpp_tb" here, so it is not an escape hatch.
            return Err(not_implemented(
                "`yield` outside a `tseq` body",
                "yield is only valid inside a `tseq ... end tseq` body",
                V1Status::Rejects,
            ));
        };
        let elem = self
            .seq_of_local(seq)
            .expect("tseq accumulator is always RecordSeq/Seq-typed");
        match elem {
            // Scalar-element sequence: yield any scalar expression.
            IrType::UInt(_) | IrType::SInt(_) | IrType::Bool | IrType::Unknown => {
                let v = self.lower_expr_no_ports(value)?;
                // A scalar-element sequence must not accept a RECORD.
                // Without this the record local is pushed verbatim and
                // the emitter writes `std::vector<int64_t>::push_back(t)`
                // with `t` a struct — uncompilable C++, no diagnostic.
                //
                // Previously unreachable: every scalar-element seq came
                // from an explicit `TSeq<uint<N>>`, and a body yielding a
                // record against that annotation was caught by the type
                // in the annotation. Defaulting an UNANNOTATED tseq to a
                // scalar element made it reachable — the check the
                // narrower gate had been providing for free.
                if let Expr::Local(id) = &v {
                    // Any NON-SCALAR local, not just a record: a
                    // sequence-typed one (`let xs = Inner(2); yield xs`)
                    // slips through a record-only check and pushes a
                    // `std::vector` into a `std::vector<int64_t>` just
                    // as silently.
                    let want = match self.local_type(*id) {
                        IrType::UInt(_) | IrType::SInt(_) | IrType::Bool | IrType::Unknown => None,
                        IrType::Record(rid) => Some(self.ctx.records[rid.index()].name.clone()),
                        other => Some(format!("{other:?}")),
                    };
                    if let Some(what) = want {
                        // v1 accepts this and emits the mismatched
                        // `push_back` — same behaviour, and so the same
                        // classification, as the sibling bad-element-type
                        // arm in `tseqs.rs`.
                        return Err(not_implemented(
                            "`yield` of a non-scalar value in a tseq whose element type is a \
                             scalar",
                            format!(
                                "annotate the tseq with `-> TSeq<{what}>`; v1 emits the \
                                 mismatched `push_back`, which does not compile"
                            ),
                            V1Status::EmitsUncompilable,
                        ));
                    }
                }
                self.push(Stmt::SeqPush { seq, value: v });
                Ok(())
            }
            // Record-element sequence: yield a bare same-typed record local
            // — the only thing `push_back` accepts.
            IrType::Record(elem_rid) => {
                let ExprKind::Ident(id) = &*value.kind else {
                    return Err(unsupported(
                        "`yield` of a non-identifier value",
                        "yield a record-typed local declared with `let t : <Transaction>`",
                    ));
                };
                let Some(local) = self.lookup(&id.name) else {
                    return Err(LowerError::Invalid(format!(
                        "yield {0}: `{0}` is not in scope",
                        id.name
                    )));
                };
                match self.record_of_local(local) {
                    Some(rid) if rid == elem_rid => {}
                    _ => {
                        return Err(unsupported(
                            &format!(
                                "`yield {}` whose value is not a `{}` record local",
                                id.name,
                                self.ctx.records[elem_rid.index()].name
                            ),
                            "yield a same-typed transaction local",
                        ));
                    }
                }
                self.push(Stmt::SeqPush {
                    seq,
                    value: Expr::Local(local),
                });
                Ok(())
            }
            other => Err(LowerError::Invalid(format!(
                "tseq accumulator has unexpected element type {other:?} (lowering bug)"
            ))),
        }
    }

    /// A registration statement installs something that outlives the
    /// statement — a `_checkers` closure, an event subscriber. That is
    /// only sound in a test's own `run`/`check` body, where the statement
    /// runs exactly once. Anywhere else (a transactor method, a helper,
    /// another handler's body) it would re-register on every call, and
    /// its `[&]` capture would outlive the parameters it captured.
    fn require_test_body(&self, what: &str) -> Result<(), LowerError> {
        if self.in_test_body {
            return Ok(());
        }
        // v1 is no escape hatch here: it emits the registration inline in
        // the method/helper lambda, so the program builds and then
        // re-registers on every call while its `[&]` capture reads
        // parameters that died with the call.
        Err(not_implemented(
            &format!("{what} outside a test's `run`/`check` body"),
            "a registration installs a closure that outlives the statement, so it must run \
             exactly once — move it into the test's `run` (or `check`) body",
            V1Status::SilentlyMisLowers,
        ))
    }

    /// Reserve a slot in the out-of-line function table BEFORE lowering
    /// the body, so a nested registration inside that body claims the
    /// NEXT slot instead of colliding on the same index. The placeholder
    /// is overwritten by `commit_pending_function`.
    fn reserve_pending_function(&mut self) -> crate::ir::FunctionId {
        let mut tables = self.side_tables.borrow_mut();
        let id = crate::ir::FunctionId(tables.pending_functions.len() as u32);
        tables
            .pending_functions
            .push(super::placeholder_function(id));
        id
    }

    fn commit_pending_function(&mut self, id: crate::ir::FunctionId, f: TbFunction) {
        self.side_tables.borrow_mut().pending_functions[id.index()] = f;
    }

    /// `on <channel>(<param>) ... end on` — subscribe to a test-scope
    /// event channel or a component event field. The body becomes a
    /// ONE-parameter `FunctionKind::TestHook` function whose parameter is
    /// the payload;
    /// the registration statement pushes a closure calling it onto the
    /// channel vector, exactly as v1 pushes its inline closure.
    fn lower_event_subscription(
        &mut self,
        h: &crate::ast::OnHandler,
        callee: &AstExpr,
        args: &[CallArg],
    ) -> Result<(), LowerError> {
        self.require_test_body("an `on <event>(...)` subscription")?;
        let (event_ref, payload, display_name) = if let ExprKind::Ident(id) = &*callee.kind {
            let Some(channel) = self.lookup(&id.name) else {
                // `lookup` failing means the name is not a LOCAL here. It
                // does NOT mean the name is undefined — an earlier version
                // of this comment said so and was wrong. Measured, all of
                // these land here: a testbench component field (`s`), a
                // testbench scalar field (`seen`), the clock (`clk`), the
                // DUT binding (`dut`), an agent TYPE name (`Src`), and a
                // component METHOD name (`fire`). Every one is declared
                // somewhere in the program.
                //
                // The verdict survives anyway, and that was measured too:
                // v1 emits `<name>.push_back(...)` for each and g++ refuses
                // all six ("'s' was not declared in this scope", "request
                // for member 'push_back' in 'dut', which is of pointer type
                // 'VTop*'", and so on). The message says "names no event
                // channel in scope", which is true of all of them; it is
                // the reasoning that was over-stated, not the wording.
                //
                // MEASURED: v1 emits `nosuch.push_back([&](int64_t v) {...})`
                // — g++: "'nosuch' was not declared in this scope". A
                // program error under both backends.
                //
                // A component event path is handled by the branch below;
                // this diagnostic is only for a bare name with no local
                // event-channel binding.
                return Err(LowerError::Invalid(format!(
                    "`on {}(...)`: `{}` names no event channel in scope",
                    id.name, id.name
                )));
            };
            let IrType::Event(payload) = *self.local_type(channel) else {
                return Err(LowerError::Invalid(format!(
                    "`on {}(...)`: `{}` is not an event channel",
                    id.name, id.name
                )));
            };
            (EventChannelRef::Local(channel), payload, id.name.clone())
        } else {
            let Some(raw) = super::components::dotted_path(callee) else {
                return Err(LowerError::Invalid(
                    "an event subscription target must be an event-channel name or component \
                     event path"
                        .to_string(),
                ));
            };
            let path = self.strip_tb_prefix(&raw).to_vec();
            if path.len() < 2 {
                return Err(LowerError::Invalid(format!(
                    "`on {}(...)` does not name a component event field",
                    path.join(".")
                )));
            }
            let head = path[0].clone();
            let Some(&head_cid) = self.ctx.component_fields.get(&head) else {
                return Err(LowerError::Invalid(format!(
                    "`on {}(...)`: `{head}` is not a component binding",
                    path.join(".")
                )));
            };
            let recv = path[..path.len() - 1].to_vec();
            let event = path.last().expect("path has event segment").clone();
            let cid = self.resolve_component_recv(head_cid, &recv[1..])?;
            let comp = &self.ctx.components[cid.index()];
            let Some(field) = comp.field(&event) else {
                return Err(LowerError::Invalid(format!(
                    "component `{}` has no event field `{event}`",
                    comp.name
                )));
            };
            let ComponentFieldKind::Event { payload } = field.kind else {
                return Err(LowerError::Invalid(format!(
                    "component `{}.{event}` is not an event field",
                    comp.name
                )));
            };
            let activation = field.activation;
            self.require_component_activation(
                &head,
                head_cid,
                self.binding_mode(&head),
                &recv[1..],
                activation,
                "event",
                &event,
            )?;
            let display = path.join(".");
            (
                EventChannelRef::Component {
                    base: ComponentBase::Path(recv),
                    component: cid,
                    event,
                    payload,
                },
                payload,
                display,
            )
        };
        // Exactly one parameter, and it must be a plain binding name —
        // the payload the emitter passes in.
        let param = match args {
            [CallArg::Expr(e)] => match &*e.kind {
                ExprKind::Ident(p) => p.name.clone(),
                _ => {
                    return Err(LowerError::Invalid(format!(
                        "`on {display_name}(...)`: the payload binding must be a name"
                    )))
                }
            },
            _ => {
                return Err(LowerError::Invalid(format!(
                    "`on {display_name}(...)` takes exactly one payload binding, got {}",
                    args.len()
                )))
            }
        };

        let pending_id = self.reserve_pending_function();
        let param_ty = match payload {
            crate::ir::EventPayload::Scalar { signed: true } => IrType::SInt(None),
            crate::ir::EventPayload::Scalar { signed: false } => IrType::UInt(None),
            crate::ir::EventPayload::Record(r) => IrType::Record(r),
        };
        let mut b = FuncBuilder::new(self.ctx, self.helpers, self.side_tables);
        super::reserve_tb_record_names(&mut b, self.ctx);
        // The payload binding is local 0, which is the verifier's
        // parameter convention (`locals[..params.len()]` mirror the
        // params one-to-one and are defined at entry).
        let p = b.declare(&param);
        b.set_local_type(p, param_ty.clone());
        b.lower_block_stmts(&h.body)?;
        if !b.is_terminated() {
            b.terminate(Terminator::Return);
        }
        let mut f = b.finish(
            pending_id,
            format!("_on_event_{}", pending_id.0),
            crate::ir::FunctionKind::TestHook,
            self.ctx.owner,
        )?;
        f.params = vec![crate::ir::TypedParam {
            name: param,
            ty: param_ty,
        }];

        self.commit_pending_function(pending_id, f);
        self.push(Stmt::EventSubscribe {
            event: event_ref,
            handler: pending_id,
        });
        Ok(())
    }

    /// A statement-position `on ... end on` handler inside a run or check
    /// body (spec §7.10 and the cycle-trigger form).
    ///
    /// Two shapes lower: the cycle trigger (`on <bool-expr>`, gated on the
    /// declared edge mode) and the periodic form (`on <N> cycles`). Both
    /// become a `_checkers` / `_post_eval_services` closure installed at
    /// this statement's position, which is where v1's `emit_cycle_trigger`
    /// pushes them — a handler written after a `wait` never observes the
    /// earlier cycles under either backend.
    ///
    /// The remaining shapes stay out of subset with precise messages:
    /// method hooks (`on <obj>.<m> pre/post`) need the reference-capturing
    /// hook vectors the component path owns, and event subscriptions
    /// (`on <ev>(arg)`) need a subscriber list on a component field.
    fn lower_on_handler(&mut self, h: &crate::ast::OnHandler) -> Result<(), LowerError> {
        use crate::ir::{CycleHandlerKind, CycleHandlerSchema};
        self.require_test_body("an `on ... end on` handler")?;
        if h.hook.is_some() {
            // A hook side in statement position splits on the TRIGGER,
            // because v1 does. v1 routes any hooked `on` here through
            // its method-hook resolver, which wants an `<obj>.<method>`
            // path and refuses anything else outright.
            //
            //   * `on s.send pre` — v1 emits the same
            //     `<Type>_<method>_pre.push_back` registration a
            //     test-scope hook gets, so `--codegen v1` is a real
            //     escape hatch and this stays `Unsupported`.
            //   * `on <bool-expr> pre`, `on <N> cycles pre`,
            //     `on ev(x) pre` — v1 REFUSES ("obj.method must resolve
            //     to a `hookable` on a known component type"). Pointing
            //     at v1 there sends the user to a second error.
            //
            // The destination named below used to be "the component or
            // testbench", and BOTH of those fail — a hook in a component
            // body hits `components::validate_cycle_handler` and one in
            // a `testbench` declaration hits the testbench-scope arm in
            // `mod.rs`. The destination that works is the test / `impl
            // ... for` body, against a DIRECT transactor field, which is
            // what `axilite_hooks_test` exercises in the equivalence
            // registry. (A path that is well-formed but does not resolve
            // to a `hookable` — `w.plain`, `nosuch.send` — is refused by
            // v1 too; those keep the suggestion, and the user lands on
            // v1's own precise message rather than on silence.)
            // The phase is called out separately from the trigger shape:
            // both are `Rejects`, but "not a method path" would be a
            // false explanation for `on s.send pre phase post_eval`,
            // whose path is fine and whose phase is not.
            if h.phase == crate::ast::OnPhase::PostEval {
                return Err(not_implemented(
                    "a `phase post_eval` modifier on a `pre`/`post` hook in statement position",
                    "v1 refuses a phase modifier on a method hook and suggests a \
                     cycle-trigger `on <expr> phase post_eval` instead",
                    V1Status::Rejects,
                ));
            }
            if !super::is_v1_method_hook_shape(h) {
                return Err(not_implemented(
                    "a `pre`/`post` hook on a non-method-path `on` handler in statement position",
                    "a hook side names a method to wrap; v1 routes every hooked `on` through \
                     its method-hook resolver and refuses a trigger that is not an \
                     `<obj>.<method>` path",
                    V1Status::Rejects,
                ));
            }
            // Shape is not resolution, and this arm used to stop at
            // shape. `is_v1_method_hook_shape` accepts any dotted path
            // of the right length, so `drv.send.x`, `nosuch.send`,
            // `drv.plain` and `dut.rst.x` all reached the suggestion
            // below — and MEASURED, v1 refuses every one of them with
            // "obj.method must resolve to a `hookable` on a known
            // component type", while the resolving `drv.send` emits.
            // So the suggestion was honest for exactly one of the five
            // and misdirection for the rest.
            //
            // This is v1's own condition, and it is the same check the
            // test-scope arm in `mod.rs` makes: does `<obj>.<method>`
            // name a `hookable` on a transactor or component testbench
            // field? Checked in the recoverable direction — a miss
            // yields the honest `Rejects`, and a hit only ever upgrades
            // to the suggestion.
            let Some((target, params)) = resolve_statement_method_hook_target(self, h)? else {
                return Err(not_implemented(
                    "a `pre`/`post` hook whose path names no `hookable` in statement position",
                    "v1 routes every hooked `on` through its method-hook resolver and \
                     refuses a path that does not resolve to a `hookable` on a known \
                     component type, so it is not a way to run this program",
                    V1Status::Rejects,
                ));
            };
            let captures = self.method_hook_captures(&params, &h.body);
            let capture_params: Vec<TypedParam> = captures
                .iter()
                .map(|(name, local)| TypedParam {
                    name: name.clone(),
                    ty: self.local_type(*local).clone(),
                })
                .collect();
            let pending_id = self.reserve_pending_function();
            let hook_fn = super::lower_method_hook_body(
                pending_id,
                format!("_on_method_hook_{}", pending_id.0),
                self.ctx.owner,
                &params,
                &capture_params,
                &h.body,
                self.ctx,
                self.helpers,
                self.side_tables,
            )?;
            self.commit_pending_function(pending_id, hook_fn);
            self.push(Stmt::MethodHookSubscribe {
                target,
                side: h.hook.expect("hook branch has a side"),
                handler: pending_id,
                captures: captures.into_iter().map(|(_, local)| local).collect(),
            });
            return Ok(());
        }
        // `on e(v) ... end on` — an event subscription. A test-scope
        // channel (`let e : event<T>`) subscribes here; a component's
        // `event` FIELD is reached through the component path instead,
        // which a statement position cannot name.
        if !h.periodic {
            if let ExprKind::Call { callee, args } = &*h.event.kind {
                return self.lower_event_subscription(h, callee, args);
            }
        }

        let kind = if h.periodic {
            // `tb_periodic_literal` answers `None` for a NON-POSITIVE
            // literal as well as a non-literal, so this arm has two
            // inputs and they do not share a verdict source:
            //
            //   * a named period — v1 registers the closure where the
            //     statement is WRITTEN, after the impl's `let`s, so it
            //     resolves correctly. Built and run: 10 firings in 21
            //     cycles at period 2, with a shadowing `const` present.
            //     That is the escape hatch this arm's suggestion offers,
            //     and it is real. (Divergence 78 — the same construct
            //     mis-lowers at four other landings precisely because
            //     they register ABOVE the `let`s.)
            //   * `on 0 cycles` — v1 emits the handler and its own
            //     `period > 0` guard never lets it fire. Built and run:
            //     0 firings in 21 cycles. The program asked for a
            //     handler and got a no-op, silently.
            //
            // Worst-under-arm, so the arm is `SilentlyMisLowers` even
            // though the row that motivated its old `Unsupported` is
            // still a genuine escape hatch. Splitting on
            // `parse_int_literal_expr(..) == Some(0)` would recover it
            // and is not done here — the detail names both instead.
            let period = super::tb_periodic_literal(&h.event).ok_or_else(|| {
                not_implemented(
                    "an `on <N> cycles` handler with a non-literal or non-positive period",
                    "`on 0 cycles` makes v1 emit a handler its own `period > 0` guard \
                     never fires, so the registration is a silent no-op; a NAMED period \
                     does work here, because a statement-position handler is registered \
                     after the impl's `let` bindings",
                    V1Status::SilentlyMisLowers,
                )
            })?;
            CycleHandlerKind::Periodic { period }
        } else {
            // The trigger is re-evaluated every cycle inside the
            // registration closure, which owns no statement slot — same
            // constraint as a concurrent check body.
            let trigger = self.with_check_body(&[], "an `on <bool-expr>` trigger", |b| {
                b.lower_expr(&h.event)
            })?;
            CycleHandlerKind::Trigger {
                trigger,
                edge: crate::ir::CycleEdge::from_ast(h.edge),
            }
        };

        // The body becomes its own zero-parameter `TestHook` function.
        // It cannot see the enclosing function's locals (run and hook are
        // separate IR functions, so v1's shared `[&]` capture is not
        // representable); testbench fields and DUT ports resolve as usual,
        // and an unresolved name is reported by the ordinary lookup path.
        let pending_id = self.reserve_pending_function();
        let mut b = FuncBuilder::new(self.ctx, self.helpers, self.side_tables);
        super::reserve_tb_record_names(&mut b, self.ctx);
        b.lower_block_stmts(&h.body)?;
        if !b.is_terminated() {
            b.terminate(Terminator::Return);
        }
        let f = b.finish(
            pending_id,
            format!("_on_handler_{}", pending_id.0),
            crate::ir::FunctionKind::TestHook,
            self.ctx.owner,
        )?;

        // The handler runs FROM the per-cycle checker pass, which `tick()`
        // drives. A `wait` in the body lowers to `tick()`, so the body
        // would re-enter the checker pass that called it and recurse
        // until the stack runs out. v1 has the same shape and the same
        // hazard; refuse rather than reproduce it.
        if super::function_suspends(&f) {
            return Err(unsupported(
                "a `wait` inside a statement-position `on` handler body",
                "the body runs from the per-cycle checker pass, and a wait there re-enters \
                 that same pass — move the wait into the run body, or gate the run body on \
                 the same condition with `wait until`",
            ));
        }
        self.commit_pending_function(pending_id, f);
        let id = {
            let mut tables = self.side_tables.borrow_mut();
            let id = crate::ir::CycleHandlerId(tables.cycle_handlers.len() as u32);
            tables.cycle_handlers.push(CycleHandlerSchema {
                kind,
                function: pending_id,
                phase: crate::ir::HandlerPhase::from_ast(h.phase),
            });
            id
        };
        self.push(Stmt::CycleHandler(id));
        Ok(())
    }

    /// An explicit `assert property NAME` / `assume property NAME` /
    /// `cover property NAME` whose `NAME` is not a declared property.
    ///
    /// The `property` keyword does not change v1's dispatch (it classifies
    /// on the property table alone), so v1 falls through to the immediate
    /// form and emits the bare identifier — a name that does not exist in
    /// the generated C++. The keyword does, however, state the author's
    /// intent unambiguously, so name it in the diagnostic instead of
    /// letting the generic unresolved-name path report it. A program
    /// error under every backend, hence `Invalid` — no `--codegen v1`
    /// suggestion.
    fn reject_unknown_named_property(
        &self,
        kw: &str,
        v: &crate::ast::Verify,
    ) -> Result<(), LowerError> {
        if !v.property_kw {
            return Ok(());
        }
        let Some(expr) = &v.expr else {
            return Ok(());
        };
        if let ExprKind::Ident(id) = &*expr.kind {
            if !self.ctx.properties.contains_key(&id.name) {
                return Err(LowerError::Invalid(format!(
                    "{kw} property `{}`: no property declaration with that name",
                    id.name
                )));
            }
        }
        Ok(())
    }

    /// The check-body operand of a `assert`/`assume`/`cover`: a bare
    /// identifier naming a declared `property` resolves to that
    /// property's body, everything else is the written expression. Same
    /// resolution v1 performs at the reference site.
    fn resolve_check_body(&self, expr: &AstExpr) -> AstExpr {
        if let ExprKind::Ident(id) = &*expr.kind {
            if let Some(body) = self.ctx.properties.get(&id.name) {
                return body.clone();
            }
        }
        expr.clone()
    }

    /// Lower one concurrent check body (a property shape or a cover
    /// predicate) with the temporal latch slots pre-assigned.
    ///
    /// A check body executes inside a per-cycle closure that owns no
    /// statement slot, so nothing it lowers may push a statement into the
    /// current block: a hoisted DUT read, an inlined impure helper, or a
    /// transactor call edge would run ONCE at registration instead of
    /// every cycle. `f` is run with `temporal_slots` installed and the
    /// block's statement count is checked afterwards; a body that pushed
    /// is rejected rather than silently mis-lowered.
    fn with_check_body<T>(
        &mut self,
        temporals: &[crate::codegen::cpp_tb::Temporal],
        construct: &str,
        f: impl FnOnce(&mut Self) -> Result<T, LowerError>,
    ) -> Result<T, LowerError> {
        let saved = std::mem::take(&mut self.temporal_slots);
        for (i, t) in temporals.iter().enumerate() {
            let kind = match t.kind {
                crate::ast::SystemFn::Past => crate::ir::TemporalFn::Past,
                crate::ast::SystemFn::Rose => crate::ir::TemporalFn::Rose,
                crate::ast::SystemFn::Fell => crate::ir::TemporalFn::Fell,
                crate::ast::SystemFn::Stable => crate::ir::TemporalFn::Stable,
                // `collect_temporal_occurrences` only yields the four
                // temporal readings; `clog2` is filtered out upstream.
                crate::ast::SystemFn::Clog2 => continue,
            };
            self.temporal_slots
                .insert((t.call_span.start, t.call_span.end), (i as u32, kind));
        }
        // A body that pushes a statement, opens a block, or moves the
        // cursor has escaped the closure: an inlined suspending helper
        // splits the CFG, so checking the current block's length alone
        // would miss it.
        let before = (
            self.current,
            self.blocks.len(),
            self.blocks[self.current].stmts.len(),
        );
        let out = f(self);
        self.temporal_slots = saved;
        let out = out?;
        let after = (
            self.current,
            self.blocks.len(),
            self.blocks[self.current].stmts.len(),
        );
        if after != before {
            return Err(unsupported(
                construct,
                "the body needs a statement-level step (a hoisted DUT read, an inlined \
                 helper, or a transactor call), which cannot run inside a per-cycle \
                 concurrent check",
            ));
        }
        Ok(out)
    }

    /// Lower the latch operands of a check body. Each is lowered with NO
    /// slot map installed, so a nested `past(past(x))` is rejected by the
    /// ordinary temporal-system-call gate rather than silently aliasing a
    /// slot (v1's occurrence walk likewise does not recurse into operands).
    fn lower_temporal_slots(
        &mut self,
        temporals: &[crate::codegen::cpp_tb::Temporal],
        construct: &str,
    ) -> Result<Vec<crate::ir::TemporalSlot>, LowerError> {
        let mut out = Vec::with_capacity(temporals.len());
        for t in temporals {
            let inner = self.with_check_body(&[], construct, |b| b.lower_expr(&t.inner))?;
            out.push(crate::ir::TemporalSlot { inner });
        }
        Ok(out)
    }

    /// `assert`/`assume` whose operand names a declared property or
    /// carries a temporal operator: a concurrent check registered here
    /// and evaluated on every primary-clock edge from this point on.
    fn lower_property_check(
        &mut self,
        severity: crate::ir::PropertySeverity,
        v: &crate::ast::Verify,
        raw: &AstExpr,
    ) -> Result<(), LowerError> {
        use crate::ast::BinaryOp;
        use crate::ir::{PropertyCheckSchema, PropertyShape};

        let construct = match severity {
            crate::ir::PropertySeverity::Fail => "a concurrent `assert`",
            crate::ir::PropertySeverity::AssumeFail => "a concurrent `assume`",
        };
        self.require_test_body(construct)?;
        let body = self.resolve_check_body(raw);
        let temporals = crate::codegen::cpp_tb::collect_temporal_occurrences(&body);
        let slots = self.lower_temporal_slots(&temporals, construct)?;

        let shape = match &*body.kind {
            ExprKind::Binary {
                op: op @ (BinaryOp::PipeImplies | BinaryOp::PipeImpliesNext),
                lhs,
                rhs,
            } => {
                let next = matches!(op, BinaryOp::PipeImpliesNext);
                let (ante, cons) = self.with_check_body(&temporals, construct, |b| {
                    Ok((b.lower_expr(lhs)?, b.lower_expr(rhs)?))
                })?;
                if next {
                    PropertyShape::ImpliesNext { ante, cons }
                } else {
                    PropertyShape::Implies { ante, cons }
                }
            }
            _ => PropertyShape::Invariant(
                self.with_check_body(&temporals, construct, |b| b.lower_expr(&body))?,
            ),
        };

        // `assert <temporal> else fail("...")` — the clause names what
        // the failure means, which is strictly more useful than the
        // generic property line, so it replaces it. The message renders
        // in the same per-cycle closure as the condition, so it is held
        // to the same rule: it may read locals and ports, but it may not
        // push a statement into the test.
        //
        // Lowered with NO slot map, like a latch operand. `lower_fmt`
        // re-parses each `${…}` capture as a standalone fragment, whose
        // spans are relative to the fragment rather than to the file, so
        // a capture's span can collide with a real temporal occurrence
        // and get rewritten into that occurrence's `Expr::TemporalSlot`.
        // With the map empty a `${past(x)}` reaches the ordinary
        // temporal gate and is rejected by name instead.
        let message = match v.else_fail.as_ref() {
            Some(e) => {
                let msg = self.else_fail_literal(e)?;
                Some(self.with_check_body(&[], construct, |b| b.lower_fmt(&msg))?)
            }
            None => None,
        };

        let id = {
            let mut tables = self.side_tables.borrow_mut();
            let id = crate::ir::PropertyCheckId(tables.property_checks.len() as u32);
            tables.property_checks.push(PropertyCheckSchema {
                tag: format!("_p_{}_{}", raw.span.start, raw.span.end),
                label: crate::codegen::cpp_tb::property_label(v, raw),
                severity,
                shape,
                temporals: slots,
                message,
            });
            id
        };
        self.push(Stmt::PropertyCheck(id));
        Ok(())
    }

    /// `cover <expr>` (spec §5) — a flat witness counter bumped on every
    /// primary-clock edge the predicate holds, reported at end of test.
    pub(crate) fn lower_cover(&mut self, v: &crate::ast::Verify) -> Result<(), LowerError> {
        self.reject_unknown_named_property("cover", v)?;
        let Some(raw) = &v.expr else {
            // v1 returns silently on a bodyless `cover`; the parser
            // always produces one, so this is unreachable in practice.
            return Ok(());
        };
        self.require_test_body("a `cover` witness")?;
        // A `cover` has no failure line to name: it counts the cycles
        // its predicate held and reports hit/total. The parser accepts
        // the clause on any `verify` statement, and v1 drops it here
        // without a word — the same "written, accepted, lost" shape
        // that `else fail(...)` on a concurrent `assert` had.
        if v.else_fail.is_some() {
            return Err(LowerError::Invalid(
                "`cover` counts witnesses; it has no failure to report, so an \
                 `else fail(...)` clause has nothing to name (use `assert` if the \
                 condition must hold)"
                    .to_string(),
            ));
        }
        let construct = "a `cover` witness";
        let label = match &*raw.kind {
            ExprKind::Ident(id) => id.name.clone(),
            _ => format!("cov_{}_{}", raw.span.start, raw.span.end),
        };
        let body = self.resolve_check_body(raw);
        let temporals = crate::codegen::cpp_tb::collect_temporal_occurrences(&body);
        let slots = self.lower_temporal_slots(&temporals, construct)?;
        let cond = self.with_check_body(&temporals, construct, |b| b.lower_expr(&body))?;

        let id = {
            let mut tables = self.side_tables.borrow_mut();
            let id = crate::ir::CoverCheckId(tables.cover_checks.len() as u32);
            tables.cover_checks.push(crate::ir::CoverCheckSchema {
                // The index, not just the span, keys the counter: one
                // source `cover` inlined at two call sites is two
                // registrations, and two file-scope statics with the same
                // name would not compile.
                tag: format!("c{}_{}_{}", id.0, raw.span.start, raw.span.end),
                label,
                cond,
                temporals: slots,
            });
            id
        };
        self.push(Stmt::CoverCheck(id));
        Ok(())
    }

    /// `assume <plain bool>` — an immediate, point-in-time assumption.
    /// Logs `ASSUME` on violation and, unlike `assert`, does NOT bump the
    /// error counter (v1's `emit_inline_assume`).
    pub(crate) fn lower_assume(&mut self, v: &crate::ast::Verify) -> Result<(), LowerError> {
        self.reject_unknown_named_property("assume", v)?;
        let Some(expr) = &v.expr else {
            return Err(LowerError::Invalid("assume without expression".to_string()));
        };
        if crate::codegen::cpp_tb::is_concurrent_assertion(expr, &self.ctx.properties) {
            return self.lower_property_check(crate::ir::PropertySeverity::AssumeFail, v, expr);
        }
        // Ports stay inline (lazy eval, like an immediate assert); a
        // transactor call edge hoists ahead of the check.
        let cond = self.lower_expr(expr)?;
        let cond = self.hoist_transactor_calls(cond);
        let msg = match v.else_fail.as_ref() {
            Some(e) => self.else_fail_literal(e)?,
            None => "assumption failed".to_string(),
        };
        let on_fail = self.lower_fmt(&msg)?;
        self.push(Stmt::AssumeCheck { cond, on_fail });
        Ok(())
    }

    /// A destination for a queue `pop` whose value is discarded
    /// (`q.pop()` in statement position). The pop still has to run —
    /// `pop` mutates the queue, which is the whole point of writing it
    /// bare — and every IR pop carries a destination, so the value
    /// lands in a temp nothing reads. v1 emits the same call and drops
    /// the return value.
    ///
    /// Typed from the queue's element so a record-element pop declares a
    /// struct slot rather than a scalar one; a scalar element leaves the
    /// temp at the default u64, exactly as a `let` with no annotation
    /// does on the bound path.
    fn discard_slot(&mut self, elem: crate::ir::QueueElem) -> crate::ir::LocalId {
        let dest = self.fresh_temp();
        if let crate::ir::QueueElem::Record(rid) = elem {
            self.set_local_type(dest, IrType::Record(rid));
        }
        dest
    }

    /// The string literal in an `else fail("...")` clause. A non-literal
    /// message is rejected the same way for `assert` and `assume`, in
    /// the immediate and concurrent forms alike.
    fn else_fail_literal(&self, e: &AstExpr) -> Result<String, LowerError> {
        match &*e.kind {
            ExprKind::String(s) => Ok(s.clone()),
            _ => Err(not_implemented(
                "non-string-literal `else fail(...)` message",
                "interpolate the value into the literal instead — \
                 `else fail(\"x=${v}\")`",
                V1Status::SilentlyMisLowers,
            )),
        }
    }

    fn lower_assert(&mut self, v: &crate::ast::Verify) -> Result<(), LowerError> {
        self.reject_unknown_named_property("assert", v)?;
        let Some(expr) = &v.expr else {
            return Err(LowerError::Invalid("assert without expression".to_string()));
        };
        // Spec §2 LL(1) table: a bare identifier naming a declared
        // `property`, or any expression carrying a temporal operator, is
        // a CONCURRENT assertion (evaluated every primary-clock edge);
        // everything else is the immediate point-in-time check. The
        // legacy `property` keyword still parses but no longer changes
        // dispatch — same rule as v1's `is_concurrent_assertion`.
        if crate::codegen::cpp_tb::is_concurrent_assertion(expr, &self.ctx.properties) {
            return self.lower_property_check(crate::ir::PropertySeverity::Fail, v, expr);
        }
        // Ports stay inline (lazy assert eval), but a transactor-method
        // call edge cannot stay nested in the condition — hoist it into a
        // preceding `Stmt::TransactorCall` (the seam rule, and the call may
        // advance simulated time). `(helper.read(0) & 1) == 1`.
        let cond = self.lower_expr(expr)?; // ports allowed in assert conditions
        let cond = self.hoist_transactor_calls(cond);
        let msg = match v.else_fail.as_ref() {
            Some(e) => self.else_fail_literal(e)?,
            None => "assertion failed".to_string(),
        };
        let on_fail = self.lower_fmt(&msg)?;
        self.push(Stmt::AssertCheck { cond, on_fail });
        Ok(())
    }

    fn lower_fail_msg(&mut self, msg: &crate::ast::Expr) -> Result<FmtArgs, LowerError> {
        // A bare `fail(...)` statement lowers to an always-failing
        // `AssertCheck`, so its message is unconditionally evaluated —
        // CFG-inlined calls may be hoisted (matches v1's inline eval).
        match &*msg.kind {
            ExprKind::String(s) => self.lower_fmt_hoisting(s),
            // v1 does not evaluate a non-literal message: it emits the
            // fixed text "fail() with non-string arg" and DROPS the
            // expression, so the failure line says nothing about the
            // value that caused it.
            _ => Err(not_implemented(
                "non-string-literal `fail(...)` message",
                "interpolate the value into the literal instead — `fail(\"x=${v}\")`",
                V1Status::SilentlyMisLowers,
            )),
        }
    }

    fn lower_log(&mut self, args: &[CallArg], file: Option<String>) -> Result<(), LowerError> {
        // Both extractors below match `CallArg::Expr` only, so a NAMED
        // argument hides whatever it wraps — and v1's do the same, so
        // this is a silent mis-lowering under both backends rather than
        // a divergence:
        //
        //   `log(level = fatal, "BOOM")`  -> `sim_log_line("INFO", "BOOM")`
        //   `log(fatal, msg = "BOOM")`    -> `sim_log_line("FATAL", "")`
        //
        // The first is the dangerous one. A `fatal` silently becomes an
        // `info`: no `ctx.errors++`, no `_fatal`, and a test that should
        // abort passes green. The severity guard further down rejects a
        // TYPO (`log(errror, ...)`) for exactly this reason — "rejecting
        // it is what makes `log(error, ...)` trustworthy" — and a named
        // severity walked straight past it.
        //
        // Gated on what the name HIDES **and on the slot being empty**.
        // The extractors take positional matches only, so a named
        // argument costs the user something exactly when a slot it could
        // have filled is left unfilled:
        //
        //   * `log` needs ONE string (the message).
        //   * `logf` needs TWO — the path is the first, the message the
        //     next. A named path with only one positional string left
        //     promotes the message to filename:
        //     `logf(path = "t.log", error, "BOOM", "EXTRA")` writes to a
        //     file called `BOOM`. Modelling only the message slot missed
        //     that; this counts strings instead of comparing them, which
        //     also retires a `file`-equality test nothing pinned.
        //   * a severity slot is filled by any positional bare ident.
        //
        // With a slot filled positionally the named argument is inert
        // under both backends and must lower: `log(fatal, "BOOM", lvl =
        // warn)` emits `FATAL`, `log(level = fatal, error, "BOOM")`
        // emits `ERROR`, and `logf(p = "a.log", "t.log", error, "BOOM")`
        // logs `BOOM`. Refusing those would be refusing correct
        // programs, which two earlier versions of this gate did.
        let positional_strings = args
            .iter()
            .filter(|a| matches!(a, CallArg::Expr(e) if matches!(&*e.kind, ExprKind::String(_))))
            .count();
        let strings_needed = if file.is_some() { 2 } else { 1 };
        let has_positional_sev = args
            .iter()
            .any(|a| matches!(a, CallArg::Expr(e) if matches!(&*e.kind, ExprKind::Ident(_))));
        let what = if file.is_some() { "logf" } else { "log" };
        for a in args {
            let CallArg::Named { name, value } = a else {
                continue;
            };
            let hidden = match &*value.kind {
                // Only a REAL severity can be hidden. `who = nosuch` was
                // never going to become one — positionally it would have
                // been rejected by the severity guard below, not
                // silently used — so refusing it claims a loss that
                // cannot happen.
                ExprKind::Ident(id)
                    if !has_positional_sev && is_log_severity(&id.name.to_lowercase()) =>
                {
                    "a severity"
                }
                ExprKind::String(_) if positional_strings < strings_needed => {
                    if file.is_some() {
                        "a path or message"
                    } else {
                        "the message"
                    }
                }
                _ => continue,
            };
            return Err(not_implemented(
                &format!(
                    "a named argument `{}` carrying {hidden} in `{what}`",
                    name.name
                ),
                "both backends read the severity, path and message positionally and skip \
                 named arguments entirely, so with no positional one to fall back on this \
                 drops what the name wraps — a named `fatal` becomes `info`, which bumps \
                 no failure counter",
                V1Status::SilentlyMisLowers,
            ));
        }
        // Mirror v1's extraction rules: first bare ident is the
        // severity (default info); the message is the first positional
        // string AFTER the one `logf` consumes as its path.
        //
        // v1 CONSUMES the path (`StmtKind::LogF` splits the first
        // positional string out of the list and hands `emit_log` what
        // is left), so its rule is POSITIONAL. This used to compare
        // each string's VALUE against the path instead, which is the
        // same answer only while the message happens to differ from the
        // path:
        //
        //   `logf("t.log", "t.log", error, "BOOM")` gave TB-IR "BOOM"
        //   where v1 emits "t.log", and `logf("t.log", error, "t.log")`
        //   gave TB-IR "" where v1 emits "t.log".
        //
        // Both backends accept both programs, so that was a live silent
        // DIVERGENCE, not a shared mis-lowering.
        //
        // The named-argument guard above deliberately still runs on the
        // FULL argument list, before the path is consumed: it exists to
        // catch a named argument that leaves a positional slot empty,
        // and counting a list the path has already been removed from
        // would tell it there was one fewer slot to fill.
        let sev = args
            .iter()
            .find_map(|a| match a {
                CallArg::Expr(e) => match &*e.kind {
                    ExprKind::Ident(id) => Some(id.name.to_lowercase()),
                    _ => None,
                },
                _ => None,
            })
            .unwrap_or_else(|| "info".to_string());
        let msg = args
            .iter()
            .filter_map(|a| match a {
                CallArg::Expr(e) => match &*e.kind {
                    ExprKind::String(s) => Some(s.clone()),
                    _ => None,
                },
                _ => None,
            })
            .nth(usize::from(file.is_some()))
            .unwrap_or_default();

        let base = match sev.as_str() {
            "debug" => FileLogLevel::Debug,
            "info" => FileLogLevel::Info,
            "warn" => FileLogLevel::Warn,
            "error" => FileLogLevel::Error,
            "fatal" => FileLogLevel::Fatal,
            // Spec §7.7: `Severity` is a closed enum
            // (`debug`/`info`/`warn`/`error`/`fatal`), so anything else
            // names no severity. This is not a missing feature in
            // either backend — v1 accepts it by uppercasing whatever
            // ident it finds, which turns a typo (`log(errror, ...)`)
            // into an `ERRROR`-tagged line that never bumps the failure
            // counter. Rejecting it is what makes `log(error, ...)`
            // trustworthy.
            other => {
                return Err(LowerError::Invalid(format!(
                    "`{other}` is not a log severity; spec §7.7 defines \
                     `Severity` as debug, info, warn, error, fatal"
                )));
            }
        };
        let level = match file {
            Some(path) => LogLevel::File { path, level: base },
            None => match base {
                FileLogLevel::Debug => LogLevel::Debug,
                FileLogLevel::Info => LogLevel::Info,
                FileLogLevel::Warn => LogLevel::Warn,
                FileLogLevel::Error => LogLevel::Error,
                FileLogLevel::Fatal => LogLevel::Fatal,
            },
        };
        let fmt_args = self.lower_fmt_hoisting(&msg)?;
        self.push(Stmt::Log {
            level,
            args: fmt_args,
        });
        Ok(())
    }

    /// `let x = _tb.m(...)` — testbench method call RHS, CFG-inlined.
    /// Returns `true` when the RHS was such a call (declared `x` holds
    /// the inlined return value).
    pub(crate) fn try_lower_tb_method_let(
        &mut self,
        l: &crate::ast::LetStmt,
        value: &crate::ast::Expr,
    ) -> Result<bool, LowerError> {
        let ExprKind::Call { callee, args } = &*value.kind else {
            return Ok(false);
        };
        let Some(m) = self.tb_method_call_name(callee) else {
            return Ok(false);
        };
        let v = self.lower_tb_method_call(&m, args)?;
        // The TESTBENCH-METHOD spelling of the component-method rule a
        // screen up. `lower_tb_method_call` already typed its return
        // temp, and this dropped that on the floor: an unannotated
        // `let r = make_result(7)` on a `-> Beat` method left `r`
        // untyped, so every slot guard read it as a scalar and called
        // the program `Invalid` — while v1 emits
        // `auto r = Tb_make_result(_tb, 7);` and compiles.
        let ret_record = self.record_id_of_expr(&v);
        // NO sequence arm here, deliberately, and the first attempt at
        // this batch wrote one that could never fire. A testbench
        // method's return temp is typed by `ir_type_of_with_records`,
        // which answers `Unknown` for `TSeq<T>` — so there is no
        // sequence type to carry, and code reading `expr_type` for one
        // is dead.
        //
        // That is a real gap, and it is OLDER than the slot rule: the
        // whole inlined chain is untyped, parameter included, so tbir
        // emits `uint64_t s = 0; uint64_t ys = 0;` and then `s = xs;`,
        // which g++ refuses ("cannot convert `std::vector<Beat>` to
        // `uint64_t`") while v1 compiles
        // `[&](Tb& self, const std::vector<Beat>& s)`. Reproducible on
        // the merge base, so it is not this rule's to fix — typing that
        // chain changes the emitted C++ and wants its own measurement.
        // Recorded in divergence 114; the `Unknown` rule keeps the slot
        // check from adding a false rejection on top of it meanwhile.
        // No "…unless the annotation names that same record" escape,
        // because by construction the annotation here never can. A `let`
        // whose declared type names a record is claimed a thousand lines
        // up, by the record-typed-local branch of `lower_let`, which
        // returns on every path: same record → the struct-copy `Assign`,
        // different record → `record_assign_mismatch`. So the only
        // annotations that reach this lane are the ones that name no
        // record at all — `int`, `bit`, an enum, an unknown name — and
        // for those a record-returning RHS is always the mismatch.
        // (Probed: `let r : Beat` and `let r : Pkg.Beat` lower, `let r :
        // Other` reports the transaction-local mismatch, and only
        // `let r : int` lands here.)
        if let (Some(_), Some(rid)) = (&l.ty, ret_record) {
            return Err(LowerError::Invalid(format!(
                "`let {}` is declared with a non-record type and initialised from a `{}`",
                l.name.name,
                self.ctx.records[rid.index()].name
            )));
        }
        let id = self.declare(&l.name.name);
        if let Some(rid) = ret_record {
            self.set_local_type(id, IrType::Record(rid));
        }
        self.push(Stmt::Assign(id, v));
        Ok(true)
    }

    /// Lower an interpolated message string into pre-parsed `FmtArgs`,
    /// reusing v1's `process_interp` so format tokens (and therefore
    /// runtime log/trace text) are byte-identical across backends.
    ///
    /// Used by conditionally-evaluated messages (an assert's `else
    /// fail(...)`, a timeout header) — CFG-inlined calls stay rejected
    /// there, because eagerly hoisting them ahead of the check would run
    /// the inlined body even when the message is never emitted.
    pub(crate) fn lower_fmt(&mut self, msg: &str) -> Result<FmtArgs, LowerError> {
        self.lower_fmt_impl(msg, false)
    }

    /// `lower_fmt` for UNCONDITIONALLY-evaluated messages (`log(...)`, a
    /// bare `fail(...)`): every interpolation is evaluated exactly once at
    /// the statement, so a CFG-inlined call can be hoisted ahead of it
    /// with identical observable order/count.
    pub(crate) fn lower_fmt_hoisting(&mut self, msg: &str) -> Result<FmtArgs, LowerError> {
        self.lower_fmt_impl(msg, true)
    }

    fn lower_fmt_impl(&mut self, msg: &str, hoist: bool) -> Result<FmtArgs, LowerError> {
        let (fmt, caps) = crate::codegen::cpp_tb::process_interp(msg);
        let mut args = Vec::with_capacity(caps.len());
        for c in caps {
            // `Invalid`, not `Unsupported`. A capture that does not parse is
            // a static error in the program, not a gap in this backend —
            // string interpolation is supported — and the old `Unsupported`
            // mapping offered `--codegen v1` as the way out, which was the
            // backend that wrote the raw HARC text into its C++ for the same
            // input (harc#593). The parser now rejects such a capture up
            // front, so this should be unreachable; it stays as a
            // fail-closed backstop rather than a claim about v1.
            let mut parsed = crate::parser::parse_expr_fragment(&c.expr).map_err(|_| {
                LowerError::Invalid(format!(
                    "`${{{}}}` is not an expression; an interpolation holds one \
                     complete expression, optionally followed by `:` and a format spec",
                    c.expr
                ))
            })?;
            // A message interpolation lowers lazily (the captured expr is
            // re-evaluated at the log/failure site), so a CFG-inlined call
            // — an impure helper or a testbench method — cannot live inside
            // it. For an UNCONDITIONALLY-evaluated message, v1 evaluates
            // each `${...}` exactly once, in place, at the message point;
            // mirror that by eagerly HOISTING every such call into a fresh
            // temp before the statement, then referencing the temp in the
            // format arg. Hoisting preserves the evaluation-count-of-one
            // and the left-to-right capture order, so the runtime trace is
            // identical to v1's inline form.
            //
            // Bus/TLM calls suspend mid-message and are structurally
            // unhoistable (their `wait`s would land between hoist and log);
            // those keep the reject in `lower_expr`/`try_lower_bus_call`.
            if hoist {
                self.hoist_fmt_calls(&mut parsed)?;
            }

            // Ports are allowed in format args, but DUT/sync-touching
            // helper calls are not — messages evaluate lazily at the
            // log/failure site, and an inlined CFG cannot.
            let was = self.in_fmt_args;
            self.in_fmt_args = true;
            let lowered = self.lower_expr(&parsed);
            self.in_fmt_args = was;
            let expr = lowered?;
            args.push(FmtArg {
                expr,
                wide_hex: c.wide_hex,
            });
        }
        Ok(FmtArgs { fmt, args })
    }

    /// Eagerly hoist CFG-inlined calls out of one message-interpolation
    /// AST expression, in place. Any call that would be rejected inside a
    /// `${...}` (an impure/DUT-touching file-scope helper, or a testbench
    /// method) is lowered NOW — outside `in_fmt_args`, so its inlined CFG
    /// lands as ordinary statements before the message — assigned to a
    /// fresh unique `__msg_tmpN` local, and the call node is replaced by an
    /// `Ident(__msg_tmpN)` so the later fmt-arg lowering just reads the
    /// temp. Recurses depth-first, left-to-right (argument/operand order),
    /// and does NOT descend into a call once it has been hoisted whole.
    /// Pure helpers and width-cast intrinsics lower fine inside a message,
    /// so they are left in place (only their sub-expressions are visited,
    /// in case an impure call is nested as an argument).
    fn hoist_fmt_calls(&mut self, e: &mut AstExpr) -> Result<(), LowerError> {
        // A suspending bus/TLM (or transactor) method call inside an
        // unconditionally-evaluated message: hoist it through the
        // statement-position lowering (which drives the protocol and lands
        // the `wait`/suspend before the message), then read the resolved
        // value in the fmt arg. This is exactly the manual rewrite the
        // former reject diagnostic suggested. Checked before the
        // impure-helper hoist so a bus/transactor call is never routed
        // through `lower_expr` (which rejects it in expression position).
        if self.hoist_fmt_suspending_call(e)? {
            return Ok(());
        }
        if self.fmt_call_needs_hoist(e) {
            // Hoist the whole call once. Lower it in normal (non-fmt-args)
            // context so the impure/tb-method inline emits its statements
            // into the current block ahead of the message.
            let call = std::mem::replace(e, AstExpr::new(ExprKind::Bool(false), e.span));
            let span = call.span;
            let lowered = self.lower_expr(&call)?;
            let ty = self.expr_type(&lowered).unwrap_or(IrType::Unknown);
            // Use a source name that is unique BEFORE `declare` sees it, so
            // `declare` stores it verbatim (no dedup suffix) and scope-keys
            // it under that exact name — the fmt-arg `Ident` below then
            // resolves to precisely this local via `lookup`.
            let name = self.fresh_msg_tmp_name();
            let tmp = self.declare(&name);
            self.set_local_type(tmp, ty);
            self.push(Stmt::Assign(tmp, lowered));
            *e = AstExpr::new(ExprKind::Ident(Ident { name, span }), span);
            return Ok(());
        }
        // Not a hoisted call itself — descend into children in source
        // (left-to-right) order to catch impure calls nested inside pure
        // calls, operators, casts, etc.
        for child in fmt_expr_children_mut(e) {
            self.hoist_fmt_calls(child)?;
        }
        Ok(())
    }

    /// If `e` is a SUSPENDING call — a bus/TLM `tlm_method` call
    /// (`mem.read(a)`) or a transactor method call (`xact.read(a)`) — hoist
    /// it out of an unconditionally-evaluated message: lower it through its
    /// statement-position path (which drives the protocol / method body,
    /// including the wait/suspend) into a fresh `__msg_tmpN` local, and
    /// replace the call node with `Ident(__msg_tmpN)` so the later fmt-arg
    /// lowering just reads the resolved value. Returns `Ok(true)` when the
    /// node was such a call and was hoisted; `Ok(false)` otherwise (the
    /// caller falls through to the impure-helper hoist / child recursion).
    ///
    /// Only reached from `hoist_fmt_calls`, which runs exclusively for
    /// UNCONDITIONALLY-evaluated messages and only descends into
    /// always-evaluated operands (`fmt_expr_children_mut` skips
    /// short-circuit `&&`/`||` RHS and ternary branches), so hoisting here
    /// never changes the source-level evaluation count/order — a suspending
    /// call in a lazy/conditional position still reaches the reject in
    /// `lower_expr`.
    fn hoist_fmt_suspending_call(&mut self, e: &mut AstExpr) -> Result<bool, LowerError> {
        let ExprKind::Call { callee, .. } = &*e.kind else {
            return Ok(false);
        };
        // A bus channel `send`/`recv` handshake (`axil.r.recv()`) can also
        // suspend, but it is void or captures only a scalar and has no
        // in-message surface in v1's reject shape; the `tlm_method` call
        // edge (`bind.method(...)`) and the transactor method call are the
        // two value-bearing suspending forms rejected inside a message.
        let is_bus_tlm = matches!(&*callee.kind, ExprKind::Field { target, .. }
            if matches!(&*target.kind, ExprKind::Ident(id)
                if self.ctx.bus_bindings.contains_key(&id.name)));
        let is_transactor = self.as_transactor_call(callee)?.is_some();
        if !is_bus_tlm && !is_transactor {
            return Ok(false);
        }
        let span = e.span;
        let name = self.fresh_msg_tmp_name();
        if is_bus_tlm {
            // Route through the statement-position bus-call lowering with a
            // `Declare` destination: it declares `name`, drives the req/rsp
            // handshake, and binds the response into that local — the same
            // lowering as `let name = bind.method(...)`.
            let lowered = self.try_lower_bus_call(e, super::bus::BusCallDest::Declare(&name))?;
            debug_assert!(
                lowered,
                "bus tlm_method call failed to lower after classification"
            );
        } else {
            // Transactor method call edge with a result destination — the
            // same lowering as `let name = xact.method(...)`.
            let ExprKind::Call { callee, args } = &*e.kind else {
                unreachable!("classified as a call above");
            };
            let call = self
                .lower_transactor_call(callee, args, true)?
                .expect("transactor call failed to lower after classification");
            let id = self.declare(&name);
            self.push(Stmt::TransactorCall {
                dest: Some(id),
                call,
            });
        }
        *e = AstExpr::new(ExprKind::Ident(Ident { name, span }), span);
        Ok(true)
    }

    /// A `__msg_tmpN` source name guaranteed unique against every local
    /// declared so far, so `declare` keeps it verbatim (the `Ident` that
    /// references it must match the scope key exactly).
    fn fresh_msg_tmp_name(&mut self) -> String {
        loop {
            let candidate = format!("__msg_tmp{}", self.temp_counter);
            self.temp_counter += 1;
            if !self.local_names.contains(&candidate) {
                break candidate;
            }
        }
    }

    /// A `Call` expression that must be hoisted out of a message: one that
    /// CFG-inlines (impure/DUT-touching file-scope helper, or a testbench
    /// method) and therefore cannot lower inside a lazily-evaluated
    /// `${...}`. Pure helpers, extern fns, width-cast intrinsics, and
    /// value-query method calls lower fine in place, so they return false.
    /// Bus/TLM and transactor calls are deliberately NOT hoisted here:
    /// they can suspend, so they stay statement-only and keep their own
    /// rejects.
    fn fmt_call_needs_hoist(&self, e: &AstExpr) -> bool {
        let ExprKind::Call { callee, .. } = &*e.kind else {
            return false;
        };
        match &*callee.kind {
            // `f(...)` — an impure file-scope helper CFG-inlines; a pure
            // helper or extern fn does not.
            ExprKind::Ident(id) => self
                .helpers
                .get(&id.name)
                .map_or(false, |entry| !entry.pure),
            // `_tb.m(...)` — a testbench method always CFG-inlines.
            ExprKind::Field { .. } => self.tb_method_call_name(callee).is_some(),
            _ => false,
        }
    }
}

/// Children of an AST expression to visit when hoisting message calls,
/// in source (left-to-right) evaluation order. Deliberately does NOT
/// descend into a `Call`'s own subtree — the caller decides per-node
/// whether to hoist the whole call (and stop) or recurse into it.
fn fmt_expr_children_mut(e: &mut AstExpr) -> Vec<&mut AstExpr> {
    match &mut *e.kind {
        ExprKind::Paren(inner)
        | ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. } => vec![inner],
        ExprKind::Field { target, .. } | ExprKind::Index { target, .. } => vec![target],
        // Short-circuiting logical operators evaluate the RHS only
        // conditionally (v1 emits `&&`/`||`, which C++ short-circuits), so
        // a call in the RHS must NOT be hoisted unconditionally — descend
        // into the always-evaluated LHS only and leave any RHS call in
        // place to hit the existing lazy-message reject. Bitwise `&`/`|`
        // always evaluate both operands, so both are safe to visit.
        ExprKind::Binary { op, lhs, rhs } => {
            use crate::ast::BinaryOp as B;
            if matches!(op, B::AndAnd | B::OrOr | B::AndKw | B::OrKw) {
                vec![lhs]
            } else {
                vec![lhs, rhs]
            }
        }
        ExprKind::BitSlice { target, hi, lo } => vec![target, hi, lo],
        // A ternary evaluates only the taken branch, so only the always-
        // evaluated condition is safe to hoist from; a call in either
        // branch stays in place (and hits the lazy-message reject). (In
        // practice `process_interp` splits on `:`, so a `?:` inside a
        // `${...}` rarely survives parsing at all — this is belt-and-
        // suspenders for the forms that do.)
        ExprKind::Ternary { cond, .. } => vec![cond],
        // A pure call: descend into its arguments so an impure call nested
        // as an argument still hoists. (An impure call is caught by
        // `fmt_call_needs_hoist` before we ever recurse into it.)
        ExprKind::Call { callee, args } => {
            let mut v = vec![callee];
            for a in args {
                let (CallArg::Expr(inner) | CallArg::Named { value: inner, .. }) = a;
                v.push(inner);
            }
            v
        }
        // Leaves and forms that never contain a hoistable call in a
        // message position.
        _ => Vec::new(),
    }
}

/// The `callee` of a `let ... = <callee>(...)` initializer when the RHS is
/// a call (the shape a `.pop()` access takes), or `None`.
fn pop_call_parts(value: &Option<crate::ast::Expr>) -> Option<(&crate::ast::Expr, &[CallArg])> {
    let v = value.as_ref()?;
    match &*v.kind {
        ExprKind::Call { callee, args } => Some((callee, args.as_slice())),
        _ => None,
    }
}

/// What a typed slot accepts, or what a value presents, to the precision
/// lowering can actually decide.
///
/// `Unknown` is the load-bearing variant, and the first version of this
/// enum did not have it: everything lowering could not name fell into
/// `Scalar`, which turns an ABSENCE of information into the positive
/// claim "this is not a record". Three well-typed programs were called
/// `Invalid` on the strength of that claim — a `-> TSeq<T>` method
/// result, a `TSeq<T>` transactor parameter, a `TSeq<T>` tseq parameter
/// — each one compiled by v1 and each one rejected by the DEFAULT
/// backend. `component_base_id` already states the rule this broke:
/// callers must treat "cannot tell" as cannot tell, never as "not a
/// record".
///
/// So the guard is conservative by construction — it rejects only when
/// it can name BOTH sides. A slot whose declared type this compiler
/// cannot resolve (an enum, an unresolved path) is unchecked rather
/// than assumed scalar; the program is no worse off than before any of
/// this existed, and a wrong verdict is worse than no verdict.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// A declared record — `t: Beat`, `queue<Beat>`, `event<Beat>`.
    Record(crate::ir::RecordId),
    /// A `TSeq<T>`; `Some(rid)` when the element is a declared record.
    Seq(Option<crate::ir::RecordId>),
    /// Known to be a scalar — `uint<N>`, `sint<N>`, `bit`, `bool`.
    Scalar,
    /// Not nameable here. Never compared, never reported.
    Unknown,
}

impl Slot {
    /// For the many slots that genuinely are record-or-scalar — a queue
    /// element, an event payload — where `None` means the slot was
    /// declared scalar rather than "could not be read".
    fn of_record(want: Option<crate::ir::RecordId>) -> Self {
        match want {
            Some(rid) => Slot::Record(rid),
            None => Slot::Scalar,
        }
    }

    fn of_ir(want: &IrType) -> Self {
        match want {
            IrType::Record(rid) => Slot::Record(*rid),
            IrType::RecordSeq(rid) => Slot::Seq(Some(*rid)),
            IrType::Seq(_) => Slot::Seq(None),
            IrType::UInt(_) | IrType::SInt(_) | IrType::Bool => Slot::Scalar,
            _ => Slot::Unknown,
        }
    }

    fn known(self) -> bool {
        !matches!(self, Slot::Unknown)
    }
}

/// `pop` removes and returns the front element, so it takes nothing.
///
/// Every `pop` branch used to check only the method NAME and drop the
/// argument list on the floor, which let `q.pop(7, 9)` lower and emit
/// cleanly while v1 emitted `_tb.pend.pop(7, 9);` — g++: "no matching
/// function for call to `harc_rt::HarcQueue<long unsigned int>::pop(int,
/// int)`". Neither backend runs it, so this is `Invalid`, and it is the
/// mirror of the `push` branches' `[CallArg::Expr(arg)]` pattern, which
/// has always been exact.
fn queue_pop_takes_no_arguments(what: &str, args: &[CallArg]) -> Result<(), LowerError> {
    if args.is_empty() {
        return Ok(());
    }
    Err(LowerError::Invalid(format!(
        "{what}: `pop` takes no arguments (it removes and returns the front element), \
         but {} were passed",
        args.len()
    )))
}

/// Explicit bit width of a typed scalar `let` annotation
/// (`let s64 : uint<64> = ...`), for the width-method receiver
/// inference. Mirrors v1's `let_widths` seeding: explicit widths only.
fn typed_let_width(t: &TypeExpr) -> Option<u32> {
    let TypeExpr::Builtin { name, args, .. } = t else {
        return None;
    };
    if !matches!(
        name,
        BuiltinTy::UInt
            | BuiltinTy::UIntCap
            | BuiltinTy::SInt
            | BuiltinTy::SIntCap
            | BuiltinTy::Bits
    ) {
        return None;
    }
    match args.first()? {
        TypeArg::Expr(e) => match &*e.kind {
            ExprKind::Int(s) => s.replace('_', "").parse::<u32>().ok(),
            _ => None,
        },
        _ => None,
    }
}

fn typed_let_ir_type(t: &TypeExpr) -> Option<IrType> {
    let TypeExpr::Builtin { name, args, .. } = t else {
        return None;
    };
    let width = match args.first() {
        Some(TypeArg::Expr(e)) => match &*e.kind {
            ExprKind::Int(s) => Some(s.replace('_', "").parse::<u32>().ok()?),
            _ => return None,
        },
        Some(_) => return None,
        None => None,
    };
    if width.is_some_and(|w| w == 0) {
        return None;
    }
    match name {
        BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits => Some(IrType::UInt(width)),
        BuiltinTy::SInt | BuiltinTy::SIntCap => Some(IrType::SInt(width)),
        BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => Some(IrType::Bool),
        // `let t : time` → `uint64_t` (v1's `c_type_for(Time)`); the RHS
        // time literal lowers to a bare `Expr::Literal` of its numeric
        // prefix (`100ns` -> 100). (String has no v1-supported local
        // surface — see the `ExprKind::String` arm in lower/exprs.rs —
        // so it is intentionally absent here.)
        BuiltinTy::Time => Some(IrType::UInt(Some(64))),
        _ => None,
    }
}

fn component_method_result_compatible(expected: &IrType, actual: &IrType) -> bool {
    if matches!(expected, IrType::Unknown) || matches!(actual, IrType::Unknown) {
        return true;
    }
    if expected == actual {
        return true;
    }
    match (expected, actual) {
        (IrType::UInt(Some(ew)), IrType::UInt(Some(aw)))
        | (IrType::SInt(Some(ew)), IrType::SInt(Some(aw))) => aw <= ew,
        (IrType::UInt(Some(ew)), IrType::Bool) | (IrType::SInt(Some(ew)), IrType::Bool) => *ew >= 1,
        _ => false,
    }
}

/// The closed `Severity` set (spec §7.7). Shared by `lower_log`'s
/// named-argument gate and its severity guard, so "is this a severity"
/// is answered the same way in both — the gate must not claim a name
/// hides a severity that the guard would have rejected outright.
fn is_log_severity(s: &str) -> bool {
    matches!(s, "debug" | "info" | "warn" | "error" | "fatal")
}

/// A queue method in STATEMENT position that is neither `push` nor
/// `pop` — both of those are lowered above, so this is what is left.
///
/// v1 emits the call against `harc_rt::HarcQueue`, whose whole API is
/// `push`, `pop`, `size` and `empty`. So the split is exact rather than
/// a proxy:
///
///   * `size` / `empty` — compile. The value is discarded, which makes
///     the statement a legal no-op; `discard_queue_query_statement`
///     accepts and elides it before this diagnostic helper is reached.
///   * anything else — `clear`, `front`, a typo — g++: "'struct
///     harc_rt::HarcQueue<long unsigned int>' has no member named
///     'clear'". No backend runs it.
///
/// The emission is verbatim for every name EXCEPT the four width-method
/// intrinsics: `try_emit_width_method` claims `trunc`/`zext`/`sext`/
/// `resize` by name before the member-call path, so `sb.q.trunc(2)`
/// comes out as `((uint64_t)(((uint64_t)(_tb.sb.q) & 0x3ULL)));`, not as
/// a `.trunc(2)` call. The `Invalid` verdict still holds for those four
/// — g++ rejects the cast: "invalid cast from type
/// `harc_rt::HarcQueue<long unsigned int>` to type `uint64_t`" — but it
/// holds for a different reason, so the runtime header is the
/// discriminator for every other name and not for these.
///
/// Measured at ALL FIVE landings independently rather than four inferred
/// from one: testbench-owned field, scoreboard queue, component queue,
/// bare target-state field, and instance-qualified target-state field
/// each take their own probe. All five behave this way — which is why
/// they now share this helper instead of three of them carrying a
/// hand-written `EmitsUncompilable` that measurement contradicts.
fn discard_queue_query_statement(
    what: &str,
    method: &str,
    args: &[CallArg],
) -> Result<bool, LowerError> {
    if !matches!(method, "size" | "empty") {
        return Ok(false);
    }
    if !args.is_empty() {
        return Err(LowerError::Invalid(format!(
            "{what}.{method}() takes no arguments, got {}",
            args.len()
        )));
    }
    // The result is intentionally discarded. An IR statement is not
    // needed for this pure host-state query; omitting it is behaviorally
    // identical to v1's emitted `<queue>.size();` / `.empty();` no-op.
    Ok(true)
}

fn queue_method_in_statement_position(what: &str) -> LowerError {
    LowerError::Invalid(format!(
        "{what} in statement position: `HarcQueue` has only `push`, `pop`, `size` and \
         `empty`"
    ))
}
