//! Control-flow lowering — `if`/`for`/`while`/`repeat`/`loop`/`wait
//! until` into the block shapes specified in docs/tb-ir-design.md
//! §"Control flow".

use super::{unsupported, FuncBuilder, LoopFrame, LowerError};
use crate::ast::{
    Block, Expr as AstExpr, ExprKind, ForStmt, IfStmt, RepeatStmt, WaitTimeout, WaitUntilMode,
};
use crate::ir;
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
                if let Some(elem) = self.seq_of_local(seq) {
                    return self.lower_for_in_seq(f, seq, elem);
                }
            }
        }
        let ExprKind::RangeLit {
            lo: Some(lo),
            hi: Some(hi),
        } = &*f.iter.kind
        else {
            return Err(unsupported(
                "`for x in <sequence>`",
                "only literal ranges `for i in lo .. hi` and `for t in <tseq-result>` are lowered",
            ));
        };

        self.push_scope();
        // Loop counter init — evaluated once, outside the loop.
        let lo_ir = self.lower_expr_no_ports(lo)?;
        let var = self.declare(&f.var.name);
        self.push(Stmt::Assign(var, lo_ir));
        // Upper bound — evaluated once; stash non-trivial expressions
        // in a synthesized local so the header re-reads a pure value.
        let hi_ir = self.lower_expr_no_ports(hi)?;
        let hi_operand = self.stash_if_impure(hi_ir);

        let cond = Expr::Binary(BinOp::Lt, Box::new(Expr::Local(var)), Box::new(hi_operand));
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

    /// `for t in <seq>` over a `RecordSeq` local. Lowers to a counted
    /// loop `for (i = 0; i < seq.size(); i++)` whose body first copies
    /// `seq[i]` into the record-typed loop variable `t`, then runs the
    /// user body. The whole-record copy (`Assign(t, SeqIndex{..})`) is
    /// the IR's record-assignment form (v1 binds `t` by `auto&`; a copy
    /// is observably identical for read-only iteration, and the IR has
    /// no by-reference local model).
    fn lower_for_in_seq(
        &mut self,
        f: &ForStmt,
        seq: LocalId,
        elem: ir::RecordId,
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
        // The loop variable is a record local copied from `seq[i]` at the
        // top of each iteration. Declared inside the loop scope so each
        // iteration's read sees the fresh element (and `_` is anonymized).
        let var = self.declare(&f.var.name);
        self.set_local_type(var, IrType::Record(elem));
        let bind = Stmt::Assign(
            var,
            Expr::SeqIndex {
                seq,
                index: Box::new(Expr::Local(counter)),
            },
        );
        self.lower_counted_loop_with_prologue(cond, step, bind, &f.body)?;
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
        self.lower_counted_loop_impl(header_cond, latch_step, None, body)
    }

    /// `lower_counted_loop` with a prologue statement injected at the top
    /// of the body block, before the user statements (used by `for t in
    /// <seq>` to copy `seq[i]` into the loop variable each iteration).
    fn lower_counted_loop_with_prologue(
        &mut self,
        header_cond: Expr,
        latch_step: Stmt,
        prologue: Stmt,
        body: &Block,
    ) -> Result<(), LowerError> {
        self.lower_counted_loop_impl(header_cond, latch_step, Some(prologue), body)
    }

    fn lower_counted_loop_impl(
        &mut self,
        header_cond: Expr,
        latch_step: Stmt,
        prologue: Option<Stmt>,
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

    pub(crate) fn lower_while(&mut self, cond: &AstExpr, body: &Block) -> Result<(), LowerError> {
        let header = self.new_block();
        let body_b = self.new_block();
        let exit = self.new_block();

        self.terminate(Terminator::Jump(header));
        // Condition evaluates in the header so DUT reads re-sample on
        // every iteration.
        self.start_block(header);
        let cond_ir = self.lower_expr_no_ports(cond)?;
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
            // Ports stay inline — the scheduler re-samples the DUT on
            // every cycle inside the predicate closure. The source
            // text rides along for the timeout breakdown, rendered by
            // the same pretty-printer v1's diagnostics use.
            let expr = self.lower_expr(c)?;
            // A transactor method call cannot live in a `wait until`
            // predicate: the scheduler re-evaluates the predicate every
            // cycle, but the call advances simulated time (and may have
            // side effects) — running it per re-evaluation is nonsensical.
            // Reject precisely (hoisting would run it exactly once, the
            // wrong semantics).
            if super::exprs::expr_has_transactor_edge(&expr) {
                return Err(unsupported(
                    "a transactor method call inside a `wait until` predicate",
                    "the predicate is re-evaluated every cycle; hoist the call into a `let` \
                     before the `wait until`",
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

        // Timed waits inside transactor method bodies are out of this
        // slice: methods run synchronously (v1's polling-loop shape,
        // not the coroutine awaiter) and the sync timeout emission is
        // not mirrored yet.
        if self.in_transactor_method {
            return Err(unsupported(
                "`wait until ... timeout` inside a transactor method",
                "use an untimed `wait until`, or a counting loop",
            ));
        }

        // Budget evaluated once, before the wait (v1's `_wu_budget`),
        // so the default timeout header reports the same value the
        // countdown used.
        let cycles_ir = self.lower_expr_no_ports(&to.cycles)?;
        let budget = self.fresh_temp();
        self.push(Stmt::Assign(budget, cycles_ir));

        // Header line — the user's `fail("…")` message, or v1's
        // default "<label> timed out after N cycles".
        let header = match &to.message {
            Some(m) => match &*m.kind {
                ExprKind::String(s) => self.lower_fmt(s)?,
                _ => {
                    return Err(unsupported(
                        "non-string-literal timeout `fail(...)` message",
                        "",
                    ));
                }
            },
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
        for s in &b.stmts {
            self.lower_stmt(s)?;
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
