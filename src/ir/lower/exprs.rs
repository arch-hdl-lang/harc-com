//! Expression lowering. Tree-shaped, no flattening; `Expr::Port` nodes
//! survive only in port-allowed positions (wait predicates, format
//! args, DutRead/DutWrite operands, assert conditions) — everywhere
//! else `lower_expr_no_ports` hoists DUT reads into `DutRead` temps.

use super::{unsupported, FuncBuilder, LowerError};
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
                // The framework error counter (`errors`), referenced from
                // `assert errors == 0` / `${errors}` after a walk like
                // `bitbash(regs)`. Locals shadow (checked above), and
                // codegen emits the framework counter directly.
                if id.name == "errors" {
                    return Ok(Expr::ErrorCount);
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
                // Whole composite-component value read — a self sub-component
                // field passed by value as a method arg (`sb.observe(addr,
                // model)`). Locals shadow (checked above).
                if let Some(cv) = self.as_component_value_read(e)? {
                    return Ok(cv);
                }
                // Closure-hook host-state promotion: a test-scope `let`
                // captured by an `on <obj>.<method> pre/post` hook was
                // promoted to a `_tb` scalar field. A bare ident in that
                // set (locals shadow — checked above) reads the shared
                // `_tb` cell. This is the read counterpart to the
                // `TbFieldWrite` produced for a promoted-let assignment.
                if self.ctx.promoted_tb_lets.contains(&id.name) {
                    return Ok(Expr::TbField(id.name.clone()));
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
                if let Some((sb, field, nested_path)) = self.scoreboard_root(target) {
                    let scalar = self.scoreboard_scalar_field(sb, &name.name)?;
                    return Ok(Expr::ScoreboardQuery {
                        sb,
                        field,
                        query: crate::ir::ScoreboardQuery::Scalar { scalar },
                        nested_path,
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
                    return self.lower_regblock_read_expr(&binding, &reg);
                }
                // Field-level read in expression position
                // (`regs.REG.FIELD` in an assert/format arg). Same
                // read-count semantics as the whole-register form.
                if let Some((binding, reg, fld)) = self.as_regblock_subfield(e) {
                    return self.lower_regblock_subfield_read_expr(&binding, &reg.name, &fld.name);
                }
                // Addrmap access in expression position
                // (`chip.inst.REG[.FIELD]`).
                if let Some(ax) = self.lower_addrmap_read_expr(e)? {
                    return Ok(ax);
                }
                self.reject_out_of_subset_regblock_access(e, "read")?;
                self.reject_out_of_subset_addrmap_access(e, "read")?;
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
                            let Some(fld) = schema.field(&name.name) else {
                                return Err(LowerError::Invalid(format!(
                                    "transaction `{}` has no field `{}`",
                                    schema.name, name.name
                                )));
                            };
                            // A whole-`Vec` field read has no scalar value:
                            // in any scalar/format/assert context the tbir
                            // backend would emit the raw `std::array` member
                            // into a position that expects an integer, which
                            // miscompiles as a raw clang error rather than a
                            // structured HARC diagnostic. Reject it here. The
                            // ONLY sanctioned whole-`Vec` field use is a
                            // `dst.field = src.field` array copy, which the
                            // write arm (`stmts.rs`) special-cases without
                            // routing the RHS through this read path. Element
                            // access (`rec.data[i]`) is handled in the
                            // `Index` arm.
                            if fld.vec_len.is_some() {
                                return Err(unsupported(
                                    &format!(
                                        "a whole-`Vec` read of record field `{}.{}`",
                                        schema.name, name.name
                                    ),
                                    "index the field element-wise (`{rec}.{field}[i]`)",
                                ));
                            }
                            return Ok(Expr::RecordField {
                                local,
                                field: name.name.clone(),
                                index: None,
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
                        if self
                            .inline_frames
                            .iter()
                            .any(|f| f.name.starts_with("_tb."))
                            && self.ctx.tb_methods.contains_key(&id.name)
                        {
                            return self.lower_tb_method_call(&id.name, args);
                        }
                        if let Some(call) =
                            self.lower_transactor_self_call(&id.name, args, true)?
                        {
                            return Ok(call);
                        }
                        if self.helpers.contains(&id.name) {
                            return self.lower_helper_call(&id.name, args);
                        }
                        if self.ctx.extern_fns.contains(&id.name) {
                            return self.lower_extern_fn_call(&id.name, args);
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
                        // Env-level aggregation: `<env>.quiesced(N)` expands
                        // to an AND of `idle(N)` over every leaf sub-component.
                        if let Some(q) = self.as_component_quiesced(callee, args)? {
                            return Ok(q);
                        }
                        // Scoreboard queue value-queries: `sb.q.size()`,
                        // `sb.q.empty()`. (`sb.q.pop()` mutates and is
                        // lowered only as a statement — reaching it here
                        // means it was used in a deeper expression
                        // position, which is rejected below.)
                        if let Some(q) = self.lower_scoreboard_query_call(callee, args)? {
                            return Ok(q);
                        }
                        // Composite-component queue value-queries:
                        // `checker.sb.errors.size()` / `.empty()`.
                        // (`.pop()` mutates → statement-only; rejected here.)
                        if let Some(q) = self.lower_component_queue_query(callee, args)? {
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
                        // Transactor method calls are call EDGES that may
                        // advance simulated time (v1 hookables run
                        // synchronously — their internal `wait`s `tick()`
                        // directly). In expression position the edge is
                        // value-bearing (`(helper.read(0) & 1) == 1`): we
                        // build the call edge here and let `hoist_ports`
                        // pull it into a `Stmt::TransactorCall { dest:
                        // Some(temp), .. }` in the SAME left-to-right pass
                        // as DUT-port reads, so the `tick()` lands in
                        // source order and the seam rule (a TransactorMethod
                        // edge only ever lives in a TransactorCall stmt or a
                        // top-level Assign RHS) is preserved. A void method
                        // used as a value is rejected by `lower_transactor_call`
                        // (`need_ret = true`), mirroring v1's C++ type error.
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
                            if let Some(call) = self.lower_transactor_call(callee, args, true)? {
                                return Ok(call);
                            }
                        }
                        format!("transactor/method call `.{}(...)`", name.name)
                    }
                    _ => "a call expression".to_string(),
                };
                Err(unsupported(&what, ""))
            }
            ExprKind::ForkCall { .. } => Err(unsupported(
                "`fork` bus-method calls in expression position",
                "test-scope `let x = fork bus.m(...)` (initiator-side issue) IS lowered; a \
                 `fork` INSIDE a transactor responder body (target re-issuing a downstream \
                 TLM call — fork-forwarding) is a follow-up slice",
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
            ExprKind::Index { target, index } => {
                // `rec.data[i]` — element read of a `Vec<T, N>` record
                // field. The target is a record-field access on a
                // record-typed local; lower it to an indexed
                // `Expr::RecordField`.
                if let Some(rf) = self.lower_record_vec_index(target, index)? {
                    return Ok(rf);
                }
                // DUT port lane access: `dut.<port>[i]` (constant or
                // runtime index).
                if let Some(port) = self.as_lane_port_ref(e)? {
                    return Ok(Expr::Port(port));
                }
                Err(unsupported(
                    "index expressions",
                    "only `dut.<port>[i]` lane accesses and \
                     `<rec>.<vecfield>[i]` element reads are lowered",
                ))
            }
            ExprKind::BitSlice { target, hi, lo } => {
                // Constant scalar bit-slice `x[hi:lo]` with literal bounds
                // → IR `BitSlice` (right-shift + mask), mirroring v1's
                // scalar slice. A variable part-select (`x[s +: W]` with a
                // non-const offset) does not fold and stays out of subset.
                match (parse_int_literal_expr(hi), parse_int_literal_expr(lo)) {
                    (Some(h), Some(l)) if h >= l => {
                        match (u32::try_from(h), u32::try_from(l)) {
                            (Ok(hi), Ok(lo)) => {
                                let target = Box::new(self.lower_expr(target)?);
                                Ok(Expr::BitSlice { target, hi, lo })
                            }
                            _ => Err(unsupported("bit-slice bounds above 2^32", "")),
                        }
                    }
                    _ => Err(unsupported(
                        "bit-slice expressions with non-constant or hi<lo bounds",
                        "only literal `x[hi:lo]` bounds with hi >= lo are lowered",
                    )),
                }
            }
            // A bare string literal in expression position has no
            // v1-supported landing surface: v1's `local_value_c_type` for a
            // `let s : String` routes through `record_field_c_type ->
            // txn_field_c_type`, which lacks a `BuiltinTy::String` case and
            // falls through to `uint64_t` — emitting `uint64_t s = "...";`,
            // a C++ compile error. (The `const char*` mapping in
            // `c_type_for` only applies to method *params*, never lets.)
            // And `${s}` interpolation always emits `%lld` +
            // `harc_printf_ll`, which also fails for a pointer. Since v1
            // cannot compile ANY string-valued local, lowering it in tbir
            // would diverge from v1 rather than mirror it — keep it out of
            // subset until v1 grows a real string-local surface (audit #425
            // deferral). String *interpolation* (`${...}`) and `log`/`logf`
            // format strings are separate statement-level paths that work.
            ExprKind::String(_) => Err(unsupported("string values in expression position", "")),
            ExprKind::Float(_) => Err(unsupported("float literals", "")),
            ExprKind::Time(s) => {
                // Bare `time` value in expression position (`let t : time =
                // 100ns`). v1's `emit_expr_with_arrow` emits the leading
                // numeric portion verbatim (no unit conversion) and types
                // the local `uint64_t`. Mirror that exactly: take the digit/
                // underscore prefix, strip underscores, parse as u64. (This
                // is NOT the `wait <dur>` path, which converts to ps via
                // `time_literal_to_ps` — a different surface.)
                let digits: String = s
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '_')
                    .filter(|c| *c != '_')
                    .collect();
                let value = digits.parse::<u64>().map_err(|_| {
                    unsupported("time literal with no leading numeric value", "")
                })?;
                Ok(Expr::Literal {
                    value,
                    ty: IrType::UInt(Some(64)),
                })
            }
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

    /// `rec.data[i]` — element read of a `Vec<T, N>` record field.
    /// Returns `Some(Expr::RecordField { index })` when `target` is a
    /// field access (`rec.data`) on a record-typed local whose field is
    /// a `Vec`; `None` if `target` is not such an access (the caller
    /// then tries the DUT-lane and rejection paths). A scalar field
    /// indexed like an array is a hard error (a scalar has no elements).
    pub(crate) fn lower_record_vec_index(
        &mut self,
        target: &AstExpr,
        index: &AstExpr,
    ) -> Result<Option<Expr>, LowerError> {
        let ExprKind::Field { target: ft, name } = &*target.kind else {
            return Ok(None);
        };
        let ExprKind::Ident(root) = &*ft.kind else {
            return Ok(None);
        };
        let Some(local) = self.lookup(&root.name) else {
            return Ok(None);
        };
        let Some(rid) = self.record_of_local(local) else {
            return Ok(None);
        };
        let schema = &self.ctx.records[rid.index()];
        let Some(fld) = schema.field(&name.name) else {
            return Err(LowerError::Invalid(format!(
                "transaction `{}` has no field `{}`",
                schema.name, name.name
            )));
        };
        if fld.vec_len.is_none() {
            return Err(unsupported(
                &format!(
                    "indexing the scalar record field `{}.{}`",
                    schema.name, name.name
                ),
                "only `Vec<T, N>` record fields are indexable",
            ));
        }
        let idx = self.lower_expr(index)?;
        Ok(Some(Expr::RecordField {
            local,
            field: name.name.clone(),
            index: Some(Box::new(idx)),
        }))
    }

    /// Lower and hoist every surviving `Expr::Port` into a `DutRead`
    /// temp in the current block.
    pub(crate) fn lower_expr_no_ports(&mut self, e: &AstExpr) -> Result<Expr, LowerError> {
        let ir = self.lower_expr(e)?;
        Ok(self.hoist_ports(ir))
    }

    pub(crate) fn hoist_ports(&mut self, e: Expr) -> Expr {
        self.hoist_ports_with_hint(e, None)
    }

    fn hoist_ports_with_hint(&mut self, e: Expr, hint: Option<IrType>) -> Expr {
        match e {
            Expr::Port(p) => {
                let t = self.fresh_temp();
                if let Some(ty) = port_temp_type(&p, hint.as_ref()) {
                    self.set_local_type(t, ty);
                }
                self.push(Stmt::DutRead(t, p));
                Expr::Local(t)
            }
            Expr::Binary(op, a, b) => {
                let a_hint = if matches!(op, BinOp::Eq | BinOp::Ne) {
                    self.expr_type(&b)
                } else {
                    None
                };
                let b_hint = if matches!(op, BinOp::Eq | BinOp::Ne) {
                    self.expr_type(&a)
                } else {
                    None
                };
                let a = self.hoist_ports_with_hint(*a, a_hint);
                let b = self.hoist_ports_with_hint(*b, b_hint);
                Expr::Binary(op, Box::new(a), Box::new(b))
            }
            Expr::Unary(op, a) => {
                let a = self.hoist_ports_with_hint(*a, None);
                Expr::Unary(op, Box::new(a))
            }
            Expr::Ternary(c, t, e) => {
                let c = self.hoist_ports_with_hint(*c, None);
                let t = self.hoist_ports_with_hint(*t, None);
                let e = self.hoist_ports_with_hint(*e, None);
                Expr::Ternary(Box::new(c), Box::new(t), Box::new(e))
            }
            Expr::BitSlice { target, hi, lo } => {
                let target = self.hoist_ports_with_hint(*target, None);
                Expr::BitSlice {
                    target: Box::new(target),
                    hi,
                    lo,
                }
            }
            Expr::WidthCast {
                kind,
                width,
                src_width,
                inner,
            } => {
                let inner = self.hoist_ports_with_hint(*inner, None);
                Expr::WidthCast {
                    kind,
                    width,
                    src_width,
                    inner: Box::new(inner),
                }
            }
            Expr::Call(t, args) => {
                let args = args
                    .into_iter()
                    .map(|a| self.hoist_ports_with_hint(a, None))
                    .collect();
                // A value-bearing transactor-method call in expression
                // position: pull the call edge into its own
                // `Stmt::TransactorCall { dest: Some(temp), .. }` (the
                // seam rule's sanctioned home) and substitute the result
                // temp. Args (and sibling ports, since this is the same
                // left-to-right pass as `Expr::Port` hoisting) are already
                // lifted above, so the `tick()` inside the call lands in
                // source order. Helper/Builtin/Tseq targets are ordinary
                // inline values.
                self.hoist_transactor_edge(Expr::Call(t, args))
            }
            Expr::ComponentIdle { base, kind, n } => {
                let n = self.hoist_ports_with_hint(*n, None);
                Expr::ComponentIdle {
                    base,
                    kind,
                    n: Box::new(n),
                }
            }
            Expr::SeqIndex { seq, index } => {
                let index = self.hoist_ports_with_hint(*index, None);
                Expr::SeqIndex {
                    seq,
                    index: Box::new(index),
                }
            }
            // An indexed `Vec`-field read carries the index sub-expr,
            // which may hold a DUT port; hoist into it. A scalar
            // RecordField (index `None`) is the no-op host-state value.
            Expr::RecordField { local, field, index: Some(index) } => {
                let index = self.hoist_ports_with_hint(*index, None);
                Expr::RecordField { local, field, index: Some(Box::new(index)) }
            }
            other @ (Expr::Literal { .. }
            | Expr::WideLiteral(_)
            | Expr::Local(_)
            // The global cycle counter / error counter — framework
            // values, no DUT port.
            | Expr::CycleCount
            | Expr::ErrorCount
            | Expr::RecordField { index: None, .. }
            | Expr::TbField(_)
            // Transactor-instance state is host state — no DUT port inside.
            | Expr::TransactorState { .. }
            // Scoreboard reads are host state — no DUT port inside.
            | Expr::ScoreboardQuery { .. }
            // Component fields are host state — no DUT port inside.
            | Expr::ComponentField { .. }
            // A by-value component arg is host state — no DUT port inside.
            | Expr::ComponentValue { .. }
            // Component-queue size/empty reads are host state — no port.
            | Expr::ComponentQueueQuery { .. }
            // Sequence length is host state — no DUT port inside.
            | Expr::SeqLen(_)
            // A register-level frontdoor read carries no DUT *port*
            // subtree — its bus read routes through the helper lambda,
            // emitted inline. Nothing to hoist.
            | Expr::RegRead { .. }
            | Expr::CovBin { .. }) => other,
        }
    }

    pub(crate) fn expr_type(&self, e: &Expr) -> Option<IrType> {
        match e {
            Expr::Literal { ty, .. } => Some(ty.clone()),
            Expr::WideLiteral(words) => Some(IrType::UInt(Some(wide_literal_bits(words)))),
            Expr::Local(l) => Some(self.local_type(*l).clone()),
            Expr::Port(p) => p.width.map(|w| IrType::UInt(Some(w))),
            Expr::Binary(op, a, b) => match op {
                BinOp::Eq
                | BinOp::Ne
                | BinOp::Lt
                | BinOp::Le
                | BinOp::Gt
                | BinOp::Ge
                | BinOp::And
                | BinOp::Or => Some(IrType::Bool),
                _ => self.expr_type(a).or_else(|| self.expr_type(b)),
            },
            Expr::Unary(_, inner) => self.expr_type(inner),
            Expr::Ternary(_, t, e) => self.expr_type(t).or_else(|| self.expr_type(e)),
            Expr::BitSlice { hi, lo, .. } => Some(IrType::UInt(Some(hi - lo + 1))),
            Expr::WidthCast { kind, width, .. } => Some(match kind {
                WidthCastKind::Sext => IrType::SInt(Some(*width)),
                _ => IrType::UInt(Some(*width)),
            }),
            _ => None,
        }
    }

    /// If `e` is a value-bearing `CallTarget::TransactorMethod` edge,
    /// pull it into a fresh `Stmt::TransactorCall { dest: Some(temp), .. }`
    /// and return `Expr::Local(temp)`; otherwise return `e` unchanged.
    /// This is the seam rule's sanctioned home for a transactor edge that
    /// surfaced in expression position (e.g. `(helper.read(0) & 1) == 1`):
    /// the edge never lives nested in another expression, and the call's
    /// internal `tick()` runs at the hoist point (source order, because the
    /// callers traverse left-to-right).
    fn hoist_transactor_edge(&mut self, e: Expr) -> Expr {
        match &e {
            Expr::Call(crate::ir::CallTarget::TransactorMethod { .. }, _) => {
                let temp = self.fresh_temp();
                self.push(Stmt::TransactorCall {
                    dest: Some(temp),
                    call: e,
                });
                Expr::Local(temp)
            }
            Expr::Call(crate::ir::CallTarget::TransactorSelfMethod { .. }, _) => {
                let temp = self.fresh_temp();
                self.push(Stmt::TransactorSelfCall {
                    dest: Some(temp),
                    call: e,
                });
                Expr::Local(temp)
            }
            _ => e,
        }
    }

    /// Hoist every value-bearing transactor-method call edge out of `e`
    /// into preceding `Stmt::TransactorCall` statements, leaving DUT
    /// `Expr::Port` leaves INLINE (unlike `hoist_ports`). Used where ports
    /// are intentionally left lazy — assert conditions — but a transactor
    /// edge still cannot stay nested (the seam rule, and the call may
    /// advance simulated time). Traverses left-to-right so the synthesized
    /// `TransactorCall`s land in source order.
    ///
    /// Subset note: an expression that mixes a hoisted port read with a
    /// transactor call is not exercised by any fixture; here ports stay
    /// inline (lazy assert eval) so there is no port/tick reordering.
    pub(crate) fn hoist_transactor_calls(&mut self, e: Expr) -> Expr {
        match e {
            Expr::Binary(op, a, b) => {
                let a = self.hoist_transactor_calls(*a);
                let b = self.hoist_transactor_calls(*b);
                Expr::Binary(op, Box::new(a), Box::new(b))
            }
            Expr::Unary(op, a) => {
                let a = self.hoist_transactor_calls(*a);
                Expr::Unary(op, Box::new(a))
            }
            Expr::Ternary(c, t, f) => {
                let c = self.hoist_transactor_calls(*c);
                let t = self.hoist_transactor_calls(*t);
                let f = self.hoist_transactor_calls(*f);
                Expr::Ternary(Box::new(c), Box::new(t), Box::new(f))
            }
            Expr::BitSlice { target, hi, lo } => {
                let target = self.hoist_transactor_calls(*target);
                Expr::BitSlice {
                    target: Box::new(target),
                    hi,
                    lo,
                }
            }
            Expr::WidthCast {
                kind,
                width,
                src_width,
                inner,
            } => {
                let inner = self.hoist_transactor_calls(*inner);
                Expr::WidthCast {
                    kind,
                    width,
                    src_width,
                    inner: Box::new(inner),
                }
            }
            Expr::ComponentIdle { base, kind, n } => {
                let n = self.hoist_transactor_calls(*n);
                Expr::ComponentIdle {
                    base,
                    kind,
                    n: Box::new(n),
                }
            }
            Expr::SeqIndex { seq, index } => {
                let index = self.hoist_transactor_calls(*index);
                Expr::SeqIndex {
                    seq,
                    index: Box::new(index),
                }
            }
            Expr::Call(t, args) => {
                let args = args
                    .into_iter()
                    .map(|a| self.hoist_transactor_calls(a))
                    .collect();
                self.hoist_transactor_edge(Expr::Call(t, args))
            }
            other => other,
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
        let Some((sb, field, queue, method, nested_path)) = self.as_scoreboard_queue_call(callee)
        else {
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
        Ok(Some(Expr::ScoreboardQuery {
            sb,
            field,
            query,
            nested_path,
        }))
    }

    /// Lower `<recv>.<queue>.size()` / `.empty()` on a composite-component
    /// `queue<T>` field into an `Expr::ComponentQueueQuery`, or `None` when
    /// `callee` is not a component-queue method access. A `pop()` reaching
    /// here (deeper than a `let`/assign RHS) is rejected — it mutates and
    /// must be a statement. Mirrors `lower_scoreboard_query_call`.
    fn lower_component_queue_query(
        &self,
        callee: &AstExpr,
        args: &[crate::ast::CallArg],
    ) -> Result<Option<Expr>, LowerError> {
        let Some((base, queue, method)) = self.as_component_queue_call(callee)? else {
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
                    &format!("component `{queue}.pop()` in a nested expression"),
                    "bind it to its own `let` first — `pop` mutates the queue",
                ));
            }
            other => {
                return Err(unsupported(
                    &format!("component queue method `{queue}.{other}(...)`"),
                    "only `push`/`pop`/`size`/`empty` are lowered",
                ));
            }
        };
        if !args.is_empty() {
            return Err(LowerError::Invalid(format!(
                "component `{queue}.{method}()` takes no arguments"
            )));
        }
        Ok(Some(Expr::ComponentQueueQuery { base, query }))
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
                            return Err(unsupported("nested DUT port paths (`dut.a.b`)", ""));
                        }
                        segments.reverse();
                        // A single-segment `dut.<name>` whose name was
                        // declared as a `probe` on `let dut` is a DUT-
                        // internal access, not a top-level port: it lowers
                        // to a `Probe` (read-only) or `Force` (force-
                        // capable) `PortRef` so the tbir backend routes it
                        // through the SV bind-stub accessor. Ordinary ports
                        // keep `Port`. See docs/probe-signals.md.
                        let (access, width) = match self.ctx.probes.get(&segments[0]) {
                            Some(meta) => {
                                let access = if meta.force {
                                    PortAccess::Force
                                } else {
                                    PortAccess::Probe
                                };
                                (access, meta.width)
                            }
                            None => (PortAccess::Port, None),
                        };
                        return Ok(Some(PortRef {
                            testbench_field: self.ctx.dut_field.clone(),
                            port_path: segments,
                            direction: None,
                            width,
                            access,
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
                        if self
                            .ctx
                            .scoreboard_fields
                            .contains_key(segments.last().unwrap())
                        {
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
                        if segments.len() == 1 && self.ctx.tb_scalar_fields.contains(&segments[0]) {
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
            ExprKind::Ident(id)
                if self.lookup(&id.name).is_none()
                    && (self.ctx.bare_transactor_fields.contains(&id.name)
                        || self.ctx.transactor_fields.contains_key(&id.name)) =>
            {
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
        // Two access shapes carry transactor state:
        //   * `target.read_count` — a test-scope `let` bound-to responder
        //     (not `_tb`-prefixed by the impl-for desugaring), so the
        //     root is the instance name directly;
        //   * `_tb.xact.last_read` — a testbench transactor FIELD (the
        //     impl-for desugaring prepends `_tb`), so the instance name
        //     is the middle segment.
        let instance = match &*target.kind {
            ExprKind::Ident(root) => root.name.clone(),
            ExprKind::Field {
                target: inner,
                name: mid,
            } => {
                let ExprKind::Ident(root) = &*inner.kind else {
                    return None;
                };
                if Some(root.name.as_str()) != self.ctx.tb_field.as_deref() {
                    return None;
                }
                mid.name.clone()
            }
            _ => return None,
        };
        let fields = self.ctx.target_state.get(&instance)?;
        fields
            .contains(&name.name)
            .then(|| (instance, name.name.clone()))
    }

    /// `Some(PortRef)` (with `lane`) when the expression is a lane
    /// access on a direct DUT port: `dut.<port>[i]`. A constant index
    /// (integer literal, through parens, or via a `const`/enum name)
    /// folds to `LaneIndex::Const`; any other index expression is
    /// lowered as a runtime value into `LaneIndex::Var`, mirroring v1's
    /// `dut_packed_lane`, which re-renders an arbitrary `&Expr`.
    pub(crate) fn as_lane_port_ref(&mut self, e: &AstExpr) -> Result<Option<PortRef>, LowerError> {
        let ExprKind::Index { target, index } = &*e.kind else {
            return Ok(None);
        };
        let Some(mut port) = self.as_port_ref(target)? else {
            return Ok(None);
        };
        port.lane = Some(match self.const_eval_index(index) {
            Some(lane) => crate::ir::LaneIndex::Const(lane),
            None => crate::ir::LaneIndex::Var(Box::new(self.lower_expr(index)?)),
        });
        Ok(Some(port))
    }

    /// Constant-evaluate a lane index: integer literal, parenthesized
    /// literal, or a `const`/enum-variant name.
    pub(crate) fn const_eval_index(&self, e: &AstExpr) -> Option<u64> {
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
    /// receiver width, ≤ 128-bit subset (the `_harc_u128` model carries
    /// 65..128-bit casts; >128-bit `HarcWide<N>` casts are not in the
    /// IR's expression model yet).
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
        if width > 128 {
            return Err(unsupported(
                &format!("`.{kind_name}<{width}>()` with a width above 128 bits"),
                "the TB-IR expression model carries scalars up to 128 bits \
                 (`_harc_u128`); the >128-bit `HarcWide<N>` word-array model \
                 is not lowered yet",
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
/// `bits<W>` relabel with W ≤ 128. The 65..128 form is accepted for
/// wide-local/port construction where the surrounding expression or
/// destination carries `_harc_u128` storage; it remains a relabel in the
/// IR expression tree. Width-less
/// scalar casts give 64.
pub(crate) fn cast_relabel_width(ty: &TypeExpr) -> Option<u32> {
    let TypeExpr::Builtin { name, args, .. } = ty else {
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
    let width = match args.first() {
        Some(TypeArg::Expr(e)) => match &*e.kind {
            ExprKind::Int(s) => s.replace('_', "").parse::<u32>().ok()?,
            _ => return None,
        },
        Some(_) => return None,
        None => 64,
    };
    (width > 0 && width <= 128).then_some(width)
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

pub(crate) fn lower_bin_op(op: BinaryOp) -> Result<BinOp, LowerError> {
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

/// Fold an AST expression to a `u64` when it is an integer literal
/// (optionally parenthesized). Used by the target-side `out_of_order
/// tags N` responder lowering to range-check the literal tag count;
/// mirrors v1's `fold_int_literal` over the same surface.
pub(crate) fn parse_int_literal_expr(e: &crate::ast::Expr) -> Option<u64> {
    match &*e.kind {
        crate::ast::ExprKind::Int(s) => parse_int_literal(s),
        crate::ast::ExprKind::Paren(inner) => parse_int_literal_expr(inner),
        _ => None,
    }
}

fn port_temp_type(p: &PortRef, hint: Option<&IrType>) -> Option<IrType> {
    if let Some(w) = p.width {
        return Some(IrType::UInt(Some(w)));
    }
    match hint {
        Some(IrType::UInt(Some(w)) | IrType::SInt(Some(w))) if *w > 64 => {
            Some(IrType::UInt(Some(*w)))
        }
        Some(IrType::Bool) => Some(IrType::Bool),
        _ => None,
    }
}

fn wide_literal_bits(words: &[u32]) -> u32 {
    let Some((idx, word)) = words.iter().enumerate().rev().find(|(_, w)| **w != 0) else {
        return 1;
    };
    (idx as u32) * 32 + (32 - word.leading_zeros())
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

/// True when `e` contains a (nested) transactor call edge anywhere. Used
/// to reject a transactor method call in positions that cannot hoist it
/// into a preceding call statement — notably a `wait until` predicate,
/// which the scheduler re-evaluates every cycle (a per-cycle call would
/// be nonsensical).
pub(crate) fn expr_has_transactor_edge(e: &Expr) -> bool {
    match e {
        Expr::Call(
            crate::ir::CallTarget::TransactorMethod { .. }
            | crate::ir::CallTarget::TransactorSelfMethod { .. },
            _,
        ) => true,
        Expr::Call(_, args) => args.iter().any(expr_has_transactor_edge),
        Expr::Binary(_, a, b) => expr_has_transactor_edge(a) || expr_has_transactor_edge(b),
        Expr::Unary(_, a) => expr_has_transactor_edge(a),
        Expr::Ternary(c, t, f) => {
            expr_has_transactor_edge(c)
                || expr_has_transactor_edge(t)
                || expr_has_transactor_edge(f)
        }
        Expr::WidthCast { inner, .. } => expr_has_transactor_edge(inner),
        Expr::ComponentIdle { n, .. } => expr_has_transactor_edge(n),
        Expr::SeqIndex { index, .. } => expr_has_transactor_edge(index),
        _ => false,
    }
}
