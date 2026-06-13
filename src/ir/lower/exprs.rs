//! Expression lowering. Tree-shaped, no flattening; `Expr::Port` nodes
//! survive only in port-allowed positions (wait predicates, format
//! args, DutRead/DutWrite operands, assert conditions) — everywhere
//! else `lower_expr_no_ports` hoists DUT reads into `DutRead` temps.

use super::{FuncBuilder, LowerError, unsupported};
use crate::ast::{
    BinaryOp, BuiltinTy, CallArg, Expr as AstExpr, ExprKind, TypeArg, TypeExpr, UnaryOp,
};
use crate::ir::{BinOp, Expr, IrType, PortAccess, PortRef, Stmt, UnOp, WidthCastKind};

impl FuncBuilder<'_> {
    /// Lower with `Expr::Port` allowed in the result.
    pub(crate) fn lower_expr(&mut self, e: &AstExpr) -> Result<Expr, LowerError> {
        match &*e.kind {
            ExprKind::Int(s) => {
                if let Some(value) = parse_int_literal(s) {
                    return Ok(Expr::Literal {
                        value,
                        ty: IrType::Unknown,
                    });
                }
                // Hex literals wider than 64 bits lower to LSB-first
                // 32-bit word lists (v1's `c_wide_lit_words` shape).
                if let Some(words) = parse_wide_hex_literal(s) {
                    return Ok(Expr::WideLiteral(words));
                }
                Err(unsupported(
                    "integer literal",
                    format!("`{s}` is not a plain literal"),
                ))
            }
            ExprKind::Bool(b) => Ok(Expr::Literal {
                value: *b as u64,
                ty: IrType::Bool,
            }),
            ExprKind::Ident(id) => {
                if let Some(local) = self.lookup(&id.name) {
                    return Ok(Expr::Local(local));
                }
                // The framework cycle counter (`cycle_count`), conventionally
                // referenced from `${cycle_count}` in a watchdog/log
                // diagnostic. A local of the same name shadows it (checked
                // above). v1 emits the in-scope `cycle_count` variable.
                if id.name == "cycle_count" {
                    return Ok(Expr::CycleCount);
                }
                // Persistent state field of a bound-to target responder
                // body — a bare ident (locals shadow, checked above).
                // `instance` is a placeholder; the test-binding stage
                // fills it once the passive instance is resolved.
                if self.target_state_fields.contains(&id.name) {
                    return Ok(Expr::TransactorState {
                        instance: String::new(),
                        field: id.name.clone(),
                    });
                }
                if self.is_dut_name(&id.name) {
                    return Err(unsupported(
                        "a bare DUT reference",
                        "DUT access must name a port (`dut.<port>`)",
                    ));
                }
                // File-scope `const` / enum-variant substitution
                // (locals shadow — checked above; v1's constexpr /
                // variant-index emission is value-identical).
                if let Some(v) = self.ctx.consts.get(&id.name) {
                    return Ok(Expr::Literal {
                        value: *v,
                        ty: IrType::Unknown,
                    });
                }
                // Self-relative component field read inside a method body
                // (`count` → `self.count`). Locals shadow (checked above).
                if let Some(ce) = self.as_component_field_read(e)? {
                    return Ok(ce);
                }
                if self.in_check && self.ctx.test_scope_lets.contains(&id.name) {
                    return Err(unsupported(
                        &format!("test-scope `let {}` referenced in the check phase", id.name),
                        "test-scope lets lower as run-function locals; run and check are \
                         separate functions in the IR, so v1's shared-capture scoping is \
                         not representable",
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
                // Scalar testbench field read (`_tb.expected`).
                if let Some(field) = self.as_tb_scalar_field(e) {
                    return Ok(Expr::TbField(field));
                }
                // Test-scope read of a bound-to target responder's
                // persistent state (`target.read_count`).
                if let Some((instance, field)) = self.as_transactor_state(e) {
                    return Ok(Expr::TransactorState { instance, field });
                }
                // Scoreboard scalar-counter read (`sb.writes` /
                // `_tb.sb.writes` after impl-form desugaring).
                if let Some((sb, field)) = self.scoreboard_root(target) {
                    let scalar = self.scoreboard_scalar_field(sb, &name.name)?;
                    return Ok(Expr::ScoreboardQuery {
                        sb,
                        field,
                        query: crate::ir::ScoreboardQuery::Scalar { scalar },
                    });
                }
                // Regblock-binding access in expression position. The
                // mirror IS a record local, so `regs.NAME` would
                // otherwise fall into the record-field path below and
                // silently read the mirror — but a RW/RO register read
                // must go to the bus (v1's frontdoor + read-predict).
                // Register reads are only lowered in `let`-RHS position
                // (`let v = regs.NAME`), so any register read reaching
                // here sits in a value position the IR can't represent
                // without a hoist that changes the bus-read count.
                if let Some((binding, reg)) = self.as_regblock_register(e) {
                    return Err(unsupported(
                        &format!(
                            "register read `{binding}.{reg}` outside a `let` binding"
                        ),
                        "v1 reads the bus inline (and predicts the mirror) at every read \
                         site; the IR lowers register reads only in `let x = regs.NAME` \
                         position — hoist the read into a `let` first",
                    ));
                }
                self.reject_out_of_subset_regblock_access(e, "read")?;
                // Composite-component scalar field read via a test-scope
                // path (`env.sb.count`).
                if let Some(ce) = self.as_component_field_read(e)? {
                    return Ok(ce);
                }
                // `r.field` read on a `recv()`-captured payload local
                // (`let r = bus.<ch>.recv(); ... r.data`). Each payload
                // signal was captured into its own local at recv time;
                // resolve the named field to that local. v1 reads the
                // field off the captured payload struct.
                if let ExprKind::Ident(root) = &*target.kind {
                    if let Some(local) = self.lookup(&root.name) {
                        if let Some(fields) = self.recv_payloads.get(&local) {
                            return match fields.iter().find(|(f, _)| f == &name.name) {
                                Some((_, fid)) => Ok(Expr::Local(*fid)),
                                None => Err(LowerError::Invalid(format!(
                                    "recv payload `{}` has no field `{}` (valid: {})",
                                    root.name,
                                    name.name,
                                    fields
                                        .iter()
                                        .map(|(f, _)| f.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ))),
                            };
                        }
                    }
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
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => {
                // Lowered to the IR ternary, emitted as the C++ `?:`
                // operator — the not-taken arm stays lazily skipped,
                // exactly v1's emission. (Port reads hoisted out of a
                // ternary by `lower_expr_no_ports` become eager, but a
                // DUT port read is side-effect-free and untraced, so
                // the difference is unobservable.)
                let c = self.lower_expr(cond)?;
                let t = self.lower_expr(then_branch)?;
                let e = self.lower_expr(else_branch)?;
                Ok(Expr::Ternary(Box::new(c), Box::new(t), Box::new(e)))
            }
            ExprKind::Call { callee, args } => {
                let what = match &*callee.kind {
                    ExprKind::Ident(id) => {
                        if self.helpers.contains(&id.name) {
                            return self.lower_helper_call(&id.name, args);
                        }
                        format!("helper call `{}(...)`", id.name)
                    }
                    ExprKind::Field { target, name } => {
                        // Width-method intrinsics: `.trunc<N>()` /
                        // `.zext<N>()` / `.sext<N>()` / `.resize<N>()`.
                        if let Some(kind) = width_cast_kind(&name.name) {
                            return self.lower_width_method(kind, &name.name, target, args);
                        }
                        // Component heartbeat-idle predicates:
                        // `agent.idle_in(N)`, `.idle_out(N)`, `.idle(N)`.
                        if let Some(idle) = self.as_component_idle(callee, args)? {
                            return Ok(idle);
                        }
                        // Scoreboard queue value-queries: `sb.q.size()`,
                        // `sb.q.empty()`. (`sb.q.pop()` mutates and is
                        // lowered only as a statement — reaching it here
                        // means it was used in a deeper expression
                        // position, which is rejected below.)
                        if let Some(q) = self.lower_scoreboard_query_call(callee, args)? {
                            return Ok(q);
                        }
                        // Testbench helper method call (`_tb.reset()`),
                        // CFG-inlined like an impure helper.
                        if let Some(m) = self.tb_method_call_name(callee) {
                            return self.lower_tb_method_call(&m, args);
                        }
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
                        // Transactor method calls are call EDGES, never
                        // expression values — they may advance simulated
                        // time, which an expression position cannot
                        // represent (statement order is the contract).
                        if self.as_transactor_call(callee)?.is_some() {
                            if self.in_fmt_args {
                                return Err(unsupported(
                                    &format!(
                                        "transactor method call `.{}(...)` inside a message",
                                        name.name
                                    ),
                                    "log/fail messages evaluate lazily; hoist the call into \
                                     a `let` first",
                                ));
                            }
                            return Err(unsupported(
                                &format!(
                                    "transactor method call `.{}(...)` in expression position",
                                    name.name
                                ),
                                "hoist it into a `let` first",
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
            ExprKind::Cast { expr, ty } => {
                // `e as uint<W>` / `as sint<W>` / `as bits<W>` (W ≤ 64)
                // is a width relabel: v1 emits `((uint64_t)(e))` (the
                // C type for every width ≤ 64 is the same 64-bit
                // integer), so the value is unchanged in the IR's
                // uint64 local model. The annotation still feeds the
                // width-method receiver inference (done on the AST at
                // the call site). Anything else stays rejected.
                if cast_relabel_width(ty).is_some() {
                    return self.lower_expr(expr);
                }
                Err(unsupported(
                    "`as` casts outside scalar uint/sint/bits (≤ 64 bits)",
                    "",
                ))
            }
            ExprKind::Index { .. } => {
                // Constant-lane DUT port access: `dut.<port>[<const>]`.
                if let Some(port) = self.as_lane_port_ref(e)? {
                    return Ok(Expr::Port(port));
                }
                Err(unsupported(
                    "index expressions",
                    "only `dut.<port>[<constant>]` lane accesses are lowered",
                ))
            }
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
            Expr::Ternary(c, t, e) => {
                let c = self.hoist_ports(*c);
                let t = self.hoist_ports(*t);
                let e = self.hoist_ports(*e);
                Expr::Ternary(Box::new(c), Box::new(t), Box::new(e))
            }
            Expr::WidthCast {
                kind,
                width,
                src_width,
                inner,
            } => {
                let inner = self.hoist_ports(*inner);
                Expr::WidthCast {
                    kind,
                    width,
                    src_width,
                    inner: Box::new(inner),
                }
            }
            Expr::Call(t, args) => {
                let args = args.into_iter().map(|a| self.hoist_ports(a)).collect();
                Expr::Call(t, args)
            }
            Expr::ComponentIdle { base, kind, n } => {
                let n = self.hoist_ports(*n);
                Expr::ComponentIdle {
                    base,
                    kind,
                    n: Box::new(n),
                }
            }
            Expr::SeqIndex { seq, index } => {
                let index = self.hoist_ports(*index);
                Expr::SeqIndex {
                    seq,
                    index: Box::new(index),
                }
            }
            other @ (Expr::Literal { .. }
            | Expr::WideLiteral(_)
            | Expr::Local(_)
            // The global cycle counter — a framework value, no DUT port.
            | Expr::CycleCount
            | Expr::RecordField { .. }
            | Expr::TbField(_)
            // Transactor-instance state is host state — no DUT port inside.
            | Expr::TransactorState { .. }
            // Scoreboard reads are host state — no DUT port inside.
            | Expr::ScoreboardQuery { .. }
            // Component fields are host state — no DUT port inside.
            | Expr::ComponentField { .. }
            // Sequence length is host state — no DUT port inside.
            | Expr::SeqLen(_)
            | Expr::CovBin { .. }) => other,
        }
    }

    /// Lower `sb.<queue>.size()` / `sb.<queue>.empty()` into an
    /// `Expr::ScoreboardQuery`, or `None` when `callee` is not a
    /// scoreboard queue method access. A `pop()` reaching here (deeper
    /// than a `let`/assign RHS) is rejected — it mutates and must be a
    /// statement.
    fn lower_scoreboard_query_call(
        &self,
        callee: &AstExpr,
        args: &[crate::ast::CallArg],
    ) -> Result<Option<Expr>, LowerError> {
        let Some((sb, field, queue, method)) = self.as_scoreboard_queue_call(callee) else {
            return Ok(None);
        };
        let query = match method.as_str() {
            "size" => crate::ir::ScoreboardQuery::QueueSize {
                queue: queue.clone(),
            },
            "empty" => crate::ir::ScoreboardQuery::QueueEmpty {
                queue: queue.clone(),
            },
            "pop" => {
                return Err(unsupported(
                    &format!("scoreboard `{field}.{queue}.pop()` in a nested expression"),
                    "bind it to its own `let` first — `pop` mutates the queue",
                ));
            }
            other => {
                return Err(unsupported(
                    &format!("scoreboard queue method `{field}.{queue}.{other}(...)`"),
                    "only `push`/`pop`/`size`/`empty` are lowered",
                ));
            }
        };
        if !args.is_empty() {
            return Err(LowerError::Invalid(format!(
                "scoreboard `{field}.{queue}.{method}()` takes no arguments"
            )));
        }
        self.scoreboard_queue_field(sb, &queue)?;
        Ok(Some(Expr::ScoreboardQuery { sb, field, query }))
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
                    // A declared local SHADOWS the DUT name (a method
                    // param or `let` named like the DUT field is host
                    // state, not the DUT — v1 surfaces such shadowing
                    // as a C++ compile error; without this guard the
                    // access would silently mis-lower to a DutWrite/
                    // DutRead). DUT-bound inline-helper params are not
                    // declared as locals, so they pass through.
                    if self.lookup(&root.name).is_none() && self.is_dut_name(&root.name) {
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
                            lane: None,
                        }));
                    }
                    if Some(root.name.as_str()) == self.ctx.tb_field.as_deref()
                        && !segments.is_empty()
                    {
                        // Covergroup-field paths (`_tb.cov...`) and
                        // scalar-field paths (`_tb.expected`) are not
                        // ports — `lower_expr` resolves them as
                        // `Expr::CovBin` via `as_cov_bin`; `Expr::TbField`
                        // via the testbench-field path. Transactor-
                        // field paths (`_tb.xact...`) are call/bind
                        // surfaces handled by their statement forms.
                        if self.ctx.cov_fields.contains_key(segments.last().unwrap())
                            || self
                                .ctx
                                .transactor_fields
                                .contains_key(segments.last().unwrap())
                        {
                            return Ok(None);
                        }
                        // Scoreboard-field paths (`_tb.sb`, `_tb.sb.q`,
                        // `_tb.sb.q.push`) are host state, not ports —
                        // `lower_expr` / `lower_assign` resolve them via
                        // the scoreboard op/query forms. The root field
                        // (the segment after `_tb`) is the scoreboard
                        // instance name.
                        if self.ctx.scoreboard_fields.contains_key(segments.last().unwrap()) {
                            return Ok(None);
                        }
                        // Composite-component field paths (`_tb.prod`,
                        // `_tb.prod.seen`, `_tb.top.prod`) are host
                        // instances, not ports — `lower_expr` /
                        // `lower_assign` resolve them via the component
                        // field/method/idle/emit forms. `segments` is in
                        // reverse path order (innermost first), so the
                        // segment right after `_tb` — the component
                        // instance name — is `segments.last()`.
                        if self
                            .ctx
                            .component_fields
                            .contains_key(segments.last().unwrap())
                        {
                            return Ok(None);
                        }
                        if segments.len() == 1
                            && self.ctx.tb_scalar_fields.contains(&segments[0])
                        {
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

    /// `Some((tb_field, transactor, method))` when `callee` is a
    /// method access on a transactor-typed testbench field:
    /// `_tb.xact.write1` (the impl-for desugaring already rewrote
    /// `xact.` → `_tb.xact.`). An access to a method the transactor
    /// does not declare is a hard error — v1 would surface it as a
    /// C++ compile failure; the IR rejects it at lowering.
    pub(crate) fn as_transactor_call(
        &self,
        callee: &AstExpr,
    ) -> Result<Option<(String, crate::ir::TransactorId, String)>, LowerError> {
        let ExprKind::Field {
            target,
            name: method,
        } = &*callee.kind
        else {
            return Ok(None);
        };
        // Two access shapes resolve to a transactor field:
        //   `_tb.<field>.<method>` — testbench-field instance (the
        //     impl-for desugaring rewrote `xact.` → `_tb.xact.`).
        //   `<field>.<method>`     — test-scope-let instance, accessed
        //     by its bare name (left unqualified by the desugaring).
        let field_name = match &*target.kind {
            ExprKind::Field {
                target: root_expr,
                name: field,
            } => {
                let ExprKind::Ident(root) = &*root_expr.kind else {
                    return Ok(None);
                };
                if Some(root.name.as_str()) != self.ctx.tb_field.as_deref() {
                    return Ok(None);
                }
                field.name.clone()
            }
            ExprKind::Ident(id) if self.ctx.bare_transactor_fields.contains(&id.name) => {
                id.name.clone()
            }
            _ => return Ok(None),
        };
        let Some(&xid) = self.ctx.transactor_fields.get(&field_name) else {
            return Ok(None);
        };
        let schema = &self.ctx.transactors[xid.index()];
        if schema.method(&method.name).is_none() {
            return Err(LowerError::Invalid(format!(
                "transactor `{}` has no method `{}`",
                schema.name, method.name
            )));
        }
        Ok(Some((field_name, xid, method.name.clone())))
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
    /// `Some(field)` when the expression is a one-segment access to a
    /// scalar testbench field: `_tb.expected`.
    pub(crate) fn as_tb_scalar_field(&self, e: &AstExpr) -> Option<String> {
        let tb_field = self.ctx.tb_field.as_deref()?;
        let ExprKind::Field { target, name } = &*e.kind else {
            return None;
        };
        let ExprKind::Ident(root) = &*target.kind else {
            return None;
        };
        (root.name == tb_field && self.ctx.tb_scalar_fields.contains(&name.name))
            .then(|| name.name.clone())
    }

    /// `Some((instance, field))` when the expression is a test-scope
    /// access to a bound-to target responder's persistent state field:
    /// `target.read_count`. The instance is a passive responder bound
    /// in this test; the field must be one of its declared state fields
    /// (an unknown field is a hard error, surfaced precisely). Returns
    /// `None` for any non-matching shape so the caller falls through.
    pub(crate) fn as_transactor_state(&self, e: &AstExpr) -> Option<(String, String)> {
        let ExprKind::Field { target, name } = &*e.kind else {
            return None;
        };
        let ExprKind::Ident(root) = &*target.kind else {
            return None;
        };
        let fields = self.ctx.target_state.get(&root.name)?;
        fields
            .contains(&name.name)
            .then(|| (root.name.clone(), name.name.clone()))
    }

    /// `Some(PortRef)` (with `lane`) when the expression is a
    /// constant-index lane access on a direct DUT port:
    /// `dut.<port>[<const>]`. The index must reduce to an integer
    /// literal (directly, through parens, or via a `const`/enum name)
    /// — non-constant lane indices are an explicit rejection at the
    /// caller.
    pub(crate) fn as_lane_port_ref(
        &mut self,
        e: &AstExpr,
    ) -> Result<Option<PortRef>, LowerError> {
        let ExprKind::Index { target, index } = &*e.kind else {
            return Ok(None);
        };
        let Some(mut port) = self.as_port_ref(target)? else {
            return Ok(None);
        };
        let Some(lane) = self.const_eval_index(index) else {
            return Err(unsupported(
                "a non-constant lane index on a DUT port",
                "only `dut.<port>[<integer constant>]` is lowered",
            ));
        };
        port.lane = Some(lane);
        Ok(Some(port))
    }

    /// Constant-evaluate a lane index: integer literal, parenthesized
    /// literal, or a `const`/enum-variant name.
    fn const_eval_index(&self, e: &AstExpr) -> Option<u64> {
        match &*e.kind {
            ExprKind::Int(s) => parse_int_literal(s),
            ExprKind::Paren(inner) => self.const_eval_index(inner),
            ExprKind::Ident(id) if self.lookup(&id.name).is_none() => {
                self.ctx.consts.get(&id.name).copied()
            }
            _ => None,
        }
    }

    /// Lower a width-method intrinsic call (`recv.trunc<N>()`, ...).
    /// Mirrors v1's `try_emit_width_method`: constant width required,
    /// zero-width rejected, direction checked against the best-effort
    /// receiver width, ≤ 64-bit subset only (wide receivers are not in
    /// the IR's expression model yet).
    fn lower_width_method(
        &mut self,
        kind: WidthCastKind,
        kind_name: &str,
        target: &AstExpr,
        args: &[CallArg],
    ) -> Result<Expr, LowerError> {
        let width_expr = match args.first() {
            Some(CallArg::Expr(e)) if args.len() == 1 => e,
            _ => {
                return Err(LowerError::Invalid(format!(
                    "`.{kind_name}<N>()` requires a constant width argument"
                )));
            }
        };
        let Some(width) = const_eval_width(width_expr) else {
            return Err(LowerError::Invalid(format!(
                "`.{kind_name}<N>()` requires a constant integer width"
            )));
        };
        if width == 0 {
            return Err(LowerError::Invalid(format!(
                "`.{kind_name}<{width}>()`: width must be greater than zero"
            )));
        }
        if width > 64 {
            return Err(unsupported(
                &format!("`.{kind_name}<{width}>()` with a width above 64 bits"),
                "the TB-IR expression model is 64-bit",
            ));
        }
        // Best-effort receiver-width inference (v1's
        // `infer_expr_width_best_effort`) for the direction check and
        // the sext shift-fill shape.
        let src_width = self.infer_expr_width(target);
        if let Some(sw) = src_width {
            match kind {
                WidthCastKind::Trunc if width >= sw => {
                    return Err(LowerError::Invalid(format!(
                        "`.trunc<{width}>()` on a {sw}-bit value: width must be strictly \
                         less than the source width (otherwise it's a no-op or \
                         wrong-direction). Use `.zext<{width}>()` to widen, or remove \
                         the cast if you meant a no-op."
                    )));
                }
                WidthCastKind::Zext | WidthCastKind::Sext if width < sw => {
                    return Err(LowerError::Invalid(format!(
                        "`.{kind_name}<{width}>()` on a {sw}-bit value: width must be \
                         ≥ the source width (otherwise it narrows, wrong direction). \
                         Use `.trunc<{width}>()` to narrow."
                    )));
                }
                _ => {}
            }
        }
        let inner = self.lower_expr(target)?;
        Ok(Expr::WidthCast {
            kind,
            width,
            src_width,
            inner: Box::new(inner),
        })
    }

    /// Best-effort receiver bit-width (v1's
    /// `infer_expr_width_best_effort`): parens recurse, `as uint<W>`
    /// casts give W, nested width methods give their target width,
    /// bare literals give their minimum unsigned width, and locals
    /// resolve through the typed-`let` width table.
    fn infer_expr_width(&self, e: &AstExpr) -> Option<u32> {
        match &*e.kind {
            ExprKind::Paren(inner) => self.infer_expr_width(inner),
            ExprKind::Cast { ty, .. } => cast_relabel_width(ty),
            ExprKind::Call { callee, args } => {
                if let ExprKind::Field { name, .. } = &*callee.kind {
                    if width_cast_kind(&name.name).is_some() {
                        if let Some(CallArg::Expr(w)) = args.first() {
                            return const_eval_width(w);
                        }
                    }
                }
                None
            }
            ExprKind::Int(s) => {
                let v = parse_int_literal(s)?;
                Some(if v == 0 { 1 } else { 64 - v.leading_zeros() })
            }
            ExprKind::Ident(id) => {
                let local = self.lookup(&id.name)?;
                self.let_widths.get(&local).copied()
            }
            _ => None,
        }
    }
}

/// Width-method name → `WidthCastKind`.
pub(crate) fn width_cast_kind(name: &str) -> Option<WidthCastKind> {
    match name {
        "trunc" => Some(WidthCastKind::Trunc),
        "zext" => Some(WidthCastKind::Zext),
        "sext" => Some(WidthCastKind::Sext),
        "resize" => Some(WidthCastKind::Resize),
        _ => None,
    }
}

/// `Some(W)` when the cast target is a scalar `uint<W>`/`sint<W>`/
/// `bits<W>` relabel with W ≤ 64 (v1 lowers these to a same-storage
/// C cast — value-identity in the IR's uint64 model). Width-less
/// scalar casts give 64.
pub(crate) fn cast_relabel_width(ty: &TypeExpr) -> Option<u32> {
    let TypeExpr::Builtin { name, args, .. } = ty else {
        return None;
    };
    if !matches!(
        name,
        BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::SInt | BuiltinTy::SIntCap | BuiltinTy::Bits
    ) {
        return None;
    }
    let width = match args.first() {
        Some(TypeArg::Expr(e)) => match &*e.kind {
            ExprKind::Int(s) => s.replace('_', "").parse::<u32>().ok()?,
            _ => return None,
        },
        Some(_) => return None,
        None => 64,
    };
    (width > 0 && width <= 64).then_some(width)
}

/// Constant width argument of a width method (v1's `eval_const_width`:
/// integer literal, possibly parenthesized).
fn const_eval_width(e: &AstExpr) -> Option<u32> {
    match &*e.kind {
        ExprKind::Paren(inner) => const_eval_width(inner),
        ExprKind::Int(s) => parse_int_literal(s).and_then(|v| u32::try_from(v).ok()),
        _ => None,
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

/// Parse a hex literal wider than 64 bits (> 16 hex digits) into
/// LSB-first 32-bit words — v1's `c_wide_lit_words` decomposition,
/// extended down to the 65..=128-bit range (v1 covers that range with
/// a `_harc_u128` composite; the tbir emitter reconstructs the same
/// composite from the words). Returns `None` for non-hex or ≤ 64-bit
/// literals (those take the plain `Expr::Literal` path).
pub(crate) fn parse_wide_hex_literal(s: &str) -> Option<Vec<u32>> {
    let t = s.replace('_', "");
    let hex = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X"))?;
    if hex.len() <= 16 || hex.chars().any(|c| !c.is_ascii_hexdigit()) {
        return None;
    }
    let mut words = Vec::with_capacity(hex.len().div_ceil(8));
    let mut remaining = hex.len();
    while remaining > 0 {
        let start = remaining.saturating_sub(8);
        words.push(u32::from_str_radix(&hex[start..remaining], 16).ok()?);
        remaining = start;
    }
    Some(words)
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
