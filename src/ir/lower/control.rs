//! Control-flow lowering — `if`/`for`/`while`/`repeat`/`loop`/`wait
//! until` into the block shapes specified in docs/tb-ir-design.md
//! §"Control flow".

use super::{not_implemented, unsupported, FuncBuilder, LoopFrame, LowerError, V1Status};
use crate::ast::{
    Block, Expr as AstExpr, ExprKind, ForStmt, IfStmt, RepeatStmt, WaitTimeout, WaitUntilMode,
};
use crate::ir::{
    BinOp, BlockId, Expr, FmtArg, FmtArgs, IrType, LocalId, PredSrc, Stmt, Terminator, WaitMode,
};

impl FuncBuilder<'_> {
    pub(crate) fn lower_if(&mut self, i: &IfStmt) -> Result<(), LowerError> {
        let merge = self.new_block();
        self.lower_if_arm(
            &i.cond,
            &i.then_block,
            &i.elsifs,
            i.else_block.as_ref(),
            merge,
        )?;
        self.start_block(merge);
        Ok(())
    }

    fn lower_if_arm(
        &mut self,
        cond: &AstExpr,
        then_block: &Block,
        elsifs: &[(AstExpr, Block)],
        else_block: Option<&Block>,
        merge: BlockId,
    ) -> Result<(), LowerError> {
        // Condition (DUT reads hoisted into the current block).
        let cond_ir = self.lower_expr_no_ports(cond)?;
        self.validate_truth_expr(&cond_ir, "if/elsif condition")?;
        let then_b = self.new_block();
        let else_b = self.new_block();
        self.terminate(Terminator::Branch(cond_ir, then_b, else_b));

        self.start_block(then_b);
        self.push_scope();
        self.lower_block_stmts(then_block)?;
        self.pop_scope();
        if !self.is_terminated() {
            self.terminate(Terminator::Jump(merge));
        }

        self.start_block(else_b);
        match elsifs.split_first() {
            Some(((c, b), rest)) => {
                self.lower_if_arm(c, b, rest, else_block, merge)?;
            }
            None => {
                if let Some(eb) = else_block {
                    self.push_scope();
                    self.lower_block_stmts(eb)?;
                    self.pop_scope();
                }
                if !self.is_terminated() {
                    self.terminate(Terminator::Jump(merge));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn lower_for(&mut self, f: &ForStmt) -> Result<(), LowerError> {
        // `for t in <seq>` — iterate a transaction-sequence local (the
        // result of a tseq call). Each iteration binds `t` to the i-th
        // record (`seq[i]`) over `0 .. seq.size()`, mirroring v1's
        // `for (auto& t : txns)` range-for.
        if let ExprKind::Ident(id) = &*f.iter.kind {
            if let Some(seq) = self.lookup(&id.name) {
                if let Some(elem_ty) = self.seq_of_local(seq) {
                    return self.lower_for_in_seq(f, seq, elem_ty);
                }
            }
        }
        // `for t in S(...)` — the generator call written inline. v1's
        // `for (auto& t : S())` binds the returned vector to a temporary
        // that lives for the whole loop, so the generator runs ONCE;
        // materializing it into a synthesized local here has the same
        // shape and the same single evaluation.
        if let ExprKind::Call { callee, args } = &*f.iter.kind {
            if let ExprKind::Ident(name) = &*callee.kind {
                if let Some((_, elem, _, _)) = self.ctx.tseqs.get(&name.name).cloned() {
                    let seq_ty = elem.seq_type();
                    let call = self.lower_tseq_call(&name.name, args, f.iter.span)?;
                    let seq = self.fresh_temp();
                    self.set_local_type(seq, seq_ty);
                    self.push(Stmt::Assign(seq, call));
                    let elem_ty = self
                        .seq_of_local(seq)
                        .expect("a tseq result local is seq-typed by construction");
                    return self.lower_for_in_seq(f, seq, elem_ty);
                }
            }
        }
        // `for x in <rec>.<vecfield>` — iterate a fixed-size `Vec<T, N>`
        // record field. v1 emits `for (auto& x : _tb.cur.data)`, which
        // walks the whole `std::array`; the length is a schema constant
        // here, so it lowers to a counted loop over `0 … N-1` whose body
        // binds the loop variable to `<field>[i]`.
        if matches!(&*f.iter.kind, ExprKind::Field { .. }) {
            if let Some(chain) = self.try_record_field_chain(&f.iter)? {
                let Some(n) = chain.leaf_vec_len else {
                    // A scalar leaf is not iterable in either backend:
                    // v1 emits `for (auto& x : _tb.cur.v)` over a
                    // `uint64_t`, which has no `begin`/`end`.
                    return Err(not_implemented(
                        &format!("`for x in {}` over a scalar record field", chain.dotted),
                        "only `Vec<T, N>` record fields are iterable",
                        V1Status::EmitsUncompilable,
                    ));
                };
                return self.lower_for_in_record_vec(f, chain, n);
            }
        }
        let ExprKind::RangeLit {
            lo: Some(lo),
            hi: Some(hi),
        } = &*f.iter.kind
        else {
            // v1 emits a C++ range-for over whatever the sequence
            // expression lowers to: `for (auto& x :
            // harc_rt::harc_read(dut->count_out))`. `harc_read` returns a
            // scalar value with no `begin()`/`end()`, so the translation
            // unit does not compile ("'begin' was not declared in this
            // scope"), verified against the real runtime header.
            return Err(not_implemented(
                "`for x in <sequence>`",
                "only literal ranges `for i in lo .. hi`, `for t in <tseq-result>`, and \
                 `for x in <rec>.<vecfield>` are lowered; v1 emits a C++ range-for over a \
                 value that has no iterator, which does not compile",
                V1Status::EmitsUncompilable,
            ));
        };

        self.push_scope();
        // Loop counter init — evaluated once, outside the loop.
        let lo_ir = self.lower_expr_no_ports(lo)?;
        self.validate_numeric_expr(&lo_ir, "range lower bound")?;
        let var = self.declare(&f.var.name);
        self.push(Stmt::Assign(var, lo_ir));
        // Upper bound — evaluated once; stash non-trivial expressions
        // in a synthesized local so the header re-reads a pure value.
        let hi_ir = self.lower_expr_no_ports(hi)?;
        self.validate_numeric_expr(&hi_ir, "range upper bound")?;
        // The loop counter is a `uint64_t`, so a bound wider than 64
        // bits reaches the header through `HarcWide`'s implicit
        // conversion and the loop runs over its low 64 bits. That is a
        // real sharp edge and worth a diagnostic one day.
        //
        // It is NOT worth a refusal, which is what used to sit here:
        // the grade was `EmitsUncompilable` on the strength of v1
        // failing to build the same program, and refusing it made
        // TB-IR lower LESS than v1 rather than more (harc#662). A
        // `for` loop must not differ from `for i in 0 .. 3` in whether
        // it compiles because someone widened the bound's type.
        let hi_operand = self.stash_if_impure(hi_ir);

        // `for i in lo .. hi` is INCLUSIVE of `hi` (`lo, lo+1, …, hi`),
        // matching ARCH's `lo..hi` range semantics so the two co-authored
        // languages agree on the most basic loop construct. The header
        // therefore tests `i <= hi`, not `i < hi`. (The `for t in <seq>`
        // path below keeps its `i < seq.size()` half-open counter — that
        // iterates the element indices `0 … size-1`, unrelated to the
        // user-facing numeric range.)
        let cond = Expr::Binary(BinOp::Le, Box::new(Expr::Local(var)), Box::new(hi_operand));
        let step = Stmt::Assign(
            var,
            Expr::Binary(
                BinOp::Add,
                Box::new(Expr::Local(var)),
                Box::new(Expr::Literal {
                    value: 1,
                    ty: IrType::Unknown,
                }),
            ),
        );
        self.lower_counted_loop(cond, step, &f.body)?;
        self.pop_scope();
        Ok(())
    }

    /// `for t in <seq>` over a `RecordSeq`/`Seq` local. Lowers to a counted
    /// loop `for (i = 0; i < seq.size(); i++)` whose body first copies
    /// `seq[i]` into the `elem`-typed loop variable `t`, then runs the
    /// user body. The whole-element copy (`Assign(t, SeqIndex{..})`) is
    /// the IR's element-assignment form (v1 binds `t` by `auto&`; a copy
    /// is observably identical for read-only iteration, and the IR has
    /// no by-reference local model). `elem` is the element `IrType`:
    /// `IrType::Record` for a `RecordSeq`, or the boxed scalar for a `Seq`.
    fn lower_for_in_seq(
        &mut self,
        f: &ForStmt,
        seq: LocalId,
        elem: IrType,
    ) -> Result<(), LowerError> {
        self.push_scope();
        // Hidden counter, initialized once outside the loop.
        let counter = self.fresh_temp();
        self.push(Stmt::Assign(
            counter,
            Expr::Literal {
                value: 0,
                ty: IrType::Unknown,
            },
        ));
        let cond = Expr::Binary(
            BinOp::Lt,
            Box::new(Expr::Local(counter)),
            Box::new(Expr::SeqLen(seq)),
        );
        let step = Stmt::Assign(
            counter,
            Expr::Binary(
                BinOp::Add,
                Box::new(Expr::Local(counter)),
                Box::new(Expr::Literal {
                    value: 1,
                    ty: IrType::Unknown,
                }),
            ),
        );
        // The loop variable is an element-typed local copied from `seq[i]`
        // at the top of each iteration. Declared inside the loop scope so
        // each iteration's read sees the fresh element (and `_` is anonymized).
        let var = self.declare(&f.var.name);
        self.set_local_type(var, elem);
        let bind = Stmt::Assign(
            var,
            Expr::SeqIndex {
                seq,
                index: Box::new(Expr::Local(counter)),
            },
        );
        self.lower_counted_loop_with_prologue(cond, step, bind, None, &f.body)?;
        self.pop_scope();
        Ok(())
    }

    /// `for x in <rec>.<vecfield>` over a `Vec<T, N>` record field. The
    /// length is a schema constant, so the header is a plain
    /// `i < N` counter and the body's prologue binds `x` to
    /// `<field>[i]` — the same `Expr::RecordField` an explicit
    /// `<rec>.<vecfield>[i]` read lowers to. Like `for t in <seq>`, the
    /// loop variable is a COPY of the element (the IR has no
    /// by-reference local); `lower_counted_loop_with_prologue` rejects a
    /// write to it rather than dropping one silently.
    ///
    /// Non-leaf element selectors (`t.entries[k].data`) are stashed in
    /// temps BEFORE the loop. v1's range-for evaluates the container
    /// expression once and iterates that, so a selector left inside the
    /// per-iteration bind would (a) re-read `k` every iteration, walking
    /// a different row each time if the body mutates it, and (b) leave a
    /// port read in an `Assign` value, which the verifier rejects as an
    /// internal error rather than a user diagnostic.
    fn lower_for_in_record_vec(
        &mut self,
        f: &ForStmt,
        chain: super::exprs::RecordFieldChain,
        n: usize,
    ) -> Result<(), LowerError> {
        self.push_scope();
        // Snapshot each selector into its own temp, UNCONDITIONALLY: a
        // bare `Expr::Local` is what `stash_if_impure` would leave in
        // place, and a local is exactly what the body can reassign
        // (`for x in t.entries[k].data … k = k + 1`). Copying the value
        // once is what pins the container for the whole loop.
        let mid_indices = chain
            .mid_indices
            .into_iter()
            .map(|(pos, idx)| {
                let idx = self.hoist_ports(idx);
                let t = self.fresh_temp();
                self.push(Stmt::Assign(t, idx));
                (pos, Expr::Local(t))
            })
            .collect::<Vec<_>>();
        let counter = self.fresh_temp();
        self.push(Stmt::Assign(
            counter,
            Expr::Literal {
                value: 0,
                ty: IrType::Unknown,
            },
        ));
        let cond = Expr::Binary(
            BinOp::Lt,
            Box::new(Expr::Local(counter)),
            Box::new(Expr::Literal {
                value: n as u64,
                ty: IrType::Unknown,
            }),
        );
        let step = Stmt::Assign(
            counter,
            Expr::Binary(
                BinOp::Add,
                Box::new(Expr::Local(counter)),
                Box::new(Expr::Literal {
                    value: 1,
                    ty: IrType::Unknown,
                }),
            ),
        );
        let var = self.declare(&f.var.name);
        self.set_local_type(var, chain.leaf_ty);
        let bind = Stmt::Assign(
            var,
            Expr::RecordField {
                local: chain.local,
                field: chain.field,
                path: chain.path,
                mid_indices,
                index: Some(Box::new(Expr::Local(counter))),
            },
        );
        self.lower_counted_loop_with_prologue(cond, step, bind, Some(var), &f.body)?;
        self.pop_scope();
        Ok(())
    }

    pub(crate) fn lower_repeat(&mut self, r: &RepeatStmt) -> Result<(), LowerError> {
        self.push_scope();
        let var = self.fresh_temp();
        self.push(Stmt::Assign(
            var,
            Expr::Literal {
                value: 0,
                ty: IrType::Unknown,
            },
        ));
        let count_ir = self.lower_expr_no_ports(&r.count)?;
        self.validate_numeric_expr(&count_ir, "repeat count")?;
        // Same synthesized `uint64_t` counter as `for`, same silent
        // truncation, same v1 build failure.
        let count_operand = self.stash_if_impure(count_ir);
        let cond = Expr::Binary(
            BinOp::Lt,
            Box::new(Expr::Local(var)),
            Box::new(count_operand),
        );
        let step = Stmt::Assign(
            var,
            Expr::Binary(
                BinOp::Add,
                Box::new(Expr::Local(var)),
                Box::new(Expr::Literal {
                    value: 1,
                    ty: IrType::Unknown,
                }),
            ),
        );
        self.lower_counted_loop(cond, step, &r.body)?;
        self.pop_scope();
        Ok(())
    }

    /// Shared header/body/latch/exit shape for `for` and `repeat`.
    /// Precondition: counter init already emitted in the current block.
    fn lower_counted_loop(
        &mut self,
        header_cond: Expr,
        latch_step: Stmt,
        body: &Block,
    ) -> Result<(), LowerError> {
        self.lower_counted_loop_impl(header_cond, latch_step, None, None, body)
    }

    /// `lower_counted_loop` with a prologue statement injected at the top
    /// of the body block, before the user statements (used by `for t in
    /// <seq>` to copy `seq[i]` into the loop variable each iteration).
    /// `no_write` names a local the body may not assign. It is set only
    /// for the `Vec`-record-field loop: that form is new here, so
    /// rejecting the write costs no working program, and shipping a
    /// SILENT divergence from v1's `auto&` would be worse than
    /// rejecting. `for t in <tseq-result>` deliberately passes `None` —
    /// its by-copy loop variable predates this sweep and `for t in txns
    /// … t.addr = … end for` is an established idiom both backends
    /// accept today; turning it into an error is a regression, and
    /// giving the tseq form real write-back is its own change.
    fn lower_counted_loop_with_prologue(
        &mut self,
        header_cond: Expr,
        latch_step: Stmt,
        prologue: Stmt,
        no_write: Option<LocalId>,
        body: &Block,
    ) -> Result<(), LowerError> {
        self.lower_counted_loop_impl(header_cond, latch_step, Some(prologue), no_write, body)
    }

    fn lower_counted_loop_impl(
        &mut self,
        header_cond: Expr,
        latch_step: Stmt,
        prologue: Option<Stmt>,
        no_write: Option<LocalId>,
        body: &Block,
    ) -> Result<(), LowerError> {
        let header = self.new_block();
        let body_b = self.new_block();
        let latch = self.new_block();
        let exit = self.new_block();

        self.terminate(Terminator::Jump(header));
        self.start_block(header);
        self.terminate(Terminator::Branch(header_cond, body_b, exit));

        self.loop_stack.push(LoopFrame {
            continue_to: latch,
            break_to: exit,
        });
        self.start_block(body_b);
        self.push_scope();
        if let Some(p) = prologue {
            self.push(p);
        }
        self.lower_block_stmts(body)?;
        if let Some(bound) = no_write {
            self.reject_loop_var_write(bound, body_b)?;
        }
        self.pop_scope();
        if !self.is_terminated() {
            self.terminate(Terminator::Jump(latch));
        }
        self.loop_stack.pop();

        self.start_block(latch);
        self.push(latch_step);
        self.terminate(Terminator::Jump(header));

        self.start_block(exit);
        Ok(())
    }

    /// Reject a write to an element-bound loop variable. `body_b` is the
    /// loop body's entry block, whose FIRST statement is the bind this
    /// check must not count; every block from there on belongs to the
    /// body (the latch and exit blocks are still empty at this point,
    /// and any block the body opened was appended after them).
    ///
    /// EVERY statement that names a local destination is checked, not
    /// just `Assign`: `lower_assign` routes `x = sb.q.pop()`,
    /// `x = xact.m(…)` and `x = comp.m(…)` into `ScoreboardOp::QueuePop`
    /// / `TransactorCall` / `ComponentCall` with the loop variable as
    /// their `dest`, so a check that only looked for `Assign` would let
    /// exactly those writes through.
    fn reject_loop_var_write(&self, bound: LocalId, body_b: BlockId) -> Result<(), LowerError> {
        for (bi, block) in self.blocks.iter().enumerate().skip(body_b.0 as usize) {
            for (si, s) in block.stmts.iter().enumerate() {
                if bi == body_b.0 as usize && si == 0 {
                    continue; // the element bind itself
                }
                let writes = match s {
                    Stmt::Assign(l, _)
                    | Stmt::DutRead(l, _)
                    | Stmt::RecordInit(l, _)
                    | Stmt::AggregateInit(l) => *l == bound,
                    Stmt::RecordFieldWrite { local, .. } | Stmt::RecordWriteCb { local, .. } => {
                        *local == bound
                    }
                    Stmt::TbQueuePop { dest, .. }
                    | Stmt::TransactorStateQueuePop { dest, .. }
                    | Stmt::ComponentQueuePop { dest, .. } => *dest == bound,
                    Stmt::TransactorCall { dest, .. }
                    | Stmt::TransactorSelfCall { dest, .. }
                    | Stmt::ComponentCall { dest, .. } => *dest == Some(bound),
                    Stmt::ScoreboardOp {
                        op: crate::ir::ScoreboardOp::QueuePop { dest, .. },
                        ..
                    } => *dest == bound,
                    _ => false,
                };
                if writes {
                    return Err(not_implemented(
                        "a write to a `for` loop's element variable",
                        "the loop variable is a copy of the element, so the write would \
                         not reach the container — index the container directly \
                         (`xs[i] = …` over `for i in 0 .. N-1`)",
                        V1Status::SilentlyMisLowers,
                    ));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn lower_while(&mut self, cond: &AstExpr, body: &Block) -> Result<(), LowerError> {
        let header = self.new_block();
        let body_b = self.new_block();
        let exit = self.new_block();

        self.terminate(Terminator::Jump(header));
        // Condition evaluates in the header so DUT reads re-sample on
        // every iteration.
        self.start_block(header);
        let cond_ir = self.lower_expr_no_ports(cond)?;
        self.validate_truth_expr(&cond_ir, "while condition")?;
        self.terminate(Terminator::Branch(cond_ir, body_b, exit));

        self.loop_stack.push(LoopFrame {
            continue_to: header,
            break_to: exit,
        });
        self.start_block(body_b);
        self.push_scope();
        self.lower_block_stmts(body)?;
        self.pop_scope();
        if !self.is_terminated() {
            self.terminate(Terminator::Jump(header));
        }
        self.loop_stack.pop();

        self.start_block(exit);
        Ok(())
    }

    pub(crate) fn lower_loop(&mut self, body: &Block) -> Result<(), LowerError> {
        let body_b = self.new_block();
        let exit = self.new_block();
        self.terminate(Terminator::Jump(body_b));

        self.loop_stack.push(LoopFrame {
            continue_to: body_b,
            break_to: exit,
        });
        self.start_block(body_b);
        self.push_scope();
        self.lower_block_stmts(body)?;
        self.pop_scope();
        if !self.is_terminated() {
            self.terminate(Terminator::Jump(body_b));
        }
        self.loop_stack.pop();

        // `loop` without a `break` never reaches `exit`; the
        // reachability prune in `finish` removes it.
        self.start_block(exit);
        Ok(())
    }

    /// `wait until …` (spec §7.9) — `Single`/`AllOf`/`AnyOf` forms,
    /// with or without `timeout N cycles fail("…")`. Mirrors v1's
    /// `emit_wait_until` observable contract: on timeout the
    /// diagnostics are the FAIL header (the user's `fail(...)`
    /// message, or "<label> timed out after N cycles") followed by the
    /// per-mode breakdown — one "  not yet true: <src>" line per
    /// still-false sub-predicate for `Single`/`AllOf`, or a single
    /// "  none of: <src1>, <src2>, …" line for `AnyOf` (a timed-out
    /// any-of means NO predicate ever fired, so v1 lists them all
    /// unconditionally). `errors` is bumped exactly once (the bump
    /// rides the terminator's timeout edge — see `codegen::tbir::func`).
    pub(crate) fn lower_wait_until(
        &mut self,
        mode: WaitUntilMode,
        conditions: &[AstExpr],
        timeout: Option<&WaitTimeout>,
    ) -> Result<(), LowerError> {
        let ir_mode = match mode {
            WaitUntilMode::Single => WaitMode::Single,
            WaitUntilMode::AllOf => WaitMode::AllOf,
            WaitUntilMode::AnyOf => WaitMode::AnyOf,
        };
        if conditions.is_empty() {
            return Err(LowerError::Invalid(
                "wait until: at least one condition required".to_string(),
            ));
        }
        let mut preds = Vec::with_capacity(conditions.len());
        for c in conditions {
            let prelude = self.expr_value_prelude_summary(c);
            if prelude.has_inline_statements() {
                return Err(unsupported(
                    "a value call requiring statement materialization inside a `wait until` predicate",
                    "the predicate is re-evaluated every cycle; use an explicit polling loop if a component call, queue pop, or record-valued queue front must run on each attempt",
                ));
            }
            // Ports stay inline — the scheduler re-samples the DUT on
            // every cycle inside the predicate closure. The source
            // text rides along for the timeout breakdown, rendered by
            // the same pretty-printer v1's diagnostics use.
            let previous = self.in_reevaluated_predicate;
            self.in_reevaluated_predicate = true;
            let lowered = self.lower_expr(c);
            self.in_reevaluated_predicate = previous;
            let expr = lowered?;
            self.validate_truth_expr(&expr, "wait-until predicate")?;
            // Synchronous sibling and testbench-instance methods remain inline
            // and are re-evaluated on every predicate attempt, matching v1.
            // Bus/TLM calls still require the statement-level handshake seam.
            if super::exprs::expr_has_bound_transactor_edge(&expr, &|field| {
                self.ctx.bus_bindings.contains_key(field)
            }) {
                return Err(unsupported(
                    "a transactor method call inside a `wait until` predicate",
                    "the predicate is re-evaluated every cycle, so a call that advances time \
                     changes the timing; hoist the call into a `let` before the `wait until`",
                ));
            }
            preds.push(PredSrc {
                expr,
                src_text: crate::codegen::cpp_tb::expr_source_str(c),
            });
        }

        let Some(to) = timeout else {
            let succ = self.new_block();
            self.terminate(Terminator::WaitUntil {
                preds,
                mode: ir_mode,
                succ,
            });
            self.start_block(succ);
            return Ok(());
        };

        // Budget evaluated once, before the wait (v1's `_wu_budget`),
        // so the default timeout header reports the same value the
        // countdown used.
        let cycles_ir = self.lower_expr_no_ports(&to.cycles)?;
        self.validate_numeric_expr(&cycles_ir, "wait-until timeout count")?;
        // `_wu_budget` is an `int64_t`; a wide budget reaches it
        // through `HarcWide`'s two implicit conversions, which tbir
        // resolved silently (keeping the low 64 bits) and g++ called
        // ambiguous under v1. The third synthesized-conversion
        // position, after the `for`/`repeat` counter and the cycle
        // count — an enumeration that said "two" for one round.
        let budget = self.fresh_temp();
        self.push(Stmt::Assign(budget, cycles_ir));

        // Keep the user's header source until the timeout block exists.
        // A statement-level interpolation call must run only after the wait
        // has actually timed out, never while registering the wait.
        let header_msg = match &to.message {
            Some(m) => match &*m.kind {
                ExprKind::String(s) => Some(s.clone()),
                _ => {
                    // v1 DISCARDS the message and substitutes its own
                    // generic one: `sim_log_line("FAIL", "wait until
                    // timed out after %lld cycles", _wu_budget)`. It
                    // compiles and runs, so the failure still fires —
                    // but the diagnostic the user wrote is gone, and
                    // nothing says so.
                    return Err(not_implemented(
                        "non-string-literal timeout `fail(...)` message",
                        "v1 silently replaces it with a generic \"wait until timed out\" line, \
                         so the message written here never appears",
                        V1Status::SilentlyMisLowers,
                    ));
                }
            },
            None => None,
        };

        let on_fire = self.new_block();
        let on_timeout = self.new_block();
        self.terminate(Terminator::WaitUntilTimeout {
            preds: preds.clone(),
            mode: ir_mode,
            cycles: Expr::Local(budget),
            on_fire,
            on_timeout,
        });

        // Timeout arm: header + per-mode breakdown, then rejoin the
        // success path (a timed-out wait fails the test via the error
        // count but does not abort the run — v1 semantics).
        self.start_block(on_timeout);
        let header = match header_msg {
            Some(msg) => self.lower_fmt_hoisting(&msg)?,
            None => {
                let label = match ir_mode {
                    WaitMode::Single => "wait until",
                    WaitMode::AllOf => "wait until all of",
                    WaitMode::AnyOf => "wait until any of",
                };
                FmtArgs {
                    fmt: format!("{label} timed out after %lld cycles"),
                    args: vec![FmtArg {
                        expr: Expr::Local(budget),
                        wide_hex: None,
                    }],
                }
            }
        };
        self.push(Stmt::FailDiag {
            guard: None,
            args: header,
        });
        match ir_mode {
            WaitMode::Single | WaitMode::AllOf => {
                for p in &preds {
                    self.push(Stmt::FailDiag {
                        guard: Some(p.expr.clone()),
                        args: FmtArgs {
                            fmt: format!("  not yet true: {}", p.src_text),
                            args: Vec::new(),
                        },
                    });
                }
            }
            WaitMode::AnyOf => {
                // None became true — list everything that was being
                // waited on, unguarded (v1's single "none of:" line).
                let joined = preds
                    .iter()
                    .map(|p| p.src_text.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                self.push(Stmt::FailDiag {
                    guard: None,
                    args: FmtArgs {
                        fmt: format!("  none of: {joined}"),
                        args: Vec::new(),
                    },
                });
            }
        }
        self.terminate(Terminator::Jump(on_fire));
        self.start_block(on_fire);
        Ok(())
    }

    pub(crate) fn lower_block_stmts(&mut self, b: &Block) -> Result<(), LowerError> {
        for (index, s) in b.stmts.iter().enumerate() {
            let prior = self.current_source_id;
            let source_id = b.stmt_source(index);
            if source_id.is_known() {
                self.current_source_id = source_id;
            }
            let result = self.lower_stmt(s);
            self.current_source_id = prior;
            result?;
        }
        Ok(())
    }

    /// Stash a non-trivial expression into a synthesized local so it is
    /// evaluated exactly once (loop bounds).
    fn stash_if_impure(&mut self, e: Expr) -> Expr {
        match e {
            Expr::Literal { .. } | Expr::Local(_) => e,
            other => {
                let t = self.fresh_temp();
                self.push(Stmt::Assign(t, other));
                Expr::Local(t)
            }
        }
    }
}
