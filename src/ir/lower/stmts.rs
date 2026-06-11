//! Statement lowering — one IR form per AST construct (design doc
//! §"Statements within a run / check / transactor body").

use super::{FuncBuilder, LowerError, unsupported};
use crate::ast::{CallArg, ExprKind, Stmt as AstStmt, StmtKind};
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
                let Some(frame) = self.loop_stack.last() else {
                    return Err(LowerError::Invalid("`break` outside a loop".to_string()));
                };
                self.terminate(Terminator::Jump(frame.break_to));
                Ok(())
            }
            StmtKind::Continue { .. } => {
                let Some(frame) = self.loop_stack.last() else {
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
                if clock.is_some() {
                    return Err(unsupported("`wait N cycles on <clock>`", ""));
                }
                let n = self.lower_expr_no_ports(duration)?;
                let next = self.new_block();
                self.terminate(Terminator::WaitCycles(n, next));
                self.start_block(next);
                Ok(())
            }
            StmtKind::Return(None) => {
                self.terminate(Terminator::Return);
                Ok(())
            }
            StmtKind::Return(Some(_)) => Err(unsupported("`return <expr>`", "")),
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
            // ── Explicit unsupported stubs (MVP) ────────────────────
            StmtKind::WaitUntil { timeout, .. } => Err(unsupported(
                if timeout.is_some() {
                    "`wait until ... timeout`"
                } else {
                    "`wait until`"
                },
                "",
            )),
            StmtKind::After { .. } => Err(unsupported("`after N cycles` blocks", "")),
            StmtKind::Randomize { .. } => Err(unsupported("`randomize`", "")),
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
                let what = match &*e.kind {
                    ExprKind::Call { callee, .. } => match &*callee.kind {
                        ExprKind::Ident(id) => format!("helper call `{}(...)`", id.name),
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
        let Some(value) = &l.value else {
            return Err(unsupported(
                &format!("uninitialized `let {}`", l.name.name),
                "",
            ));
        };
        // Direct DUT-read form: `let x = dut.port` → DutRead(x, port).
        if let Some(port) = self.as_port_ref(value)? {
            let id = self.declare(&l.name.name);
            self.push(Stmt::DutRead(id, port));
            return Ok(());
        }
        let e = self.lower_expr_no_ports(value)?;
        let id = self.declare(&l.name.name);
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
        if let ExprKind::Ident(id) = &*target.kind {
            if let Some(local) = self.lookup(&id.name) {
                let e = self.lower_expr_no_ports(value)?;
                self.push(Stmt::Assign(local, e));
                return Ok(());
            }
            return Err(unsupported(
                &format!("assignment to unknown name `{}`", id.name),
                "",
            ));
        }
        Err(unsupported("assignment to a non-port, non-local target", ""))
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
            "info" => FileLogLevel::Info,
            "warn" => FileLogLevel::Warn,
            "error" => FileLogLevel::Error,
            "fatal" => FileLogLevel::Fatal,
            other => {
                return Err(unsupported(
                    &format!("log severity `{other}`"),
                    "supported: info, warn, error, fatal",
                ));
            }
        };
        let level = match file {
            Some(path) => LogLevel::File { path, level: base },
            None => match base {
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
            let expr = self.lower_expr(&parsed)?; // ports allowed in format args
            args.push(FmtArg {
                expr,
                wide_hex: c.wide_hex,
            });
        }
        Ok(FmtArgs { fmt, args })
    }
}
