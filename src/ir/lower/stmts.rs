//! Statement lowering — one IR form per AST construct (design doc
//! §"Statements within a run / check / transactor body").

use super::{unsupported, FuncBuilder, LowerError};
use crate::ast::{
    BuiltinTy, CallArg, Expr as AstExpr, ExprKind, Ident, Stmt as AstStmt, StmtKind, TypeArg,
    TypeExpr,
};
use crate::ir::{Expr, FileLogLevel, FmtArg, FmtArgs, IrType, LogLevel, Stmt, Terminator};

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
                Err(unsupported(
                    "`return <expr>`",
                    "run/check bodies do not return a value",
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
            StmtKind::Fork(_) => Err(unsupported(
                "block-form `fork ... and ... join`",
                "only RHS-fork TLM (`let x = fork bus.m(...)`) + `join_all` is lowered",
            )),
            StmtKind::Parallel(_) => Err(unsupported("`parallel`", "")),
            StmtKind::Schedule(_) => Err(unsupported("`schedule`", "")),
            StmtKind::Select(_) => Err(unsupported("`select`", "")),
            StmtKind::On(_) => Err(unsupported("`on` handlers", "")),
            StmtKind::Emit { name, args, .. } => self.lower_emit(name, args),
            StmtKind::Yield(e) => self.lower_yield(e),
            StmtKind::Apply(_) => Err(unsupported("`apply`", "")),
            StmtKind::Release(e) => self.lower_release(e),
            StmtKind::Assume(_) => Err(unsupported("`assume`", "")),
            StmtKind::Cover(_) => Err(unsupported("`cover`", "")),
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
                // `sb.expected.push(x)`. `.pop()` in statement position is
                // rejected below (its value must be bound).
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
                            self.push(Stmt::ScoreboardOp {
                                sb,
                                field,
                                op: crate::ir::ScoreboardOp::QueuePush { queue, value },
                                nested_path,
                            });
                            return Ok(());
                        }
                        if method == "pop" {
                            return Err(unsupported(
                                &format!("a discarded `{field}.{queue}.pop()`"),
                                "bind the popped value: `let v = {field}.{queue}.pop()`",
                            ));
                        }
                        return Err(unsupported(
                            &format!(
                                "scoreboard queue method `{field}.{queue}.{method}(...)` in \
                                      statement position"
                            ),
                            "",
                        ));
                    }
                }
                // Statement-position component-queue op:
                // `errors.push(e)` (self) / `checker.sb.errors.push(e)`
                // (path). `.pop()` in statement position is rejected (its
                // value must be bound), mirroring the scoreboard form.
                if let ExprKind::Call { callee, args } = &*e.kind {
                    if let Some((base, queue, method)) = self.as_component_queue_call(callee)? {
                        if method == "push" {
                            let [CallArg::Expr(arg)] = args.as_slice() else {
                                return Err(LowerError::Invalid(format!(
                                    "component `{queue}.push` takes exactly one positional argument"
                                )));
                            };
                            let value = self.lower_expr_no_ports(arg)?;
                            self.push(Stmt::ComponentQueuePush { base, queue, value });
                            return Ok(());
                        }
                        if method == "pop" {
                            return Err(unsupported(
                                &format!("a discarded `{queue}.pop()`"),
                                "bind the popped value: `let v = <recv>.{queue}.pop()`",
                            ));
                        }
                        return Err(unsupported(
                            &format!(
                                "component queue method `{queue}.{method}(...)` in statement \
                                 position"
                            ),
                            "",
                        ));
                    }
                }
                // Statement-position bound-to target-responder queue
                // state-field op: `pending.push(x)` (bare field name).
                // `.pop()` in statement position is rejected (its value
                // must be bound), mirroring the component form.
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
                            self.push(Stmt::TransactorStateQueuePush {
                                instance: String::new(),
                                field,
                                value,
                            });
                            return Ok(());
                        }
                        if method == "pop" {
                            return Err(unsupported(
                                &format!("a discarded target-state `{field}.pop()`"),
                                "bind the popped value: `let v = {field}.pop()`",
                            ));
                        }
                        return Err(unsupported(
                            &format!(
                                "target-state queue method `{field}.{method}(...)` in statement \
                                 position"
                            ),
                            "only `push`/`pop`/`size`/`empty` are lowered",
                        ));
                    }
                }
                // Statement-position TEST-SCOPE target-responder queue
                // state op: `target.pending.push(x)` (fully resolved
                // instance). `.pop()` in statement position is rejected.
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
                                    self.push(Stmt::TransactorStateQueuePush {
                                        instance,
                                        field,
                                        value,
                                    });
                                    return Ok(());
                                }
                                if method == "pop" {
                                    return Err(unsupported(
                                        &format!(
                                            "a discarded target-state `{instance}.{field}.pop()`"
                                        ),
                                        "bind the popped value: `let v = {field}.pop()`",
                                    ));
                                }
                                return Err(unsupported(
                                    &format!(
                                        "target-state queue method `{instance}.{field}.{method}\
                                         (...)` in statement position"
                                    ),
                                    "only `push`/`pop`/`size`/`empty` are lowered",
                                ));
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
                        let lowered = self.lower_component_call_args(args)?;
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

    fn lower_let(&mut self, l: &crate::ast::LetStmt) -> Result<(), LowerError> {
        if !l.probes.is_empty() {
            return Err(unsupported("probe declarations", ""));
        }
        if l.bind {
            return Err(unsupported("`= bind ...` declarations", ""));
        }
        // `let v = <recv>.<queue>.pop()` on a composite-component queue —
        // pop the front into a new local. Checked before the record-typed
        // / scalar let-RHS forms: a record-element pop is `let err :
        // CheckerError = ...pop()` (a record-typed let WITH an initializer,
        // which the record-local block below rejects). The popped local's
        // type comes from the queue element (record id from the field, or a
        // scalar width from the optional `let` annotation).
        if let Some(call) = pop_call_callee(&l.value) {
            if let Some((base, queue, method)) = self.as_component_queue_call(call)? {
                if method == "pop" {
                    let elem = self.component_queue_elem(&base, &queue)?;
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
        if let Some(call) = pop_call_callee(&l.value) {
            if let Some((field, method)) = self.as_state_queue_call(call) {
                if method == "pop" {
                    let crate::ir::StateFieldKind::Queue { elem } =
                        self.target_state_fields[&field].clone()
                    else {
                        // as_state_queue_call already gated on Queue kind.
                        unreachable!("state-queue pop on a non-queue field");
                    };
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
        if let Some(call) = pop_call_callee(&l.value) {
            if let ExprKind::Field { target, name } = &*call.kind {
                if name.name == "pop" {
                    if let Some((instance, field, kind)) = self.as_transactor_state_any(target) {
                        if let crate::ir::StateFieldKind::Queue { elem } = kind {
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
                            let m = comp.method(&method).ok_or_else(|| {
                                unsupported(
                                    &format!("component `{}` has no method `{method}`", comp.name),
                                    "",
                                )
                            })?;
                            if !m.has_ret {
                                return Err(unsupported(
                                    &format!(
                                        "`let {} : {simple} = {}.{method}(...)` — method \
                                         `{method}` returns no value",
                                        l.name.name, comp.name
                                    ),
                                    "",
                                ));
                            }
                            self.check_component_method_result_assignable(
                                m,
                                IrType::Record(rid),
                                &method,
                                &l.name.name,
                            )?;
                            let lowered = self.lower_component_call_args(args)?;
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
                    return Err(unsupported(
                        &format!(
                            "`let {} : {simple} = ...` with a non-`{simple}` initializer",
                            l.name.name
                        ),
                        "transaction locals copy from a same-typed transaction local, or \
                         default-construct and assign fields individually",
                    ));
                }
                let id = self.declare(&l.name.name);
                self.set_local_type(id, IrType::Record(rid));
                self.push(Stmt::RecordInit(id, rid));
                return Ok(());
            }
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
                if let Some(elem) = self.ctx.tseqs.get(&name.name) {
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
                let m = comp.method(&method).ok_or_else(|| {
                    unsupported(
                        &format!("component `{}` has no method `{method}`", comp.name),
                        "",
                    )
                })?;
                if !m.has_ret {
                    return Err(unsupported(
                        &format!(
                            "`let {} = {}.{method}(...)` — method `{method}` returns no value",
                            l.name.name, comp.name
                        ),
                        "",
                    ));
                }
                if let Some(expected) = declared_scalar_ty.clone() {
                    self.check_component_method_result_assignable(
                        m,
                        expected,
                        &method,
                        &l.name.name,
                    )?;
                }
                let lowered = self.lower_component_call_args(args)?;
                let id = self.declare(&l.name.name);
                if let Some(w) = declared_width {
                    self.let_widths.insert(id, w);
                }
                if let Some(ty) = declared_scalar_ty.clone() {
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
        if self.try_lower_tb_method_let(&l.name.name, value)? {
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
            .filter(|t| matches!(t, IrType::Record(_)));
        // Untyped wide-scalar RHS (`let t96 = s128.trunc<96>()`): when the
        // RHS infers to a >64-bit `uint`/`sint`, propagate that width so the
        // local declares as `_harc_u128` (`local_scalar_cty`) rather than
        // silently clamping to `uint64_t`. Mirrors v1's `auto t96 = ...`,
        // whose deduced type is `_harc_u128`. ≤64-bit RHS keeps the existing
        // `uint64_t` default untouched.
        let wide_scalar_ty = self
            .expr_type(&e)
            .filter(|t| matches!(t, IrType::UInt(Some(w)) | IrType::SInt(Some(w)) if *w > 64));
        let id = self.declare(&l.name.name);
        if let Some(w) = declared_width {
            self.let_widths.insert(id, w);
        }
        if let Some(ty) = record_ty.or(declared_scalar_ty).or(wide_scalar_ty) {
            self.set_local_type(id, ty);
        }
        self.push(Stmt::Assign(id, e));
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
            self.push(Stmt::DutWrite(port, e));
            return Ok(());
        }
        // Bus-bound signal write: `axil.aw.valid = 1`.
        if let Some(port) = self.as_bus_port_ref(target)? {
            let e = self.lower_expr(value)?;
            let e = self.hoist_transactor_calls(e);
            self.push(Stmt::DutWrite(port, e));
            return Ok(());
        }
        // Constant-lane DUT port write: `dut.lane_id_in[1] = 9`.
        if let Some(port) = self.as_lane_port_ref(target)? {
            let e = self.lower_expr(value)?;
            let e = self.hoist_transactor_calls(e);
            self.push(Stmt::DutWrite(port, e));
            return Ok(());
        }
        // Scalar testbench field write: `_tb.expected = 3`.
        if let Some(field) = self.as_tb_scalar_field(target) {
            let e = self.lower_expr_no_ports(value)?;
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
            if chain.leaf_vec_len.is_some() {
                return Err(unsupported(
                    &format!(
                        "a whole-`Vec` write of record state field `{}.{}`",
                        chain.field,
                        chain.path.join(".")
                    ),
                    "assign a `Vec` record field element-wise (`{field}.{vec}[i] = ...`)",
                ));
            }
            let e = self.lower_expr_no_ports(value)?;
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
        // Composite-component whole-value copy of a sub-component:
        // `checker.sb = sb` / `responder.model = model`. The LHS terminal
        // field is a `Sub` component field; the RHS is a test-scope
        // component value. Checked before the scalar-field write (whose
        // resolver would reject the non-scalar `Sub` field).
        if self.lower_component_sub_assign(target, value)? {
            return Ok(());
        }
        // Composite-component scalar field write — self-relative inside a
        // method body (`count = ...`) or a dotted path from a test-scope
        // component local (`env.sb.errors = ...`).
        if let Some((base, field)) = self.as_component_field_target(target)? {
            let e = self.lower_expr_no_ports(value)?;
            self.push(Stmt::ComponentFieldWrite {
                base,
                field,
                value: e,
            });
            return Ok(());
        }
        // Scoreboard scalar-counter write: `sb.writes = sb.writes + 1`
        // (classic) / `_tb.sb.writes = ...` (impl-form, post-desugar).
        if let ExprKind::Field { target: ft, name } = &*target.kind {
            if let Some((sb, field, nested_path)) = self.scoreboard_root(ft) {
                let scalar = self.scoreboard_scalar_field(sb, &name.name)?;
                let e = self.lower_expr_no_ports(value)?;
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
                    if self.record_of_local(local).is_some() {
                        return Err(unsupported(
                            &format!(
                                "assignment of a scoreboard `pop()` result to transaction \
                                 local `{}`",
                                id.name
                            ),
                            "",
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
                        if self.record_of_local(local).is_some() {
                            return Err(unsupported(
                                &format!(
                                    "assignment of a transactor method result to \
                                     transaction local `{}`",
                                    id.name
                                ),
                                "",
                            ));
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
                        let m = comp.method(&method).ok_or_else(|| {
                            unsupported(
                                &format!("component `{}` has no method `{method}`", comp.name),
                                "",
                            )
                        })?;
                        if !m.has_ret {
                            return Err(unsupported(
                                &format!(
                                    "assignment from `{}.{method}(...)` — method \
                                     `{method}` returns no value",
                                    comp.name
                                ),
                                "",
                            ));
                        }
                        let expected = self.local_type(local).clone();
                        self.check_component_method_result_assignable(
                            m, expected, &method, &id.name,
                        )?;
                        let lowered = self.lower_component_call_args(args)?;
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
                if let Some(rid) = self.record_of_local(local) {
                    if self.record_id_of_expr(&e) != Some(rid) {
                        return Err(unsupported(
                            &format!(
                                "assignment of a non-`{}` value to transaction local `{}`",
                                self.ctx.records[rid.index()].name, id.name
                            ),
                            "assign fields individually, or copy from a same-typed transaction local",
                        ));
                    }
                }
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
                            return Err(unsupported(
                                &format!(
                                    "a whole-record write of state field `{}` with a \
                                     non-matching RHS",
                                    id.name
                                ),
                                "assign a value of the same record type, or set the record \
                                 fields individually (`{field}.<sub> = ...`)",
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
            return Err(unsupported(
                &format!("assignment to unknown name `{}`", id.name),
                "",
            ));
        }
        // `t.field[i] = value` on a `Vec<T, N>` record field, at any nesting
        // depth (`s.a.b[i] = v`). Resolve the field chain, then lower the
        // index and value into an indexed `RecordFieldWrite`.
        if let ExprKind::Index { target: it, index } = &*target.kind {
            if let Some(chain) = self.try_record_field_chain(it)? {
                if chain.leaf_vec_len.is_none() {
                    return Err(unsupported(
                        &format!("indexing the scalar record field `{}`", chain.dotted),
                        "only `Vec<T, N>` record fields are indexable",
                    ));
                }
                let idx = self.lower_expr_no_ports(index)?;
                let e = self.lower_expr_no_ports(value)?;
                // A `Vec<Record, N>` element store (`tbl.entries[i] = e`)
                // is a C++ struct copy: the RHS must be a whole record
                // value of the MATCHING element type (a same-typed record
                // local, a nested-record field read, or another element
                // read). Reject any other RHS rather than emit
                // `Entry = <scalar>` / a mismatched struct assignment.
                if let IrType::Record(elem_rid) = chain.leaf_ty {
                    if self.record_id_of_expr(&e) != Some(elem_rid) {
                        return Err(unsupported(
                            &format!(
                                "an element write of `Vec` record field `{}` with a \
                                 non-`{}` RHS",
                                chain.dotted, self.ctx.records[elem_rid.index()].name
                            ),
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
                // type/width). We do NOT route a Vec-target RHS through
                // `lower_expr_no_ports`, because the read arm (correctly)
                // rejects every whole-`Vec` field read — including the
                // matching one. Resolve the RHS chain directly here after
                // verifying the shape, and reject any other RHS (scalar,
                // mismatched-shape field, …) with a structured diagnostic so
                // the tbir backend never emits a `std::array = <scalar>`
                // miscompile. (Element writes `rec.data[i] = v` are handled
                // earlier.)
                if let Some(dst_len) = chain.leaf_vec_len {
                    let dst_dotted = chain.dotted.clone();
                    let mismatch = || {
                        unsupported(
                            &format!(
                                "a whole-`Vec` write of record field `{dst_dotted}` \
                                 with a non-matching RHS"
                            ),
                            "assign the field element-wise (`{rec}.{field}[i] = ...`)",
                        )
                    };
                    // RHS must be a whole-`Vec` field read of matching shape.
                    let Some(rhs) = self.try_record_field_chain(value)? else {
                        return Err(mismatch());
                    };
                    if rhs.leaf_vec_len != Some(dst_len) || rhs.leaf_ty != chain.leaf_ty {
                        return Err(mismatch());
                    }
                    self.push(Stmt::RecordFieldWrite {
                        local: chain.local,
                        field: chain.field,
                        path: chain.path,
                        mid_indices: chain.mid_indices,
                        index: None,
                        value: Expr::RecordField {
                            local: rhs.local,
                            field: rhs.field,
                            path: rhs.path,
                            mid_indices: rhs.mid_indices,
                            index: None,
                        },
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
                        return Err(unsupported(
                            &format!(
                                "a whole-record write of field `{}` with a non-matching RHS",
                                chain.dotted
                            ),
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
                // lowered through the normal path.
                let e = self.lower_expr_no_ports(value)?;
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
        Err(unsupported(
            "assignment to a non-port, non-local target",
            "",
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
            return Err(unsupported(
                "`release` of a non-DUT target",
                "`release` applies only to a `probe force` signal on the DUT",
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
                Some(crate::ir::ComponentFieldKind::Sub { component }) => {
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
        if args.len() != m.n_params {
            return Err(LowerError::Invalid(format!(
                "transactor method `{}.{method}` takes {} argument(s), call passes {}",
                schema.name,
                m.n_params,
                args.len()
            )));
        }
        if need_ret && !m.has_ret {
            return Err(LowerError::Invalid(format!(
                "transactor method `{}.{method}` returns no value",
                schema.name
            )));
        }
        let mut lowered = Vec::with_capacity(args.len());
        for a in args {
            let e = match a {
                CallArg::Expr(e) => e,
                CallArg::Named { .. } => {
                    return Err(unsupported(
                        &format!(
                            "named arguments in transactor method call \
                             `{tb_field}.{method}(...)`"
                        ),
                        "",
                    ));
                }
            };
            lowered.push(self.lower_expr_no_ports(e)?);
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
        let Some(&(n_params, has_ret, callee_active_only)) = self.self_transactor_methods.get(name)
        else {
            return Ok(None);
        };
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
        let mut lowered = Vec::with_capacity(args.len());
        for a in args {
            let e = match a {
                CallArg::Expr(e) => e,
                CallArg::Named { .. } => {
                    return Err(unsupported(
                        &format!(
                            "named arguments in transactor sibling method call \
                             `{transactor}.{name}(...)`"
                        ),
                        "",
                    ));
                }
            };
            lowered.push(self.lower_expr_no_ports(e)?);
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
            return Err(unsupported(
                "`randomize` of a non-identifier target",
                "randomize a record-typed local declared with `let t : <Transaction>`",
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
    fn lower_tseq_call(
        &mut self,
        name: &str,
        args: &[crate::ast::CallArg],
    ) -> Result<Expr, LowerError> {
        let mut lowered = Vec::with_capacity(args.len());
        for a in args {
            let crate::ast::CallArg::Expr(e) = a else {
                return Err(unsupported(
                    &format!("named argument in tseq call `{name}`"),
                    "tseq parameters are positional",
                ));
            };
            lowered.push(self.lower_expr_no_ports(e)?);
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
            // Mirrors v1's "`yield` outside a `tseq` body" diagnostic.
            return Err(unsupported(
                "`yield` outside a `tseq` body",
                "yield is only valid inside a `tseq ... end tseq` body",
            ));
        };
        let elem = self
            .seq_of_local(seq)
            .expect("tseq accumulator is always RecordSeq/Seq-typed");
        match elem {
            // Scalar-element sequence: yield any scalar expression.
            IrType::UInt(_) | IrType::SInt(_) | IrType::Bool | IrType::Unknown => {
                let v = self.lower_expr_no_ports(value)?;
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

    fn lower_assert(&mut self, v: &crate::ast::Verify) -> Result<(), LowerError> {
        if v.named.is_some() {
            return Err(unsupported("named property `assert`", ""));
        }
        if v.property_kw {
            return Err(unsupported("`assert property`", ""));
        }
        let Some(expr) = &v.expr else {
            return Err(LowerError::Invalid("assert without expression".to_string()));
        };
        // Ports stay inline (lazy assert eval), but a transactor-method
        // call edge cannot stay nested in the condition — hoist it into a
        // preceding `Stmt::TransactorCall` (the seam rule, and the call may
        // advance simulated time). `(helper.read(0) & 1) == 1`.
        let cond = self.lower_expr(expr)?; // ports allowed in assert conditions
        let cond = self.hoist_transactor_calls(cond);
        let msg = match v.else_fail.as_ref() {
            Some(e) => match &*e.kind {
                ExprKind::String(s) => s.clone(),
                _ => {
                    return Err(unsupported(
                        "non-string-literal `else fail(...)` message",
                        "",
                    ));
                }
            },
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
            _ => Err(unsupported("non-string-literal `fail(...)` message", "")),
        }
    }

    fn lower_log(&mut self, args: &[CallArg], file: Option<String>) -> Result<(), LowerError> {
        // Mirror v1's extraction rules: first bare ident is the
        // severity (default info); first string literal that isn't the
        // logf path is the message.
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
            .find(|s| file.as_deref() != Some(s.as_str()))
            .unwrap_or_default();

        let base = match sev.as_str() {
            "debug" => FileLogLevel::Debug,
            "info" => FileLogLevel::Info,
            "warn" => FileLogLevel::Warn,
            "error" => FileLogLevel::Error,
            "fatal" => FileLogLevel::Fatal,
            other => {
                return Err(unsupported(
                    &format!("log severity `{other}`"),
                    "supported: debug, info, warn, error, fatal",
                ));
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
        name: &str,
        value: &crate::ast::Expr,
    ) -> Result<bool, LowerError> {
        let ExprKind::Call { callee, args } = &*value.kind else {
            return Ok(false);
        };
        let Some(m) = self.tb_method_call_name(callee) else {
            return Ok(false);
        };
        let v = self.lower_tb_method_call(&m, args)?;
        let id = self.declare(name);
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
            let mut parsed = crate::parser::parse_expr_fragment(&c.expr).map_err(|_| {
                unsupported(
                    "string interpolation",
                    format!("`${{{}}}` does not parse as an expression", c.expr),
                )
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
            let call = std::mem::replace(
                e,
                AstExpr::new(ExprKind::Bool(false), e.span),
            );
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
            debug_assert!(lowered, "bus tlm_method call failed to lower after classification");
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
fn pop_call_callee(value: &Option<crate::ast::Expr>) -> Option<&crate::ast::Expr> {
    let v = value.as_ref()?;
    match &*v.kind {
        ExprKind::Call { callee, .. } => Some(callee),
        _ => None,
    }
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
