//! Statement lowering — one IR form per AST construct (design doc
//! §"Statements within a run / check / transactor body").

use super::{FuncBuilder, LowerError, unsupported};
use crate::ast::{BuiltinTy, CallArg, ExprKind, Stmt as AstStmt, StmtKind, TypeArg, TypeExpr};
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
                    return Err(LowerError::Invalid(
                        "`continue` outside a loop".to_string(),
                    ));
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
                // `eval_clocks_until(now_ps + N)` form (v1's emission
                // for a Time duration). Needs the multi-clock
                // scheduler — a clockless test has no absolute time
                // (v1 emits uncompilable C++ there; the IR rejects).
                if let ExprKind::Time(s) = &*duration.kind {
                    if clock.is_some() {
                        return Err(unsupported(
                            "`wait <time> on <clock>`",
                            "clock-qualified waits take a cycle count",
                        ));
                    }
                    if self.ctx.clock_names.is_empty() {
                        return Err(unsupported(
                            "wall-clock `wait <time>` in a clockless test",
                            "declare a `clock` — absolute time comes from the \
                             multi-clock scheduler",
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
                if self.in_transactor_method && clock.is_some() {
                    return Err(unsupported(
                        "`wait N cycles on <clock>` inside a transactor method",
                        "method bodies run synchronously and know no test clocks",
                    ));
                }
                let clock = match clock {
                    Some(c) => {
                        let Some(index) =
                            self.ctx.clock_names.iter().position(|n| n == &c.name)
                        else {
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
                // Plain waits inside an inlined helper / testbench-
                // method body take v1's synchronous lambda path (no
                // coroutine yield) — see `Terminator::WaitCyclesSync`.
                if clock.is_none() && !self.inline_frames.is_empty() {
                    self.terminate(Terminator::WaitCyclesSync(n, next));
                } else {
                    self.terminate(Terminator::WaitCycles(n, clock, next));
                }
                self.start_block(next);
                Ok(())
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
            // ── Explicit unsupported stubs (MVP) ────────────────────
            StmtKind::After { .. } => Err(unsupported("`after N cycles` blocks", "")),
            StmtKind::Randomize { .. } => Err(unsupported(
                "`randomize`",
                "transaction randomization awaits the constraint-IR seam \
                 (`ConstraintRef` into src/constraints)",
            )),
            StmtKind::Fork(_) | StmtKind::JoinAll { .. } => Err(unsupported("`fork`/`join`", "")),
            StmtKind::Parallel(_) => Err(unsupported("`parallel`", "")),
            StmtKind::Schedule(_) => Err(unsupported("`schedule`", "")),
            StmtKind::Select(_) => Err(unsupported("`select`", "")),
            StmtKind::On(_) => Err(unsupported("`on` handlers", "")),
            StmtKind::Emit { .. } => Err(unsupported("event `emit`", "")),
            StmtKind::Yield(_) => Err(unsupported("`yield`", "")),
            StmtKind::Apply(_) => Err(unsupported("`apply`", "")),
            StmtKind::Release(_) => Err(unsupported("probe `release`", "")),
            StmtKind::Assume(_) => Err(unsupported("`assume`", "")),
            StmtKind::Cover(_) => Err(unsupported("`cover`", "")),
            StmtKind::Expr(e) => {
                // Statement-position bus calls: `mem.poke(a, d)` (call
                // edge, result-less or discarded) and bare
                // `axil.w.send(...)` / `axil.r.recv()` handshakes.
                if self.try_lower_bus_call(e, super::bus::BusCallDest::Discard)? {
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
                    if let Some((sb, field, queue, method)) =
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
                            &format!("scoreboard queue method `{field}.{queue}.{method}(...)` in \
                                      statement position"),
                            "",
                        ));
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

    fn lower_let(&mut self, l: &crate::ast::LetStmt) -> Result<(), LowerError> {
        if !l.probes.is_empty() {
            return Err(unsupported("probe declarations", ""));
        }
        if l.bind {
            return Err(unsupported("`= bind ...` declarations", ""));
        }
        // Record-typed local: `let t : TxnType` default-constructs (v1
        // declares the struct at the let site, so field defaults re-run
        // on every loop iteration — RecordInit mirrors that).
        if let Some(TypeExpr::Named { name, .. }) = l.ty.as_ref() {
            let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
            if let Some(&rid) = self.ctx.record_ids.get(simple) {
                if self.in_pure_helper {
                    return Err(unsupported(
                        &format!("transaction-typed local `let {}` in a pure helper function", l.name.name),
                        "pure helpers emit as scalar-only file-scope functions",
                    ));
                }
                if l.value.is_some() {
                    return Err(unsupported(
                        &format!("`let {} : {simple} = ...` with an initializer", l.name.name),
                        "transaction locals default-construct; assign fields individually",
                    ));
                }
                let id = self.declare(&l.name.name);
                self.set_local_type(id, IrType::Record(rid));
                self.push(Stmt::RecordInit(id, rid));
                return Ok(());
            }
        }
        let Some(value) = &l.value else {
            return Err(unsupported(
                &format!("uninitialized `let {}`", l.name.name),
                "",
            ));
        };
        // Explicit scalar bit-width of the declaration, tracked on
        // every path for the width-method receiver inference (v1's
        // `let_widths` seeds from typed lets regardless of RHS shape).
        let declared_width = l.ty.as_ref().and_then(typed_let_width);
        // Direct DUT-read form: `let x = dut.port` → DutRead(x, port).
        if let Some(port) = self.as_port_ref(value)? {
            let id = self.declare(&l.name.name);
            if let Some(w) = declared_width {
                self.let_widths.insert(id, w);
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
            self.push(Stmt::DutRead(id, port));
            return Ok(());
        }
        // Bus call RHS: `let x = mem.read(a)` (TransactorMethod call
        // edge) or `let x = axil.r.recv()` (CFG-inlined handshake).
        // Checked before transactor fields: the namespaces are
        // disjoint (collision rejected at testbench construction).
        if self.try_lower_bus_call(
            value,
            super::bus::BusCallDest::Declare(&l.name.name),
        )? {
            return Ok(());
        }
        // `let v = sb.q.pop()` — pop the queue front into a new local.
        if let Some((sb, field, queue)) = self.as_scoreboard_pop(value)? {
            let id = self.declare(&l.name.name);
            if let Some(w) = declared_width {
                self.let_widths.insert(id, w);
            }
            self.push(Stmt::ScoreboardOp {
                sb,
                field,
                op: crate::ir::ScoreboardOp::QueuePop { queue, dest: id },
            });
            return Ok(());
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
        // RAL frontdoor register-level read: `let v = regs.NAME`.
        if self.try_lower_regblock_read_let(&l.name.name, value)? {
            return Ok(());
        }
        // Any other regblock-binding access in `let`-RHS position
        // (field-level `regs.REG.FIELD`, an unknown register) is out of
        // subset — reject precisely.
        self.reject_out_of_subset_regblock_access(value, "read")?;
        // Testbench method call RHS: `let x = _tb.m(...)`, CFG-inlined.
        if self.try_lower_tb_method_let(&l.name.name, value)? {
            return Ok(());
        }
        let e = self.lower_expr_no_ports(value)?;
        let id = self.declare(&l.name.name);
        if let Some(w) = declared_width {
            self.let_widths.insert(id, w);
        }
        self.push(Stmt::Assign(id, e));
        Ok(())
    }

    fn lower_assign(
        &mut self,
        target: &crate::ast::Expr,
        value: &crate::ast::Expr,
    ) -> Result<(), LowerError> {
        if let Some(port) = self.as_port_ref(target)? {
            let e = self.lower_expr(value)?; // ports allowed in DutWrite values
            self.push(Stmt::DutWrite(port, e));
            return Ok(());
        }
        // Bus-bound signal write: `axil.aw.valid = 1`.
        if let Some(port) = self.as_bus_port_ref(target)? {
            let e = self.lower_expr(value)?;
            self.push(Stmt::DutWrite(port, e));
            return Ok(());
        }
        // Constant-lane DUT port write: `dut.lane_id_in[1] = 9`.
        if let Some(port) = self.as_lane_port_ref(target)? {
            let e = self.lower_expr(value)?;
            self.push(Stmt::DutWrite(port, e));
            return Ok(());
        }
        // Scalar testbench field write: `_tb.expected = 3`.
        if let Some(field) = self.as_tb_scalar_field(target) {
            let e = self.lower_expr_no_ports(value)?;
            self.push(Stmt::TbFieldWrite { field, value: e });
            return Ok(());
        }
        // Scoreboard scalar-counter write: `sb.writes = sb.writes + 1`
        // (classic) / `_tb.sb.writes = ...` (impl-form, post-desugar).
        if let ExprKind::Field { target: ft, name } = &*target.kind {
            if let Some((sb, field)) = self.scoreboard_root(ft) {
                let scalar = self.scoreboard_scalar_field(sb, &name.name)?;
                let e = self.lower_expr_no_ports(value)?;
                self.push(Stmt::ScoreboardOp {
                    sb,
                    field,
                    op: crate::ir::ScoreboardOp::ScalarWrite { scalar, value: e },
                });
                return Ok(());
            }
        }
        // RAL frontdoor register-level write: `regs.NAME = expr`.
        if self.try_lower_regblock_write(target, value)? {
            return Ok(());
        }
        // A write to a regblock binding that is NOT a known register
        // (`regs.FOO = ...` for an undeclared register, or a field-level
        // `regs.REG.FIELD = ...` access) is out of subset — reject
        // precisely rather than fall through and mis-lower.
        self.reject_out_of_subset_regblock_access(target, "write")?;
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
                if let Some((sb, field, queue)) = self.as_scoreboard_pop(value)? {
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
                let e = self.lower_expr_no_ports(value)?;
                // Whole-record assignment: only a same-typed record
                // local copies (`t = u` — C++ struct assignment in
                // both backends). Anything else would otherwise
                // surface as a verifier TypeMismatch (the internal-
                // bug channel) or a C++ compile error.
                if let Some(rid) = self.record_of_local(local) {
                    let same = matches!(&e, Expr::Local(src)
                        if self.record_of_local(*src) == Some(rid));
                    if !same {
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
        // `t.field = value` on a record-typed local.
        if let ExprKind::Field { target: ft, name } = &*target.kind {
            if let ExprKind::Ident(root) = &*ft.kind {
                if let Some(local) = self.lookup(&root.name) {
                    if let Some(rid) = self.record_of_local(local) {
                        let schema = &self.ctx.records[rid.index()];
                        if schema.field(&name.name).is_none() {
                            return Err(LowerError::Invalid(format!(
                                "transaction `{}` has no field `{}`",
                                schema.name, name.name
                            )));
                        }
                        let e = self.lower_expr_no_ports(value)?;
                        self.push(Stmt::RecordFieldWrite {
                            local,
                            field: name.name.clone(),
                            value: e,
                        });
                        return Ok(());
                    }
                }
            }
        }
        Err(unsupported("assignment to a non-port, non-local target", ""))
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
    ) -> Option<(crate::ir::ScoreboardId, String, String, String)> {
        let ExprKind::Field { target, name: method } = &*callee.kind else {
            return None;
        };
        let ExprKind::Field { target: sb_t, name: queue } = &*target.kind else {
            return None;
        };
        let (sb, field) = self.scoreboard_root(sb_t)?;
        Some((sb, field, queue.name.clone(), method.name.clone()))
    }

    /// Resolve a scoreboard-field access root: `sb` (classic form) or
    /// `_tb.sb` (impl-form, after the desugaring rewrote the field
    /// prefix). Returns `(scoreboard id, field name)` when `sb` is a
    /// scoreboard testbench field of this test.
    pub(crate) fn scoreboard_root(
        &self,
        e: &crate::ast::Expr,
    ) -> Option<(crate::ir::ScoreboardId, String)> {
        match &*e.kind {
            // Impl-form: `_tb.sb`.
            ExprKind::Field { target, name } => {
                let tb_field = self.ctx.tb_field.as_deref()?;
                let ExprKind::Ident(root) = &*target.kind else {
                    return None;
                };
                if root.name != tb_field {
                    return None;
                }
                let &sb = self.ctx.scoreboard_fields.get(&name.name)?;
                Some((sb, name.name.clone()))
            }
            // Classic form (no testbench desugaring): bare `sb`.
            ExprKind::Ident(root) => {
                let &sb = self.ctx.scoreboard_fields.get(&root.name)?;
                Some((sb, root.name.clone()))
            }
            _ => None,
        }
    }

    /// Recognize `sb.<queue>.pop()` as a value expression and validate
    /// it: the field must be a declared `queue<T>` of the scoreboard.
    /// Returns `(sb id, field name, queue field name)`.
    fn as_scoreboard_pop(
        &self,
        e: &crate::ast::Expr,
    ) -> Result<Option<(crate::ir::ScoreboardId, String, String)>, LowerError> {
        let ExprKind::Call { callee, args } = &*e.kind else {
            return Ok(None);
        };
        let Some((sb, field, queue, method)) = self.as_scoreboard_queue_call(callee) else {
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
        Ok(Some((sb, field, queue)))
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
    fn lower_transactor_call(
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
        let ExprKind::Field { target: mid, name: sub } = &*target.kind else {
            return Ok(false);
        };
        let xfield_name = match &*mid.kind {
            ExprKind::Field { target: root_expr, name: xfield } => {
                let ExprKind::Ident(root) = &*root_expr.kind else {
                    return Ok(false);
                };
                if Some(root.name.as_str()) != self.ctx.tb_field.as_deref() {
                    return Ok(false);
                }
                xfield.name.clone()
            }
            ExprKind::Ident(id) if self.ctx.bare_transactor_fields.contains(&id.name) => {
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
                &format!(
                    "assignment to transactor field `{}.{}`",
                    xfield, sub.name
                ),
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
        let cond = self.lower_expr(expr)?; // ports allowed in assert conditions
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
        match &*msg.kind {
            ExprKind::String(s) => self.lower_fmt(s),
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
        let fmt_args = self.lower_fmt(&msg)?;
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
    pub(crate) fn lower_fmt(&mut self, msg: &str) -> Result<FmtArgs, LowerError> {
        let (fmt, caps) = crate::codegen::cpp_tb::process_interp(msg);
        let mut args = Vec::with_capacity(caps.len());
        for c in caps {
            let parsed = crate::parser::parse_expr_fragment(&c.expr).map_err(|_| {
                unsupported(
                    "string interpolation",
                    format!("`${{{}}}` does not parse as an expression", c.expr),
                )
            })?;
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
