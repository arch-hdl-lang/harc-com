//! Expression lowering. Tree-shaped, no flattening; `Expr::Port` nodes
//! survive only in port-allowed positions (wait predicates, format
//! args, DutRead/DutWrite operands, assert conditions) — everywhere
//! else `lower_expr_no_ports` hoists DUT reads into `DutRead` temps.

use super::{FuncBuilder, LowerError, unsupported};
use crate::ast::{BinaryOp, Expr as AstExpr, ExprKind, UnaryOp};
use crate::ir::{BinOp, Expr, IrType, PortAccess, PortRef, Stmt, UnOp};

impl FuncBuilder<'_> {
    /// Lower with `Expr::Port` allowed in the result.
    pub(crate) fn lower_expr(&mut self, e: &AstExpr) -> Result<Expr, LowerError> {
        match &*e.kind {
            ExprKind::Int(s) => {
                let value = parse_int_literal(s).ok_or_else(|| {
                    unsupported("integer literal", format!("`{s}` is not a plain literal"))
                })?;
                Ok(Expr::Literal {
                    value,
                    ty: IrType::Unknown,
                })
            }
            ExprKind::Bool(b) => Ok(Expr::Literal {
                value: *b as u64,
                ty: IrType::Bool,
            }),
            ExprKind::Ident(id) => {
                if let Some(local) = self.lookup(&id.name) {
                    return Ok(Expr::Local(local));
                }
                if self.is_dut_name(&id.name) {
                    return Err(unsupported(
                        "a bare DUT reference",
                        "DUT access must name a port (`dut.<port>`)",
                    ));
                }
                Err(unsupported(
                    &format!("the unresolved name `{}`", id.name),
                    "",
                ))
            }
            ExprKind::Field { target, name } => {
                if let Some(port) = self.as_port_ref(e)? {
                    return Ok(Expr::Port(port));
                }
                if let Some(cov_bin) = self.as_cov_bin(e)? {
                    return Ok(cov_bin);
                }
                // `t.field` read on a record-typed local.
                if let ExprKind::Ident(root) = &*target.kind {
                    if let Some(local) = self.lookup(&root.name) {
                        if let Some(rid) = self.record_of_local(local) {
                            let schema = &self.ctx.records[rid.index()];
                            if schema.field(&name.name).is_none() {
                                return Err(LowerError::Invalid(format!(
                                    "transaction `{}` has no field `{}`",
                                    schema.name, name.name
                                )));
                            }
                            return Ok(Expr::RecordField {
                                local,
                                field: name.name.clone(),
                            });
                        }
                    }
                }
                // Bus-bound signal access (`<bind>.<sig>`, `<bind>.<ch>.<sig>`).
                if let Some(port) = self.as_bus_port_ref(e)? {
                    return Ok(Expr::Port(port));
                }
                Err(unsupported("field access on a non-DUT value", ""))
            }
            ExprKind::Paren(inner) => self.lower_expr(inner),
            ExprKind::Unary { op, expr } => {
                let inner = self.lower_expr(expr)?;
                let op = match op {
                    UnaryOp::Neg => UnOp::Neg,
                    UnaryOp::Not | UnaryOp::NotKw => UnOp::Not,
                    UnaryOp::BitNot => UnOp::BitNot,
                };
                Ok(Expr::Unary(op, Box::new(inner)))
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let ir_op = lower_bin_op(*op)?;
                let l = self.lower_expr(lhs)?;
                let r = self.lower_expr(rhs)?;
                Ok(Expr::Binary(ir_op, Box::new(l), Box::new(r)))
            }
            ExprKind::Ternary { .. } => Err(unsupported("ternary expressions", "")),
            ExprKind::Call { callee, args } => {
                let what = match &*callee.kind {
                    ExprKind::Ident(id) => {
                        if self.helpers.contains(&id.name) {
                            return self.lower_helper_call(&id.name, args);
                        }
                        format!("helper call `{}(...)`", id.name)
                    }
                    ExprKind::Field { name, .. } => {
                        // Bus calls (tlm_method / send / recv) suspend,
                        // so they are statement-level only — `let x =
                        // bus.m(...)` and `x = bus.m(...)` lower via
                        // `try_lower_bus_call`; anything nested deeper
                        // gets this precise rejection.
                        if let Some(bind) = self.bus_call_root(callee) {
                            return Err(unsupported(
                                "bus method calls in expression position",
                                format!(
                                    "only `let x = {bind}.{}(...)` and statement \
                                     position are lowered (v1's surface)",
                                    name.name
                                ),
                            ));
                        }
                        format!("transactor/method call `.{}(...)`", name.name)
                    }
                    _ => "a call expression".to_string(),
                };
                Err(unsupported(&what, ""))
            }
            ExprKind::ForkCall { .. } => Err(unsupported(
                "`fork` bus-method calls",
                "out-of-order TLM issue/join_all lanes are not lowered yet",
            )),
            ExprKind::Randomize { .. } => Err(unsupported("`randomize` expressions", "")),
            ExprKind::Cast { .. } => Err(unsupported("`as` casts", "")),
            ExprKind::Index { .. } => Err(unsupported("index expressions", "")),
            ExprKind::BitSlice { .. } => Err(unsupported("bit-slice expressions", "")),
            ExprKind::String(_) => Err(unsupported(
                "string values in expression position",
                "",
            )),
            ExprKind::Float(_) => Err(unsupported("float literals", "")),
            ExprKind::Time(_) => Err(unsupported("time literals in expression position", "")),
            ExprKind::SystemCall { .. } => Err(unsupported("temporal system calls", "")),
            ExprKind::StructLit { .. } => Err(unsupported("struct literals", "")),
            ExprKind::SetLit(_) => Err(unsupported("set literals", "")),
            ExprKind::DistLit(_) | ExprKind::DistDirective { .. } => {
                Err(unsupported("`dist` constraints", ""))
            }
            ExprKind::RangeLit { .. } => Err(unsupported("range expressions", "")),
            ExprKind::Membership { .. } => Err(unsupported("`in` membership tests", "")),
            ExprKind::ImplicitSelf => Err(unsupported("`.field` shorthand", "")),
            ExprKind::Send { .. } => Err(unsupported("`<-` sends in expression position", "")),
            ExprKind::HashHash { .. } | ExprKind::SeqRepeat { .. } => {
                Err(unsupported("temporal sequence operators", ""))
            }
            ExprKind::NamedArg { .. } => Err(unsupported("named arguments", "")),
            ExprKind::CoverArrow { .. } => Err(unsupported("cover-sequence patterns", "")),
            ExprKind::SolveOrder { .. } => Err(unsupported("`solve_order`", "")),
            ExprKind::ForEachConstraint { .. } => {
                Err(unsupported("constraint `for` comprehensions", ""))
            }
        }
    }

    /// Lower and hoist every surviving `Expr::Port` into a `DutRead`
    /// temp in the current block.
    pub(crate) fn lower_expr_no_ports(&mut self, e: &AstExpr) -> Result<Expr, LowerError> {
        let ir = self.lower_expr(e)?;
        Ok(self.hoist_ports(ir))
    }

    pub(crate) fn hoist_ports(&mut self, e: Expr) -> Expr {
        match e {
            Expr::Port(p) => {
                let t = self.fresh_temp();
                self.push(Stmt::DutRead(t, p));
                Expr::Local(t)
            }
            Expr::Binary(op, a, b) => {
                let a = self.hoist_ports(*a);
                let b = self.hoist_ports(*b);
                Expr::Binary(op, Box::new(a), Box::new(b))
            }
            Expr::Unary(op, a) => {
                let a = self.hoist_ports(*a);
                Expr::Unary(op, Box::new(a))
            }
            Expr::Call(t, args) => {
                let args = args.into_iter().map(|a| self.hoist_ports(a)).collect();
                Expr::Call(t, args)
            }
            other @ (Expr::Literal { .. }
            | Expr::Local(_)
            | Expr::RecordField { .. }
            | Expr::CovBin { .. }) => other,
        }
    }

    /// `Some(PortRef)` when the expression is a dotted access rooted at
    /// the DUT field (`dut.count_out`, `dut.bus.req`). `Err` when it is
    /// rooted at the testbench instance (`_tb.<field>` — post-MVP).
    pub(crate) fn as_port_ref(&self, e: &AstExpr) -> Result<Option<PortRef>, LowerError> {
        let mut segments: Vec<String> = Vec::new();
        let mut cur = e;
        loop {
            match &*cur.kind {
                ExprKind::Field { target, name } => {
                    segments.push(name.name.clone());
                    cur = target;
                }
                ExprKind::Ident(root) => {
                    // The DUT field itself, or — inside an inlined
                    // helper — a parameter bound to the DUT. Either way
                    // the `PortRef` is rooted at the caller's DUT field.
                    if self.is_dut_name(&root.name) {
                        if segments.is_empty() {
                            return Ok(None);
                        }
                        if segments.len() > 1 {
                            // `dut.bus.sig` flattening conventions are
                            // backend-specific (bus binds, Vec<Bus>);
                            // not verified for tbir yet.
                            return Err(unsupported(
                                "nested DUT port paths (`dut.a.b`)",
                                "",
                            ));
                        }
                        segments.reverse();
                        return Ok(Some(PortRef {
                            testbench_field: self.ctx.dut_field.clone(),
                            port_path: segments,
                            direction: None,
                            width: None,
                            access: PortAccess::Port,
                        }));
                    }
                    if Some(root.name.as_str()) == self.ctx.tb_field.as_deref()
                        && !segments.is_empty()
                    {
                        // Covergroup-field paths (`_tb.cov...`) are not
                        // ports — `lower_expr` resolves them as
                        // `Expr::CovBin` via `as_cov_bin`.
                        if self.ctx.cov_fields.contains_key(segments.last().unwrap()) {
                            return Ok(None);
                        }
                        return Err(unsupported(
                            &format!("testbench field access `_tb.{}`", segments.last().unwrap()),
                            "",
                        ));
                    }
                    return Ok(None);
                }
                ExprKind::Paren(inner) => cur = inner,
                _ => return Ok(None),
            }
        }
    }

    /// `Some(Expr::CovBin)` when the expression is a check-phase bin
    /// read on a covergroup-typed testbench field: `_tb.cov.cp_x.yes`
    /// (the impl-for desugaring already rewrote `cov.` → `_tb.cov.`).
    /// Unknown point/bin names are hard errors — v1 would surface them
    /// as C++ compile failures; the IR rejects them at lowering.
    pub(crate) fn as_cov_bin(&self, e: &AstExpr) -> Result<Option<Expr>, LowerError> {
        let Some((field, rest)) = self.as_cov_field_path(e) else {
            return Ok(None);
        };
        let covgroup = self.ctx.cov_fields[&field];
        let schema = &self.ctx.covgroups[covgroup.index()];
        let [point, bin] = rest.as_slice() else {
            return Err(unsupported(
                &format!(
                    "covergroup field access `{field}.{}` (expected `{field}.<point>.<bin>`)",
                    rest.join(".")
                ),
                "",
            ));
        };
        let Some(p) = schema.points.iter().find(|p| p.name == *point) else {
            return Err(LowerError::Invalid(format!(
                "covergroup `{}` has no coverpoint `{point}`",
                schema.name
            )));
        };
        if !p.bins.iter().any(|b| b.name == *bin) {
            return Err(LowerError::Invalid(format!(
                "coverpoint `{}.{point}` has no bin `{bin}`",
                schema.name
            )));
        }
        Ok(Some(Expr::CovBin {
            inst: crate::ir::CovgroupInstance {
                tb_field: field,
                covgroup,
            },
            point: point.clone(),
            bin: bin.clone(),
        }))
    }

    /// Decompose a dotted path rooted at a covergroup-typed testbench
    /// field: `_tb.cov.a.b` → `Some(("cov", ["a", "b"]))`.
    pub(crate) fn as_cov_field_path(&self, e: &AstExpr) -> Option<(String, Vec<String>)> {
        let tb_field = self.ctx.tb_field.as_deref()?;
        let mut segments: Vec<String> = Vec::new();
        let mut cur = e;
        loop {
            match &*cur.kind {
                ExprKind::Field { target, name } => {
                    segments.push(name.name.clone());
                    cur = target;
                }
                ExprKind::Paren(inner) => cur = inner,
                ExprKind::Ident(root) => {
                    if root.name != tb_field {
                        return None;
                    }
                    segments.reverse();
                    let field = segments.first()?.clone();
                    if !self.ctx.cov_fields.contains_key(&field) {
                        return None;
                    }
                    return Some((field, segments[1..].to_vec()));
                }
                _ => return None,
            }
        }
    }
}

fn lower_bin_op(op: BinaryOp) -> Result<BinOp, LowerError> {
    Ok(match op {
        BinaryOp::Add => BinOp::Add,
        BinaryOp::Sub => BinOp::Sub,
        BinaryOp::Mul => BinOp::Mul,
        BinaryOp::Div => BinOp::Div,
        BinaryOp::Mod => BinOp::Mod,
        BinaryOp::Eq => BinOp::Eq,
        BinaryOp::Ne => BinOp::Ne,
        BinaryOp::Lt => BinOp::Lt,
        BinaryOp::Le => BinOp::Le,
        BinaryOp::Gt => BinOp::Gt,
        BinaryOp::Ge => BinOp::Ge,
        BinaryOp::AndAnd | BinaryOp::AndKw => BinOp::And,
        BinaryOp::OrOr | BinaryOp::OrKw => BinOp::Or,
        BinaryOp::BitAnd => BinOp::BitAnd,
        BinaryOp::BitOr => BinOp::BitOr,
        BinaryOp::BitXor => BinOp::BitXor,
        BinaryOp::Shl => BinOp::Shl,
        BinaryOp::Shr => BinOp::Shr,
        BinaryOp::PipeImplies
        | BinaryOp::PipeImpliesNext
        | BinaryOp::Throughout
        | BinaryOp::Within
        | BinaryOp::Intersect => {
            return Err(unsupported("temporal operators", ""));
        }
        BinaryOp::In | BinaryOp::Inside => {
            return Err(unsupported("`in`/`inside` membership operators", ""));
        }
    })
}

/// Parse a plain integer literal (decimal / 0x / 0b / 0o, `_`
/// separators). Verilog-style sized literals are not lowered.
pub(crate) fn parse_int_literal(s: &str) -> Option<u64> {
    let t = s.replace('_', "");
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else if let Some(bin) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        u64::from_str_radix(bin, 2).ok()
    } else if let Some(oct) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
        u64::from_str_radix(oct, 8).ok()
    } else {
        t.parse::<u64>().ok()
    }
}
