//! Control-flow lowering — `if`/`for`/`while`/`repeat`/`loop` into the
//! block shapes specified in docs/tb-ir-design.md §"Control flow".

use super::{FuncBuilder, LoopFrame, LowerError, unsupported};
use crate::ast::{Block, Expr as AstExpr, ExprKind, ForStmt, IfStmt, RepeatStmt};
use crate::ir::{BinOp, BlockId, Expr, IrType, Stmt, Terminator};

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
