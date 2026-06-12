//! Control-flow lowering — `if`/`for`/`while`/`repeat`/`loop`/`wait
//! until` into the block shapes specified in docs/tb-ir-design.md
//! §"Control flow".

use super::{FuncBuilder, LoopFrame, LowerError, unsupported};
use crate::ast::{
    Block, Expr as AstExpr, ExprKind, ForStmt, IfStmt, RepeatStmt, WaitTimeout, WaitUntilMode,
};
use crate::ir::{
    BinOp, BlockId, Expr, FmtArg, FmtArgs, IrType, PredSrc, Stmt, Terminator, WaitMode,
};

impl FuncBuilder<'_> {
    pub(crate) fn lower_if(&mut self, i: &IfStmt) -> Result<(), LowerError> {
        let merge = self.new_block();
        self.lower_if_arm(&i.cond, &i.then_block, &i.elsifs, i.else_block.as_ref(), merge)?;
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
        let ExprKind::RangeLit {
            lo: Some(lo),
            hi: Some(hi),
        } = &*f.iter.kind
        else {
            return Err(unsupported(
                "`for x in <sequence>`",
                "only literal ranges `for i in lo .. hi` are lowered",
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

        let cond = Expr::Binary(
            BinOp::Lt,
            Box::new(Expr::Local(var)),
            Box::new(hi_operand),
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
        self.lower_counted_loop(cond, step, &f.body)?;
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
