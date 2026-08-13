//! Expression lowering. Tree-shaped, no flattening; `Expr::Port` nodes
//! survive only in port-allowed positions (wait predicates, format
//! args, DutRead/DutWrite operands, assert conditions) — everywhere
//! else `lower_expr_no_ports` hoists DUT reads into `DutRead` temps.

use super::{unsupported, FuncBuilder, LowerError};
use crate::ast::{
    BinaryOp, BuiltinTy, CallArg, Expr as AstExpr, ExprKind, TypeArg, TypeExpr, UnaryOp,
};
use crate::ir::{
    BinOp, Expr, IrType, LocalId, PortAccess, PortRef, RecordId, Stmt, UnOp, WidthCastKind,
};

/// Resolution of a (possibly nested) record field-access chain
/// `ident.f1.f2...fn` rooted at a record-typed local. `field` is the
/// first-level field (`f1`) on the local's record; `path` is the further
/// nested field names (`f2..fn`); the leaf is the last of `[field] ++ path`.
pub(crate) struct RecordFieldChain {
    pub local: LocalId,
    pub field: String,
    pub path: Vec<String>,
    /// Element selections on NON-leaf `Vec<Record, N>` segments
    /// (`tbl.entries[i].tag`): `(pos, idx)` indexes the segment at `pos`
    /// in `[field] ++ path` and descends into the element record. The
    /// leaf's own index (an OUTERMOST `[i]`) is peeled by the caller and
    /// never appears here, so every `pos` is strictly below the leaf.
    pub mid_indices: Vec<(usize, Expr)>,
    /// `Some(N)` when the leaf is a `Vec<T, N>` field.
    pub leaf_vec_len: Option<usize>,
    /// The leaf field's element/scalar/record type.
    pub leaf_ty: IrType,
    /// Dotted `Rec.f1.f2` spelling for diagnostics.
    pub dotted: String,
}

/// Resolution of a subfield access onto a bound-to target responder's
/// whole-record state field (`last.addr` / `responder.last.addr`).
/// `instance` is the bound testbench-field instance (empty placeholder in
/// a responder body, filled at test-binding); `field` is the record state
/// field; `path` is the nested subfield chain (length ≥ 1).
pub(crate) struct TransactorStateRecordChain {
    pub instance: String,
    pub field: String,
    pub path: Vec<String>,
    /// `Some(N)` when the leaf is a `Vec<T, N>` field (rejected: whole-Vec
    /// state-record access is out of subset; index element-wise instead).
    pub leaf_vec_len: Option<usize>,
}

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
                // Whole transaction/struct-typed testbench field read
                // (`cur`) from an inlined testbench method. Ordinary
                // lookup is intentionally fenced at inline-helper
                // boundaries, but shared testbench record state must remain
                // visible so calls like `drv.drive(cur)` pass the persistent
                // record object rather than looking for a caller local.
                if let Some(local) = self.lookup_tb_record_field_in_capture_scope(&id.name) {
                    if self.record_of_local(local).is_some() {
                        return Ok(Expr::Local(local));
                    }
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
                // fills it once the passive instance is resolved. A bare
                // read is only valid for a SCALAR field; a queue field is
                // read via its ops (`.size()`/`.empty()`/`.pop()`), so a
                // bare queue ident is rejected precisely.
                if let Some(kind) = self.target_state_fields.get(&id.name) {
                    return match kind {
                        // A bare read of a scalar OR a whole-record state
                        // field is `TransactorState` (`instance.<field>`);
                        // for a record this is a by-value struct read
                        // (copied into a `let`, pushed onto a queue, …).
                        crate::ir::StateFieldKind::Scalar { .. }
                        | crate::ir::StateFieldKind::Record { .. } => Ok(Expr::TransactorState {
                            instance: String::new(),
                            field: id.name.clone(),
                        }),
                        crate::ir::StateFieldKind::Queue { .. } => Err(unsupported(
                            &format!("a bare read of the `queue` state field `{}`", id.name),
                            "read a queue state field via `.size()` / `.empty()` / `.pop()`",
                        )),
                    };
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
                        ty: if self.ctx.const_signed.get(&id.name).copied().unwrap_or(false) {
                            IrType::SInt(None)
                        } else {
                            IrType::UInt(None)
                        },
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
                // Scalar testbench host state (`expected_checks`) and
                // promoted test-scope lets live on `_tb`. Bare access is
                // allowed only from the test/check/hook body itself or from
                // an inlined `_tb.<method>` frame; free helpers stay fenced.
                if let Some(field) = self.tb_scalar_field_in_capture_scope(&id.name) {
                    return Ok(Expr::TbField(field));
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
                // Whole transaction/struct-typed testbench field read
                // (`_tb.cur`) — used when passing shared record state to
                // helpers, monitors, or scoreboards. Field-level reads
                // (`_tb.cur.value`) are handled below by the record-field
                // path.
                if let Some(local) = self.record_target_local(e) {
                    if self.record_of_local(local).is_some() {
                        return Ok(Expr::Local(local));
                    }
                }
                // Subfield read of a bound-to target responder's whole-
                // record state field (`last.data` in a responder body /
                // `responder.last.data` from the test). Checked before the
                // whole-record `as_transactor_state` lane, which only fires
                // when there is NO further subfield.
                if let Some(chain) = self.as_transactor_state_record_field(e)? {
                    if chain.leaf_vec_len.is_some() {
                        return Err(unsupported(
                            &format!(
                                "a whole-`Vec` read of record state field `{}.{}`",
                                chain.field,
                                chain.path.join(".")
                            ),
                            "read a `Vec` record field element-wise (`{field}.{vec}[i]`)",
                        ));
                    }
                    return Ok(Expr::TransactorStateRecordField {
                        instance: chain.instance,
                        field: chain.field,
                        path: chain.path,
                    });
                }
                // Test-scope read of a bound-to target responder's
                // persistent state (`target.read_count`, or a whole-record
                // `target.last`).
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
                // `t.field` read on a record-typed local (and nested
                // `t.a.b`). Resolve the field chain to its leaf schema.
                if let Some(chain) = self.try_record_field_chain(e)? {
                    // A whole-`Vec` leaf read has no scalar value: in any
                    // scalar/format/assert context the tbir backend would
                    // emit the raw `std::array` member into a position that
                    // expects an integer, which miscompiles as a raw clang
                    // error rather than a structured HARC diagnostic. Reject
                    // it here. The ONLY sanctioned whole-`Vec` field use is a
                    // `dst.field = src.field` array copy, which the write arm
                    // (`stmts.rs`) special-cases without routing the RHS
                    // through this read path. Element access (`rec.data[i]`)
                    // is handled in the `Index` arm. A whole nested-record
                    // leaf read (`let d = s.inner`) IS allowed — it yields
                    // the nested struct value (emitted as `local.field.p…`).
                    if chain.leaf_vec_len.is_some() {
                        return Err(unsupported(
                            &format!("a whole-`Vec` read of record field `{}`", chain.dotted),
                            "index the field element-wise (`{rec}.{field}[i]`)",
                        ));
                    }
                    return Ok(Expr::RecordField {
                        local: chain.local,
                        field: chain.field,
                        path: chain.path,
                        mid_indices: chain.mid_indices,
                        index: None,
                    });
                }
                // Bus-bound signal access (`<bind>.<sig>`, `<bind>.<ch>.<sig>`).
                if let Some(port) = self.as_bus_port_ref(e)? {
                    return Ok(Expr::Port(port));
                }
                Err(unsupported(
                    &format!("field access on a non-DUT value ending in `.{}`", name.name),
                    "",
                ))
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
                let inner = Expr::Binary(ir_op, Box::new(l), Box::new(r));
                // Wrapping arithmetic `+% -% *%` (harc#473): mask the result
                // to `max(W(lhs), W(rhs))` bits, matching ARCH's
                // `AddWrap/SubWrap/MulWrap` (result width = wider operand, no
                // widening). The mask is a `WidthCast::Trunc`, which codegen
                // lowers to `(expr) & ((1<<W)-1)`. Non-wrapping ops pass
                // through unchanged.
                if matches!(
                    op,
                    BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap
                ) {
                    return self.wrap_to_operand_width(*op, lhs, rhs, inner);
                }
                Ok(inner)
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
                        if self.in_testbench_method_frame()
                            && self.ctx.tb_methods.contains_key(&id.name)
                        {
                            return self.lower_tb_method_call(&id.name, args);
                        }
                        if let Some(call) = self.lower_transactor_self_call(&id.name, args, true)? {
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
                        // Bound-to target-responder queue state-field
                        // value-queries: `pending.size()` / `.empty()`
                        // (bare field name inside a responder body).
                        // (`.pop()` mutates → statement-only; rejected here.)
                        if let Some(q) = self.lower_state_queue_query(callee, args)? {
                            return Ok(q);
                        }
                        // Test-scope target-responder queue state read:
                        // `target.pending.size()` / `.empty()` (fully
                        // resolved instance). (`.pop()` → statement-only.)
                        if let Some(q) = self.lower_test_state_queue_query(callee, args)? {
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
                    let width = cast_relabel_width(ty).expect("checked above");
                    let kind = match ty {
                        TypeExpr::Builtin {
                            name: BuiltinTy::SInt | BuiltinTy::SIntCap,
                            ..
                        } => WidthCastKind::Sext,
                        _ => WidthCastKind::Zext,
                    };
                    // An explicit `as sint<W>` is a signedness relabel, not
                    // a sign extension: it must preserve the 64-bit value
                    // even when the source expression has a narrower
                    // declared width. Keep the target width as the source
                    // width metadata so TBIR can select signed operators
                    // without applying a value-changing extension.
                    let src_width = if matches!(kind, WidthCastKind::Sext) {
                        Some(width)
                    } else {
                        self.infer_expr_width(expr)
                    };
                    let inner = self.lower_expr(expr)?;
                    return Ok(Expr::WidthCast {
                        kind,
                        width,
                        src_width,
                        inner: Box::new(inner),
                    });
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
                    (Some(h), Some(l)) if h >= l => match (u32::try_from(h), u32::try_from(l)) {
                        (Ok(hi), Ok(lo)) => {
                            let target = Box::new(self.lower_expr(target)?);
                            Ok(Expr::BitSlice { target, hi, lo })
                        }
                        _ => Err(unsupported("bit-slice bounds above 2^32", "")),
                    },
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
                // the local `uint64_t`. We mirror that for the common case:
                // take the digit/underscore prefix, strip underscores, parse
                // as u64. (This is NOT the `wait <dur>` path, which converts
                // to ps via `time_literal_to_ps` — a different surface.)
                //
                // INTENTIONAL DIVERGENCE from v1 (authorized 2026-06-19, see
                // the "Time-literal digit separators" note in tbir-mvp.md):
                // for a digit-separated literal like `1_000ns`, v1 emits the
                // prefix verbatim — `uint64_t t = 1_000;` — which is a C++
                // compile error (no `operator""_000`). We strip the `_` and
                // lower `1000`, which is what the source plainly means. tbir
                // is the more-correct backend here; v1's behavior is a legacy
                // limitation, not a contract we preserve.
                let digits: String = s
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '_')
                    .filter(|c| *c != '_')
                    .collect();
                let value = digits
                    .parse::<u64>()
                    .map_err(|_| unsupported("time literal with no leading numeric value", ""))?;
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
            ExprKind::SoftConstraint(_) => Err(unsupported("`soft` constraints", "")),
            ExprKind::ForEachConstraint { .. } => {
                Err(unsupported("constraint `for` comprehensions", ""))
            }
        }
    }

    /// Resolve an AST field-access chain `ident.f1.f2...fn` rooted at a
    /// record-typed local into a [`RecordFieldChain`], descending through
    /// nested `IrType::Record` fields to reach the leaf. A NON-leaf
    /// segment may carry an element selection on a `Vec<Record, N>` field
    /// (`tbl.entries[i].tag`, at any depth) — collected into
    /// `mid_indices`; the descent then continues through the element
    /// record. Returns:
    ///   - `Ok(None)` when `e` is not a field access rooted at an `Ident`
    ///     bound to a record local (the caller falls through to the other
    ///     lanes: DUT signal, scoreboard, recv payload, …);
    ///   - `Err` when it IS such a chain but a component names no field, or
    ///     a non-leaf component is not a nested record (so it cannot be
    ///     descended into), or an element selection sits on a non-`Vec`
    ///     field / a `Vec` of records is traversed without one.
    pub(crate) fn try_record_field_chain(
        &mut self,
        e: &AstExpr,
    ) -> Result<Option<RecordFieldChain>, LowerError> {
        // Flatten `a.b.c` → root `a`, segments `[b, c]` (outer-to-inner
        // during the walk, reversed to declaration order after). An
        // `Index` node between segments records a pending element
        // selection that attaches to the NEXT (inner) `Field` segment:
        // in `tbl.entries[i].tag` the walk sees `.tag`, then `[i]`, then
        // `.entries` — so `[i]` belongs to `entries`.
        let mut segs: Vec<String> = Vec::new();
        // `(push-order seg position, index AST)` per element selection.
        let mut raw_indices: Vec<(usize, &AstExpr)> = Vec::new();
        let mut pending_index: Option<&AstExpr> = None;
        let mut cur = e;
        let root = loop {
            match &*cur.kind {
                ExprKind::Field { target, name } => {
                    if let Some(idx) = pending_index.take() {
                        raw_indices.push((segs.len(), idx));
                    }
                    segs.push(name.name.clone());
                    cur = target;
                }
                ExprKind::Index { target, index } => {
                    if pending_index.is_some() {
                        // `a.b[i][j].c` — no record-field shape has a
                        // second dimension (`Vec` of `Vec` never lowers
                        // as a field type). The root is not resolved yet,
                        // so fall through rather than claim a shape that
                        // may belong to another lane; the caller's
                        // rejection names the unsupported access.
                        return Ok(None);
                    }
                    if segs.is_empty() {
                        // Outermost node is an `Index` (`tbl.entries[i]`
                        // as a whole) — the element read/write lanes peel
                        // it before calling here; any other indexed
                        // non-field shape is not this lane's chain.
                        return Ok(None);
                    }
                    pending_index = Some(index);
                    cur = target;
                }
                ExprKind::Ident(root) => {
                    if pending_index.is_some() {
                        // `ident[i].f` — the root local itself is indexed;
                        // not a record-field chain (lane ports and seq
                        // element reads route elsewhere).
                        return Ok(None);
                    }
                    break root;
                }
                // Innermost target is not a bare ident (`f().x`, …):
                // not a record-local chain this lane handles.
                _ => return Ok(None),
            }
        };
        if segs.is_empty() {
            return Ok(None); // bare ident, no field access
        }
        segs.reverse();
        let mut field_start = 0usize;
        let local = if let Some(local) = self.lookup(&root.name) {
            local
        } else if let Some(tb_field) = self.ctx.tb_field.as_deref() {
            if root.name == tb_field {
                let Some(tb_record) = segs.first() else {
                    return Ok(None);
                };
                let Some(local) = self.lookup_tb_record_field_in_capture_scope(tb_record) else {
                    return Ok(None);
                };
                field_start = 1;
                local
            } else {
                return Ok(None);
            }
        } else {
            return Ok(None);
        };
        let Some(mut cur_rid) = self.record_of_local(local) else {
            return Ok(None);
        };
        if field_start >= segs.len() {
            return Ok(None);
        }
        // Convert element selections from push-order to declaration-order
        // positions relative to `fields`. An index landing BELOW
        // `field_start` selects on the record local itself (`_tb.cur[i]…`)
        // — not this lane's chain. Checked for every entry before any
        // index lowers, so the fall-through leaves no hoisted temps.
        let total = segs.len();
        if raw_indices
            .iter()
            .any(|(p, _)| total - 1 - p < field_start)
        {
            return Ok(None);
        }
        // Lower the index expressions left-to-right (chain order — the
        // walk collected them inner-to-outer), so hoisted statements keep
        // source order.
        let mut mid_indices: Vec<(usize, Expr)> = Vec::with_capacity(raw_indices.len());
        for (raw_pos, idx_ast) in raw_indices.iter().rev() {
            let pos = (total - 1 - raw_pos) - field_start;
            let idx = self.lower_expr(idx_ast)?;
            mid_indices.push((pos, idx));
        }
        let mut dotted = self.ctx.records[cur_rid.index()].name.clone();
        let fields = &segs[field_start..];
        let last = fields.len() - 1;
        let mut leaf_vec_len = None;
        let mut leaf_ty = IrType::Bool; // overwritten at the leaf
        for (i, seg) in fields.iter().enumerate() {
            let schema = &self.ctx.records[cur_rid.index()];
            let Some(fld) = schema.field(seg) else {
                return Err(LowerError::Invalid(format!(
                    "record `{}` has no field `{seg}`",
                    schema.name
                )));
            };
            dotted.push('.');
            dotted.push_str(seg);
            if i == last {
                // The walk attaches an index to the segment BELOW it, so
                // the leaf never carries a mid index (an outermost `[i]`
                // is peeled by the element read/write lanes).
                leaf_vec_len = fld.vec_len;
                leaf_ty = fld.ty.clone();
                break;
            }
            let indexed = mid_indices.iter().any(|(p, _)| *p == i);
            // A non-leaf component must reach a nested record to descend
            // into: either a plain nested-record field, or one element of
            // a `Vec<Record, N>` field selected by `[i]`.
            match fld.ty {
                IrType::Record(next) if fld.vec_len.is_none() && !indexed => cur_rid = next,
                IrType::Record(next) if fld.vec_len.is_some() && indexed => {
                    if let Some((_, idx)) = mid_indices.iter().find(|(p, _)| *p == i) {
                        check_literal_vec_index_bounds(
                            &dotted,
                            idx,
                            fld.vec_len.unwrap_or(0),
                        )?;
                    }
                    cur_rid = next;
                }
                _ if indexed && fld.vec_len.is_none() => {
                    return Err(unsupported(
                        &format!("indexing the non-`Vec` record field `{dotted}`"),
                        "only `Vec<T, N>` record fields are indexable",
                    ));
                }
                _ if indexed => {
                    return Err(unsupported(
                        &format!(
                            "field access `.{}` on an element of `{dotted}`, \
                             whose elements are scalars",
                            fields[i + 1]
                        ),
                        "only `Vec` fields with struct/transaction elements can be \
                         traversed further",
                    ));
                }
                IrType::Record(_) if fld.vec_len.is_some() => {
                    return Err(unsupported(
                        &format!(
                            "traversing the `Vec` record field `{dotted}` without an \
                             element index; cannot access `.{}`",
                            fields[i + 1]
                        ),
                        format!("select one element first (`{seg}[i].{}`)", fields[i + 1]),
                    ));
                }
                _ => {
                    return Err(unsupported(
                        &format!(
                            "field `{}.{seg}` is not a nested record; cannot access `.{}`",
                            schema.name,
                            fields[i + 1]
                        ),
                        "only nested struct/transaction fields can be traversed further",
                    ));
                }
            }
        }
        let field = fields[0].clone();
        let path = fields[1..].to_vec();
        Ok(Some(RecordFieldChain {
            local,
            field,
            path,
            mid_indices,
            leaf_vec_len,
            leaf_ty,
            dotted,
        }))
    }

    /// `Some(rid)` when `e` is a *whole* record value (a record-typed local,
    /// a whole nested-record field read, or one `Vec<Record, N>` element —
    /// `tbl.entries[i]`). Used to validate a whole-record field assignment
    /// (`o.a = d`) and record-typed `let`/copy RHS shapes.
    pub(crate) fn record_id_of_expr(&self, e: &Expr) -> Option<RecordId> {
        match e {
            Expr::Local(l) => self.record_of_local(*l),
            Expr::RecordField {
                local,
                field,
                path,
                mid_indices,
                index,
            } => {
                let mut cur = self.record_of_local(*local)?;
                let segs: Vec<&String> = std::iter::once(field).chain(path.iter()).collect();
                let last = segs.len() - 1;
                for (i, seg) in segs.iter().enumerate() {
                    let fld = self.ctx.records.get(cur.index())?.field(seg)?;
                    let indexed = if i == last {
                        index.is_some()
                    } else {
                        mid_indices.iter().any(|(p, _)| *p == i)
                    };
                    // A record value at each step: a plain nested-record
                    // field, or one indexed `Vec<Record, N>` element. A
                    // whole (unindexed) `Vec` leaf is an array, not a
                    // record value.
                    match fld.ty {
                        IrType::Record(r) if fld.vec_len.is_none() == !indexed => {
                            if i == last {
                                return Some(r);
                            }
                            cur = r;
                        }
                        _ => return None,
                    }
                }
                None
            }
            // A whole-record read of a target-transactor state field
            // (`responder.last` / bare `last`) — resolve via the instance's
            // (or the responder body's) state-field table.
            Expr::TransactorState { instance, field } => {
                let kind = if instance.is_empty() {
                    self.target_state_fields.get(field)
                } else {
                    self.ctx.target_state.get(instance)?.get(field)
                };
                match kind {
                    Some(crate::ir::StateFieldKind::Record { record }) => Some(*record),
                    _ => None,
                }
            }
            // A nested whole-record subfield read of a state record
            // (`responder.last.inner`, where `inner` is itself a record).
            Expr::TransactorStateRecordField {
                instance,
                field,
                path,
            } => {
                let kind = if instance.is_empty() {
                    self.target_state_fields.get(field)
                } else {
                    self.ctx.target_state.get(instance)?.get(field)
                };
                let Some(crate::ir::StateFieldKind::Record { record }) = kind else {
                    return None;
                };
                let mut cur = *record;
                let last = path.len().checked_sub(1)?;
                for (i, seg) in path.iter().enumerate() {
                    let fld = self.ctx.records.get(cur.index())?.field(seg)?;
                    match fld.ty {
                        IrType::Record(r) if fld.vec_len.is_none() => {
                            if i == last {
                                return Some(r);
                            }
                            cur = r;
                        }
                        _ => return None,
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// `rec.data[i]` — element read of a `Vec<T, N>` record field, at any
    /// nesting depth (`s.a.b[i]`). Returns `Some(Expr::RecordField { index })`
    /// when `target` is a field-access chain on a record-typed local whose
    /// leaf is a `Vec`; `None` if `target` is not such a chain (the caller
    /// then tries the DUT-lane and rejection paths). A scalar leaf indexed
    /// like an array is a hard error (a scalar has no elements).
    pub(crate) fn lower_record_vec_index(
        &mut self,
        target: &AstExpr,
        index: &AstExpr,
    ) -> Result<Option<Expr>, LowerError> {
        let Some(chain) = self.try_record_field_chain(target)? else {
            return Ok(None);
        };
        if chain.leaf_vec_len.is_none() {
            return Err(unsupported(
                &format!("indexing the scalar record field `{}`", chain.dotted),
                "only `Vec<T, N>` record fields are indexable",
            ));
        }
        let idx = self.lower_expr(index)?;
        check_literal_vec_index_bounds(&chain.dotted, &idx, chain.leaf_vec_len.unwrap_or(0))?;
        Ok(Some(Expr::RecordField {
            local: chain.local,
            field: chain.field,
            path: chain.path,
            mid_indices: chain.mid_indices,
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

    /// Lower an expression with a width hint so a bare DUT-port read in
    /// value position (e.g. a wide TLM method argument `wide.send(dut.payload)`)
    /// hoists into a temp typed at the hint's width instead of the default
    /// u64. Without the hint a `>64-bit` port read would silently truncate.
    pub(crate) fn lower_expr_no_ports_hinted(
        &mut self,
        e: &AstExpr,
        hint: Option<IrType>,
    ) -> Result<Expr, LowerError> {
        let ir = self.lower_expr(e)?;
        Ok(self.hoist_ports_with_hint(ir, hint))
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
            // An indexed `Vec`-field read carries index sub-exprs (the
            // leaf `[i]` and any mid-chain `entries[i].…` selections),
            // which may hold DUT ports; hoist into each. A plain scalar
            // RecordField (no indices) is the no-op host-state value.
            Expr::RecordField {
                local,
                field,
                path,
                mid_indices,
                index,
            } if index.is_some() || !mid_indices.is_empty() => {
                let mid_indices = mid_indices
                    .into_iter()
                    .map(|(p, idx)| (p, self.hoist_ports_with_hint(idx, None)))
                    .collect();
                let index =
                    index.map(|idx| Box::new(self.hoist_ports_with_hint(*idx, None)));
                Expr::RecordField {
                    local,
                    field,
                    path,
                    mid_indices,
                    index,
                }
            }
            Expr::CovHookParam { param, field, index: Some(index) } => {
                let index = self.hoist_ports_with_hint(*index, None);
                Expr::CovHookParam { param, field, index: Some(Box::new(index)) }
            }
            other @ (Expr::Literal { .. }
            | Expr::WideLiteral(_)
            | Expr::Local(_)
            // The global cycle counter / error counter — framework
            // values, no DUT port.
            | Expr::CycleCount
            | Expr::ErrorCount
            // Index-free RecordFields only — the guarded arm above
            // consumed every index-carrying shape.
            | Expr::RecordField { .. }
            | Expr::CovHookParam { index: None, .. }
            | Expr::CovHookArg { .. }
            | Expr::TbField(_)
            // Transactor-instance state is host state — no DUT port inside.
            | Expr::TransactorState { .. }
            // A record-state subfield read is host state — no DUT port inside.
            | Expr::TransactorStateRecordField { .. }
            // Target-state queue size/empty reads are host state — no port.
            | Expr::TransactorStateQueueQuery { .. }
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
            // A record-field chain types as its leaf: the leaf field's own
            // scalar/record type, or the element type when the leaf `Vec`
            // is indexed. A whole (unindexed) `Vec` leaf is an array — it
            // has no expression-value type here. This is what types an
            // untyped `let e = tbl.entries[i]` as the element record.
            Expr::RecordField {
                local,
                field,
                path,
                mid_indices,
                index,
            } => {
                let mut cur = self.record_of_local(*local)?;
                let segs: Vec<&String> = std::iter::once(field).chain(path.iter()).collect();
                let last = segs.len() - 1;
                for (i, seg) in segs.iter().enumerate() {
                    let fld = self.ctx.records.get(cur.index())?.field(seg)?;
                    if i == last {
                        return match (fld.vec_len, index.is_some()) {
                            (Some(_), true) | (None, false) => Some(fld.ty.clone()),
                            _ => None,
                        };
                    }
                    let indexed = mid_indices.iter().any(|(p, _)| *p == i);
                    match fld.ty {
                        IrType::Record(r) if fld.vec_len.is_none() == !indexed => cur = r,
                        _ => return None,
                    }
                }
                None
            }
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

    /// Lower `<field>.size()` / `<field>.empty()` on a bound-to target
    /// transactor's persistent `queue<T>` state field (a bare field name
    /// inside a responder body) into an `Expr::TransactorStateQueueQuery`,
    /// or `None` when `callee` is not a state-queue method access. The
    /// `instance` is a placeholder filled at test-binding. A `pop()`
    /// reaching here (deeper than a `let`/assign RHS) is rejected — it
    /// mutates and must be a statement. Mirrors `lower_component_queue_query`.
    fn lower_state_queue_query(
        &self,
        callee: &AstExpr,
        args: &[crate::ast::CallArg],
    ) -> Result<Option<Expr>, LowerError> {
        let ExprKind::Field { target, name } = &*callee.kind else {
            return Ok(None);
        };
        let ExprKind::Ident(id) = &*target.kind else {
            return Ok(None);
        };
        // A local of the same name shadows the state field (matches the
        // bare-read resolution order).
        if self.lookup(&id.name).is_some() {
            return Ok(None);
        }
        if !matches!(
            self.target_state_fields.get(&id.name),
            Some(crate::ir::StateFieldKind::Queue { .. })
        ) {
            return Ok(None);
        }
        let field = id.name.clone();
        let method = name.name.clone();
        let query = match method.as_str() {
            "size" => crate::ir::ScoreboardQuery::QueueSize {
                queue: field.clone(),
            },
            "empty" => crate::ir::ScoreboardQuery::QueueEmpty {
                queue: field.clone(),
            },
            "pop" => {
                return Err(unsupported(
                    &format!("target-state `{field}.pop()` in a nested expression"),
                    "bind it to its own `let` first — `pop` mutates the queue",
                ));
            }
            other => {
                return Err(unsupported(
                    &format!("target-state queue method `{field}.{other}(...)`"),
                    "only `push`/`pop`/`size`/`empty` are lowered",
                ));
            }
        };
        if !args.is_empty() {
            return Err(LowerError::Invalid(format!(
                "target-state `{field}.{method}()` takes no arguments"
            )));
        }
        Ok(Some(Expr::TransactorStateQueueQuery {
            instance: String::new(),
            field,
            query,
        }))
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
                            aggregate_path: true,
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
                        if self
                            .ctx
                            .tb_record_fields
                            .iter()
                            .any(|(field, _)| field == segments.last().unwrap())
                        {
                            return Ok(None);
                        }
                        if segments.len() == 1
                            && self
                                .ctx
                                .tb_record_fields
                                .iter()
                                .any(|(field, _)| field == &segments[0])
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

    /// Resolve a record-field target root. Supports both bare record
    /// locals (`cur.value`) and desugared testbench record fields
    /// (`_tb.cur.value`), where `cur` is a synthetic local declared at
    /// function entry but emitted as shared test-scope state.
    pub(crate) fn record_target_local(&self, target: &AstExpr) -> Option<crate::ir::LocalId> {
        match &*target.kind {
            ExprKind::Ident(root) => self
                .lookup(&root.name)
                .or_else(|| self.lookup_tb_record_field_in_capture_scope(&root.name)),
            ExprKind::Field { target, name } => {
                let tb_field = self.ctx.tb_field.as_deref()?;
                let ExprKind::Ident(root) = &*target.kind else {
                    return None;
                };
                if root.name != tb_field {
                    return None;
                }
                if !self
                    .ctx
                    .tb_record_fields
                    .iter()
                    .any(|(field, _)| field == &name.name)
                {
                    return None;
                }
                self.lookup_tb_record_field_in_capture_scope(&name.name)
            }
            ExprKind::Paren(inner) => self.record_target_local(inner),
            _ => None,
        }
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
        // A SCALAR field is a bare `target.<field>` read; a whole-record
        // field is a `target.<field>` value read (by-value struct copy).
        // A queue field is read via `.size()`/`.empty()`/`.pop()`, and a
        // record SUBFIELD (`target.last.addr`) is handled by the earlier
        // `as_transactor_state_record_field` lane, so both are excluded.
        matches!(
            fields.get(&name.name),
            Some(crate::ir::StateFieldKind::Scalar { .. } | crate::ir::StateFieldKind::Record { .. })
        )
        .then(|| (instance, name.name.clone()))
    }

    /// Recognize a test-scope `target.<queue>.size()` / `.empty()` read
    /// on a bound-to responder's persistent `queue<T>` state field (fully
    /// resolved: `instance` is the bound test field). Returns the built
    /// `Expr::TransactorStateQueueQuery`, or `None` for a non-matching
    /// shape. A `.pop()` reaching here (nested deeper than a `let`/assign
    /// RHS) is rejected — it mutates and must be a statement. Mirrors
    /// `lower_scoreboard_query_call` for the test-scope target-state path.
    pub(crate) fn lower_test_state_queue_query(
        &self,
        callee: &AstExpr,
        args: &[crate::ast::CallArg],
    ) -> Result<Option<Expr>, LowerError> {
        let ExprKind::Field { target, name } = &*callee.kind else {
            return Ok(None);
        };
        // `target.<queue>` (or `_tb.xact.<queue>`): reuse the state-root
        // resolution — treat the receiver `target` as the state access `e`.
        let Some((instance, field, kind)) = self.as_transactor_state_any(target) else {
            return Ok(None);
        };
        if !matches!(kind, crate::ir::StateFieldKind::Queue { .. }) {
            return Ok(None);
        }
        let method = name.name.clone();
        let query = match method.as_str() {
            "size" => crate::ir::ScoreboardQuery::QueueSize {
                queue: field.clone(),
            },
            "empty" => crate::ir::ScoreboardQuery::QueueEmpty {
                queue: field.clone(),
            },
            "pop" => {
                return Err(unsupported(
                    &format!("target-state `{instance}.{field}.pop()` in a nested expression"),
                    "bind it to its own `let` first — `pop` mutates the queue",
                ));
            }
            other => {
                return Err(unsupported(
                    &format!("target-state queue method `{instance}.{field}.{other}(...)`"),
                    "only `push`/`pop`/`size`/`empty` are lowered",
                ));
            }
        };
        if !args.is_empty() {
            return Err(LowerError::Invalid(format!(
                "target-state `{instance}.{field}.{method}()` takes no arguments"
            )));
        }
        Ok(Some(Expr::TransactorStateQueueQuery {
            instance,
            field,
            query,
        }))
    }

    /// Like `as_transactor_state` but returns the field KIND too and does
    /// not filter on scalar-ness — the callers (`lower_test_state_queue_
    /// query`, the statement-level test-scope push/pop) select the kind.
    pub(crate) fn as_transactor_state_any(
        &self,
        e: &AstExpr,
    ) -> Option<(String, String, crate::ir::StateFieldKind)> {
        let ExprKind::Field { target, name } = &*e.kind else {
            return None;
        };
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
            .get(&name.name)
            .map(|kind| (instance, name.name.clone(), kind.clone()))
    }

    /// Resolve an AST field-access chain onto a SUB-FIELD of a bound-to
    /// target responder's whole-record state field. Handles all three
    /// access shapes uniformly:
    ///   * `last.addr` — a bare responder-body chain (the record field
    ///     `last` is in `self.target_state_fields`; the instance is a
    ///     placeholder filled at test-binding);
    ///   * `responder.last.addr` — a test-scope `let`-bound responder;
    ///   * `_tb.xact.last.addr` — an impl-form testbench transactor field.
    /// Returns `Ok(None)` when the chain is not a record-state subfield
    /// access (the caller falls through), or `Err` when it IS one but a
    /// segment names no record field / a non-leaf is not a nested record.
    /// The returned `path` is length ≥ 1 (a whole-record access, path
    /// empty, is handled by the scalar `TransactorState` lane).
    pub(crate) fn as_transactor_state_record_field(
        &self,
        e: &AstExpr,
    ) -> Result<Option<TransactorStateRecordChain>, LowerError> {
        let ExprKind::Field { .. } = &*e.kind else {
            return Ok(None);
        };
        // Flatten `a.b.c…` → root ident + segments (declaration order).
        let mut segs: Vec<String> = Vec::new();
        let mut cur = e;
        let root = loop {
            match &*cur.kind {
                ExprKind::Field { target, name } => {
                    segs.push(name.name.clone());
                    cur = target;
                }
                ExprKind::Ident(root) => break root,
                _ => return Ok(None),
            }
        };
        segs.reverse();
        // A local shadows a same-named state field / instance (the
        // established convention throughout this lowerer). Fall through
        // to the record-local field-chain lane in that case.
        if self.lookup(&root.name).is_some() {
            return Ok(None);
        }
        // Resolve (instance, state-field-name, remaining subfield segs).
        // Bare responder-body form: root IS the record state field, so
        // the instance is a placeholder. Otherwise root/`_tb`-prefix
        // names the bound test-scope instance and its state field.
        let (instance, state_field, sub) = if self.target_state_fields.contains_key(&root.name) {
            // `last.addr` — root is the state field, segs are the subfields.
            (String::new(), root.name.clone(), segs)
        } else {
            // Test-scope: `responder.last.addr` (instance=root) or
            // `_tb.xact.last.addr` (instance=segs[0]).
            let (instance, rest_start) =
                if Some(root.name.as_str()) == self.ctx.tb_field.as_deref() {
                    match segs.first() {
                        Some(mid) => (mid.clone(), 1usize),
                        None => return Ok(None),
                    }
                } else {
                    (root.name.clone(), 0usize)
                };
            if !self.ctx.target_state.contains_key(&instance) {
                return Ok(None);
            }
            let rest = &segs[rest_start..];
            let Some(state_field) = rest.first() else {
                return Ok(None);
            };
            (instance, state_field.clone(), rest[1..].to_vec())
        };
        // The named state field must exist and be a whole-record field.
        let kind = if instance.is_empty() {
            self.target_state_fields.get(&state_field)
        } else {
            match self.ctx.target_state.get(&instance) {
                Some(fields) => fields.get(&state_field),
                None => return Ok(None),
            }
        };
        let Some(crate::ir::StateFieldKind::Record { record }) = kind else {
            return Ok(None);
        };
        // A bare whole-record access (no subfield) is the scalar
        // `TransactorState` lane's job, not this one.
        if sub.is_empty() {
            return Ok(None);
        }
        // Type-check the subfield chain against the record schema,
        // descending through nested records to the leaf.
        let mut cur_rid = *record;
        let last = sub.len() - 1;
        let mut leaf_vec_len = None;
        for (i, seg) in sub.iter().enumerate() {
            let schema = &self.ctx.records[cur_rid.index()];
            let Some(fld) = schema.field(seg) else {
                return Err(LowerError::Invalid(format!(
                    "record `{}` has no field `{seg}`",
                    schema.name
                )));
            };
            if i == last {
                leaf_vec_len = fld.vec_len;
                break;
            }
            match fld.ty {
                IrType::Record(next) if fld.vec_len.is_none() => cur_rid = next,
                _ => {
                    return Err(unsupported(
                        &format!(
                            "field `{}.{seg}` is not a nested record; cannot access `.{}`",
                            schema.name,
                            sub[i + 1]
                        ),
                        "only nested struct/transaction fields can be traversed further",
                    ));
                }
            }
        }
        Ok(Some(TransactorStateRecordChain {
            instance,
            field: state_field,
            path: sub,
            leaf_vec_len,
        }))
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
    /// receiver width. Destinations through the language's 1024-bit
    /// width-method limit lower to the same `WidthCast` node; storage
    /// selection is a backend concern.
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
        if width > crate::MAX_WIDTH_METHOD_BITS {
            return Err(LowerError::Invalid(format!(
                "`.{kind_name}<{width}>()`: destination width exceeds the {}-bit \
                 language limit",
                crate::MAX_WIDTH_METHOD_BITS
            )));
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

    /// Wrap a lowered `+% / -% / *%` result to `max(W(lhs), W(rhs))` bits
    /// (harc#473). ARCH's wrapping operators take the wider operand's width
    /// as the result width with no widening; the mask is emitted as a
    /// `WidthCast::Trunc`, so codegen produces `(a OP b) & ((1<<W)-1)` for
    /// `W < 64` (and a no-op cast at `W == 64`, since 64 b fills the slot).
    ///
    /// Both operand widths must be statically determinable — literals are
    /// self-sized, typed locals / DUT ports / casts carry their width. If
    /// either operand's width is unknown, lowering fails loudly rather than
    /// silently degrading to the un-wrapped value (the exact hazard the
    /// operator exists to prevent): a scoreboard mirroring a wrapping
    /// datapath would otherwise compute values the DUT can never emit.
    fn wrap_to_operand_width(
        &self,
        op: BinaryOp,
        lhs: &AstExpr,
        rhs: &AstExpr,
        inner: Expr,
    ) -> Result<Expr, LowerError> {
        let sym = match op {
            BinaryOp::AddWrap => "+%",
            BinaryOp::SubWrap => "-%",
            BinaryOp::MulWrap => "*%",
            _ => "+%",
        };
        let wl = self.infer_wrap_operand_width(lhs);
        let wr = self.infer_wrap_operand_width(rhs);
        let (Some(wl), Some(wr)) = (wl, wr) else {
            return Err(LowerError::Invalid(format!(
                "wrapping operator `{sym}` needs both operands to have a statically \
                 known bit-width so the wrap width `max(W(lhs), W(rhs))` is defined \
                 (left is {}, right is {}). Give the operand(s) a scalar type \
                 (`let x : uint<N>`), a cast (`x as uint<N>`), or a width method.",
                if wl.is_some() { "known" } else { "unknown" },
                if wr.is_some() { "known" } else { "unknown" },
            )));
        };
        let width = wl.max(wr);
        if width > 64 {
            return Err(unsupported(
                &format!("wrapping operator `{sym}` at width {width} (> 64 bits)"),
                "wrapping arithmetic is lowered for operand widths up to 64 bits; \
                 wider datapaths need the `HarcWide<N>` model, which is not wired \
                 through the wrapping mask yet",
            ));
        }
        // `width == 0` can't occur: every determinable width is >= 1.
        Ok(Expr::WidthCast {
            kind: WidthCastKind::Trunc,
            width,
            src_width: None,
            inner: Box::new(inner),
        })
    }

    /// Operand bit-width for a wrapping-op mask. Like `infer_expr_width`
    /// but also resolves DUT/bus port reads (their declared width) and
    /// composes through nested wrapping ops (`(a +% b) *% c`), so a wrap
    /// chain masks at each step's own operand width. Kept separate from
    /// `infer_expr_width` so the `.trunc<N>()` direction check it feeds is
    /// unaffected.
    fn infer_wrap_operand_width(&self, e: &AstExpr) -> Option<u32> {
        match &*e.kind {
            ExprKind::Paren(inner) => self.infer_wrap_operand_width(inner),
            ExprKind::Binary { op, lhs, rhs }
                if matches!(
                    op,
                    BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap
                ) =>
            {
                let l = self.infer_wrap_operand_width(lhs)?;
                let r = self.infer_wrap_operand_width(rhs)?;
                Some(l.max(r))
            }
            // DUT port / bus-bound signal read carries its declared width.
            ExprKind::Field { .. } => self.as_port_ref(e).ok().flatten().and_then(|p| p.width),
            // Everything else shares the receiver-width inference (parens,
            // casts, width methods, literals, typed locals).
            _ => self.infer_expr_width(e),
        }
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
        BinaryOp::Add | BinaryOp::AddWrap => BinOp::Add,
        BinaryOp::Sub | BinaryOp::SubWrap => BinOp::Sub,
        BinaryOp::Mul | BinaryOp::MulWrap => BinOp::Mul,
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

/// Reject a LITERAL `Vec` element index that is statically out of range
/// (`tbl.entries[9]` on a `Vec<T, 4>` field). Without this, the access
/// lowers cleanly and both backends emit `std::array` UB at runtime (v1's
/// textual emission has the same hole), so this is `Invalid` — a
/// statically wrong program, NOT a subset gap — and must NOT suggest
/// `--codegen v1`. A non-literal index passes through unchanged (runtime
/// range behavior is the backends', as before).
pub(crate) fn check_literal_vec_index_bounds(
    dotted: &str,
    idx: &Expr,
    len: usize,
) -> Result<(), LowerError> {
    let Expr::Literal { value, .. } = idx else {
        return Ok(());
    };
    if (*value as u128) < len as u128 {
        return Ok(());
    }
    Err(LowerError::Invalid(format!(
        "element index {value} is out of range for `Vec` record field \
         `{dotted}` of length {len} (valid indices are 0..={})",
        len.saturating_sub(1)
    )))
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
        Expr::CovHookParam {
            index: Some(index), ..
        } => expr_has_transactor_edge(index),
        _ => false,
    }
}
