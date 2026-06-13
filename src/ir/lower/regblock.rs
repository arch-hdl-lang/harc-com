//! `regblock` declaration lowering → a synthetic mirror `RecordSchema`
//! plus a `RegblockSchema`, and the register-level frontdoor access
//! lowering (`regs.NAME = v` / `let x = regs.NAME`).
//!
//! Subset (docs/tbir-mvp.md §regblock): the **register-level
//! frontdoor**. A `regblock R via <Helper> [width N]` whose `via`
//! helper is an unbound DUT-poking transactor (testbench field), with
//! single-line `register NAME @ ADDR [reset V] access rw|ro|wo`
//! registers, accessed register-level:
//!
//! ```text
//! regs.NAME = v       // mirror update + Helper.write(off, v)   (RW/WO)
//! let x = regs.NAME   // Helper.read(off) + mirror predict      (RW/RO)
//! ```
//!
//! The mirror is a synthetic value-record (one scalar field per
//! register, defaulting to its reset value), so the existing
//! `IrType::Record` / `RecordInit` / `RecordFieldWrite` /
//! `Expr::RecordField` machinery carries the host-side state with no new
//! IR variants — exactly the shape v1's `<Name>_Mirror` POD struct holds.
//! Frontdoor traffic lowers to the existing `Stmt::TransactorCall`
//! (`CallTarget::TransactorMethod { bus_field: helper, method }`) edge,
//! the same call edge bus `tlm_method`s and transactor-field calls use.
//!
//! Also lowered (later slices, all matching v1's bus semantics):
//!   * **bus-bound `via` helper** (`transactor H bound to BusT`): the
//!     corpus `regblock_*` fixtures use this form; its `hookable`
//!     bodies drive the bound bus's handshake channels (initiator BFM).
//!   * **register reads outside `let`-RHS** (assert conditions,
//!     `log`/`fail` format args): lower to `Expr::RegRead`, v1's inline
//!     assignment-expression — one bus read per textual occurrence
//!     (eager in conditions, lazy in fail messages). The `via` helper's
//!     `read` is a plain hookable lambda (not the TLM seam), so it is a
//!     legitimate sub-expression value.
//!   * **`bitbash(regs)`**: compile-time-unrolled walk-all over the RW
//!     registers (write/read both patterns + compare; RO/WO skipped).
//!
//! Out of subset — explicit `Unsupported`, never silent mis-lowering:
//!   * field-level decomposition (`regs.REG.FIELD`),
//!   * the passive `record_write`/`record_read` API and per-register
//!     `on regs.REG` write callbacks (see `detect_regblock_residual`),
//!   * `addrmap` composition (incl. `alias of`).

use super::{LowerError, unsupported};
use crate::ast::{CallArg, ExprKind, RegAccess as AstRegAccess, RegblockDecl};
use crate::ir::{
    self, BinOp, Expr, FmtArg, FmtArgs, IrType, RecordFieldSchema, RecordId, RecordSchema,
    RegAccess, RegRegisterSchema, RegblockSchema, Stmt,
};

/// Per-binding context for register access resolution. Built at the
/// `let regs : R = bind <helper>` site and carried in `LowerCtx`.
#[derive(Debug, Clone)]
pub(crate) struct RegblockBindingCtx {
    /// Mirror record (the synthetic `RecordSchema`).
    pub record: RecordId,
    /// Transactor instance field the frontdoor `write`/`read` route
    /// through (a testbench transactor field).
    pub helper_field: String,
    /// Registers in declaration order (offset / width / access).
    pub registers: Vec<RegRegisterSchema>,
}

/// Lower one `regblock` declaration into its synthetic mirror record
/// plus a `RegblockSchema`. The caller pushes the record into
/// `TbProgram::records` (id `record`) and the schema into
/// `TbProgram::regblocks`.
pub(crate) fn lower_regblock(
    r: &RegblockDecl,
    record: RecordId,
) -> Result<(RecordSchema, RegblockSchema), LowerError> {
    let name = &r.name.name;
    let default_w = r.default_width.unwrap_or(32);
    let mut fields: Vec<RecordFieldSchema> = Vec::new();
    let mut registers: Vec<RegRegisterSchema> = Vec::new();
    for reg in &r.registers {
        let rname = &reg.name.name;
        if registers.iter().any(|x| x.name == *rname) {
            return Err(LowerError::Invalid(format!(
                "regblock `{name}` declares register `{rname}` more than once"
            )));
        }
        if !reg.fields.is_empty() {
            return Err(unsupported(
                &format!("field-level decomposition in regblock `{name}` register `{rname}`"),
                "only register-level access (`regs.NAME`) is lowered",
            ));
        }
        let width = reg.width.unwrap_or(default_w);
        if width == 0 || width > 64 {
            return Err(unsupported(
                &format!("regblock `{name}` register `{rname}` width {width}"),
                "the tbir value model is 64-bit; register widths must be 1..=64",
            ));
        }
        let offset = fold_offset(name, rname, &reg.offset)?;
        let reset = match &reg.reset {
            None => None,
            Some(rv) => match &*rv.kind {
                ExprKind::Int(s) => Some(super::exprs::parse_int_literal(s).ok_or_else(|| {
                    unsupported(
                        &format!("regblock `{name}` register `{rname}` reset value `{s}`"),
                        "not a plain integer literal",
                    )
                })?),
                ExprKind::Bool(b) => Some(*b as u64),
                _ => {
                    return Err(unsupported(
                        &format!("a non-literal reset value on regblock `{name}` register `{rname}`"),
                        "",
                    ));
                }
            },
        };
        let access = lower_access(reg.access);
        fields.push(RecordFieldSchema {
            name: rname.clone(),
            ty: IrType::UInt(Some(width)),
            default: reset,
            non_random: false,
            attr_src: Vec::new(),
        });
        registers.push(RegRegisterSchema {
            name: rname.clone(),
            offset,
            width,
            access,
        });
    }
    if registers.is_empty() {
        return Err(LowerError::Invalid(format!(
            "regblock `{name}` declares no registers"
        )));
    }
    let record_schema = RecordSchema {
        name: name.clone(),
        fields,
        keeps: Vec::new(),
    };
    let schema = RegblockSchema {
        name: name.clone(),
        record,
        registers,
    };
    Ok((record_schema, schema))
}

fn lower_access(a: AstRegAccess) -> RegAccess {
    match a {
        AstRegAccess::Rw => RegAccess::Rw,
        AstRegAccess::Ro => RegAccess::Ro,
        AstRegAccess::Wo => RegAccess::Wo,
    }
}

/// Fold a register's `@ <addr>` offset to a constant. Only plain integer
/// literals are lowered (v1 const-folds arbitrary expressions; the
/// corpus uses literals exclusively).
fn fold_offset(
    block: &str,
    reg: &str,
    e: &crate::ast::Expr,
) -> Result<u64, LowerError> {
    match &*e.kind {
        ExprKind::Int(s) => super::exprs::parse_int_literal(s).ok_or_else(|| {
            unsupported(
                &format!("regblock `{block}` register `{reg}` offset `{s}`"),
                "not a plain integer literal",
            )
        }),
        _ => Err(unsupported(
            &format!("a non-literal `@ <addr>` offset on regblock `{block}` register `{reg}`"),
            "",
        )),
    }
}

impl super::FuncBuilder<'_> {
    /// `regs.NAME = value` register-level frontdoor write. Returns
    /// `Ok(true)` when `target` is a register write on a regblock
    /// binding (and lowers it), `Ok(false)` when `target` is not a
    /// regblock access at all.
    ///
    /// Lowering (RW/WO): mirror `RecordFieldWrite` then a discarded
    /// `Helper.write(off, value)` call edge. RO: mirror update only —
    /// the bus write is suppressed (v1's `ro` semantics).
    pub(crate) fn try_lower_regblock_write(
        &mut self,
        target: &crate::ast::Expr,
        value: &crate::ast::Expr,
    ) -> Result<bool, LowerError> {
        let Some((binding, reg)) = self.as_regblock_register(target) else {
            return Ok(false);
        };
        let bctx = self.ctx.regblock_bindings[&binding].clone();
        let reg = bctx
            .registers
            .iter()
            .find(|r| r.name == reg)
            .expect("as_regblock_register validated the register")
            .clone();
        let Some(local) = self.lookup(&binding) else {
            return Err(LowerError::Invalid(format!(
                "regblock binding `{binding}` is not in scope at its write site"
            )));
        };
        // Lower the value ONCE (port-hoisted like Assign), into a temp
        // when needed so both the mirror update and the frontdoor write
        // observe the same value. v1 emits the user expression at both
        // sites; evaluating it once is observably identical (the IR's
        // DUT reads are side-effect-free, and there is no tick between
        // the two uses) and avoids re-hoisting a DUT read twice.
        let v = self.lower_expr_no_ports(value)?;
        let writes_bus = reg.access.writes_to_bus();
        let v = if writes_bus && !matches!(v, Expr::Local(_) | Expr::Literal { .. }) {
            // Bind to a temp so the same value feeds both uses.
            let t = self.fresh_temp();
            self.push(Stmt::Assign(t, v));
            Expr::Local(t)
        } else {
            v
        };
        self.push(Stmt::RecordFieldWrite {
            local,
            field: reg.name.clone(),
            value: v.clone(),
        });
        if writes_bus {
            let call = self.regblock_call(&bctx.helper_field, "write", vec![lit(reg.offset), v])?;
            self.push(Stmt::TransactorCall { dest: None, call });
        }
        Ok(true)
    }

    /// `bitbash(regs)` — compile-time-unrolled walk-all over the
    /// regblock's RW registers. For each RW register and each of the two
    /// patterns (all-ones masked to width, then zero), emit
    /// `Helper.write(off, pat)`, `let got = Helper.read(off)`, and an
    /// `assert got == pat else fail("bitbash …")`. RO/WO registers are
    /// skipped (RO can't accept the write; WO reads are mirror-only) —
    /// matching v1's `try_emit_bitbash`. Returns `Ok(false)` when `e` is
    /// not a `bitbash(<regblock-binding>)` call.
    pub(crate) fn try_lower_bitbash(
        &mut self,
        e: &crate::ast::Expr,
    ) -> Result<bool, LowerError> {
        let ExprKind::Call { callee, args } = &*e.kind else {
            return Ok(false);
        };
        let ExprKind::Ident(name) = &*callee.kind else {
            return Ok(false);
        };
        if name.name != "bitbash" {
            return Ok(false);
        }
        if args.len() != 1 {
            return Err(LowerError::Invalid(
                "bitbash(regs) takes exactly one argument (the regblock binding)".to_string(),
            ));
        }
        let arg = match &args[0] {
            CallArg::Expr(ex) => ex,
            CallArg::Named { value, .. } => value,
        };
        let ExprKind::Ident(regs_id) = &*arg.kind else {
            return Err(unsupported(
                "bitbash(<expr>) over a non-identifier argument",
                "pass the regblock binding directly: `bitbash(regs)`",
            ));
        };
        if !self.is_regblock_binding(&regs_id.name) {
            // Not a regblock binding — let the generic statement path
            // produce its own (more apt) diagnostic.
            return Ok(false);
        }
        let binding = regs_id.name.clone();
        let bctx = self.ctx.regblock_bindings[&binding].clone();
        for reg in &bctx.registers {
            // Skip non-RW registers exactly like v1 (RW is the only
            // policy where both the write and the read reach the bus).
            if !reg.access.writes_to_bus() || !reg.access.reads_from_bus() {
                continue;
            }
            let mask: u64 = if reg.width >= 64 {
                u64::MAX
            } else {
                (1u64 << reg.width) - 1
            };
            for (pat_label, pat) in [("ones", mask), ("zero", 0u64)] {
                // Helper.write(off, pat)
                let wcall =
                    self.regblock_call(&bctx.helper_field, "write", vec![lit(reg.offset), lit(pat)])?;
                self.push(Stmt::TransactorCall { dest: None, call: wcall });
                // got = Helper.read(off)
                let rcall = self.regblock_call(&bctx.helper_field, "read", vec![lit(reg.offset)])?;
                let got = self.fresh_temp();
                self.push(Stmt::TransactorCall { dest: Some(got), call: rcall });
                // assert got == pat else fail("bitbash REG label: wrote 0x.., got 0x..")
                let cond = Expr::Binary(BinOp::Eq, Box::new(Expr::Local(got)), Box::new(lit(pat)));
                // v1's exact message: `bitbash <reg> <label>: wrote
                // 0x%llx, got 0x%llx` with long-long hex args. The
                // non-wide-hex (`harc_printf_ll`) arg path is the
                // long-long ABI v1's `(long long)` cast also uses, so
                // the rendered text is byte-identical across backends.
                let on_fail = FmtArgs {
                    fmt: format!(
                        "bitbash {} {pat_label}: wrote 0x%llx, got 0x%llx",
                        reg.name
                    ),
                    args: vec![
                        FmtArg { expr: lit(pat), wide_hex: None },
                        FmtArg { expr: Expr::Local(got), wide_hex: None },
                    ],
                };
                self.push(Stmt::AssertCheck { cond, on_fail });
            }
        }
        Ok(true)
    }

    /// `let x = regs.NAME` register-level frontdoor read, declaring a
    /// fresh local `dest_name`. Returns `Ok(true)` when `value` is a
    /// register read on a regblock binding (and lowers it).
    ///
    /// Lowering (RW/RO): a `Helper.read(off)` call edge into the new
    /// local, then a mirror predict `RecordFieldWrite` so a later
    /// mirror-only access sees the bus-returned value (v1's read-side
    /// predict). WO: mirror read only (no bus traffic).
    pub(crate) fn try_lower_regblock_read_let(
        &mut self,
        dest_name: &str,
        value: &crate::ast::Expr,
    ) -> Result<bool, LowerError> {
        let Some((binding, reg)) = self.as_regblock_register(value) else {
            return Ok(false);
        };
        let bctx = self.ctx.regblock_bindings[&binding].clone();
        let reg = bctx
            .registers
            .iter()
            .find(|r| r.name == reg)
            .expect("as_regblock_register validated the register")
            .clone();
        let Some(mirror) = self.lookup(&binding) else {
            return Err(LowerError::Invalid(format!(
                "regblock binding `{binding}` is not in scope at its read site"
            )));
        };
        if reg.access.reads_from_bus() {
            let call = self.regblock_call(&bctx.helper_field, "read", vec![lit(reg.offset)])?;
            let id = self.declare(dest_name);
            self.push(Stmt::TransactorCall {
                dest: Some(id),
                call,
            });
            // Read-side mirror predict: store the bus-returned value.
            self.push(Stmt::RecordFieldWrite {
                local: mirror,
                field: reg.name.clone(),
                value: Expr::Local(id),
            });
        } else {
            // WO — serve from the mirror without bus traffic.
            let id = self.declare(dest_name);
            self.push(Stmt::Assign(
                id,
                Expr::RecordField {
                    local: mirror,
                    field: reg.name.clone(),
                },
            ));
        }
        Ok(true)
    }

    /// Lower a register read in a general EXPRESSION position (an assert
    /// condition, a `log`/`fail` format arg) — `regs.NAME` that is NOT a
    /// `let`-RHS. Returns an `Expr::RegRead` that emits v1's inline
    /// assignment-expression: RW/RO fires the bus read and predicts the
    /// mirror in one expression; WO serves from the mirror cell.
    ///
    /// The bus-read count matches v1 exactly: one read per textual
    /// occurrence, fired eagerly in conditions and lazily in fail
    /// messages (which both backends emit inside the `if (!cond)`
    /// branch). No statement hoist is needed because the `via` helper's
    /// `read` is an ordinary hookable lambda, not the bus wire protocol.
    pub(crate) fn lower_regblock_read_expr(
        &self,
        binding: &str,
        reg_name: &str,
    ) -> Result<Expr, LowerError> {
        let bctx = &self.ctx.regblock_bindings[binding];
        let reg = bctx
            .registers
            .iter()
            .find(|r| r.name == reg_name)
            .expect("as_regblock_register validated the register");
        let Some(mirror) = self.lookup(binding) else {
            return Err(LowerError::Invalid(format!(
                "regblock binding `{binding}` is not in scope at its read site"
            )));
        };
        // Resolve the helper TYPE name (the emitted lambda is
        // `<helper_ty>_read`) and validate the `read` method/arity, the
        // same way `regblock_call` does for the statement paths.
        let helper_ty = self.regblock_helper_type(&bctx.helper_field, "read", 1)?;
        Ok(Expr::RegRead {
            mirror,
            helper_ty,
            field: reg.name.clone(),
            offset: reg.offset,
            reads_bus: reg.access.reads_from_bus(),
        })
    }

    /// Resolve a regblock `via` helper field to its transactor TYPE name,
    /// validating the named method exists at `arity`. Shared by the
    /// statement call-edge path and the expression-position `RegRead`.
    fn regblock_helper_type(
        &self,
        helper_field: &str,
        method: &str,
        arity: usize,
    ) -> Result<String, LowerError> {
        let Some(&xid) = self.ctx.transactor_fields.get(helper_field) else {
            return Err(LowerError::Invalid(format!(
                "regblock `via` helper `{helper_field}` is not an active transactor field"
            )));
        };
        let schema = &self.ctx.transactors[xid.index()];
        let Some(m) = schema.method(method) else {
            return Err(LowerError::Invalid(format!(
                "regblock `via` helper `{}` has no `{method}` method",
                schema.name
            )));
        };
        if m.n_params != arity {
            return Err(LowerError::Invalid(format!(
                "regblock `via` helper `{}` method `{method}` takes {} argument(s), \
                 the frontdoor passes {arity}",
                schema.name, m.n_params,
            )));
        }
        Ok(schema.name.clone())
    }

    /// `Some((binding, register))` when `e` is a two-level
    /// `<binding>.<REG>` access where `<binding>` is a regblock binding
    /// and `<REG>` is one of its registers. A binding access to an
    /// unknown register, or a deeper chain (`regs.REG.FIELD` — field
    /// access, out of subset), is rejected by the callers that need it;
    /// here we only recognize the in-subset register shape.
    pub(crate) fn as_regblock_register(
        &self,
        e: &crate::ast::Expr,
    ) -> Option<(String, String)> {
        let ExprKind::Field { target, name } = &*e.kind else {
            return None;
        };
        let ExprKind::Ident(id) = &*target.kind else {
            return None;
        };
        let bctx = self.ctx.regblock_bindings.get(&id.name)?;
        bctx.registers
            .iter()
            .any(|r| r.name == name.name)
            .then(|| (id.name.clone(), name.name.clone()))
    }

    /// `true` when `id` names a regblock binding in scope — used by
    /// callers to recognize the in-subset register-level read/write and
    /// `bitbash` shapes, and to give a precise rejection for the
    /// remaining out-of-subset shapes (field-level access, the passive
    /// `record_*` API) before falling through to a generic error.
    pub(crate) fn is_regblock_binding(&self, name: &str) -> bool {
        self.ctx.regblock_bindings.contains_key(name)
    }

    /// The root binding name of any access chain rooted at a regblock
    /// binding (`regs`, `regs.REG`, `regs.REG.FIELD`,
    /// `regs.bitbash(...)`, `regs.record_write(...)`), if any. Used to
    /// give a precise out-of-subset rejection.
    fn regblock_access_root(&self, e: &crate::ast::Expr) -> Option<String> {
        let mut cur = e;
        loop {
            match &*cur.kind {
                ExprKind::Field { target, .. } => cur = target,
                ExprKind::Call { callee, .. } => cur = callee,
                ExprKind::Index { target, .. } => cur = target,
                ExprKind::Paren(inner) => cur = inner,
                ExprKind::Ident(id) => {
                    return self.is_regblock_binding(&id.name).then(|| id.name.clone());
                }
                _ => return None,
            }
        }
    }

    /// Reject an out-of-subset access on a regblock binding with a
    /// precise message naming the deferred feature. Called after the
    /// in-subset register-level read/write paths (and `bitbash`) have
    /// declined `e`, so reaching here means `e` is rooted at a regblock
    /// binding but is NOT an in-subset access: a field-level
    /// `regs.REG.FIELD`, an unknown register, or the passive `record_*`
    /// API. (A plain register read outside `let`-RHS is now lowered to
    /// `Expr::RegRead` upstream and never reaches here.)
    /// A no-op (`Ok(())`) when `e` is not a regblock access at all.
    pub(crate) fn reject_out_of_subset_regblock_access(
        &self,
        e: &crate::ast::Expr,
        ctx_label: &str,
    ) -> Result<(), LowerError> {
        let Some(binding) = self.regblock_access_root(e) else {
            return Ok(());
        };
        // Distinguish the common shapes for an actionable message.
        let detail = match &*e.kind {
            ExprKind::Call { callee, .. } => match &*callee.kind {
                ExprKind::Field { name, .. } => format!(
                    "passive `record_*`/method call `.{}(...)` on a regblock binding",
                    name.name
                ),
                ExprKind::Ident(id) => {
                    format!("`{}(regs)` walk on a regblock binding", id.name)
                }
                _ => "a call on a regblock binding".to_string(),
            },
            // `regs.REG.FIELD` — three-level field-level access.
            ExprKind::Field { target, name } if matches!(&*target.kind, ExprKind::Field { .. }) => {
                format!("field-level access `regs.<reg>.{}`", name.name)
            }
            ExprKind::Field { name, .. } => format!(
                "access to `{}.{}` (not a declared register)",
                binding, name.name
            ),
            _ => format!("a {ctx_label} on regblock binding `{binding}`"),
        };
        Err(unsupported(
            &format!("{detail} (regblock `{binding}`)"),
            "register-level `regs.NAME = v` writes, `regs.NAME` reads (incl. assert/format \
             positions), and `bitbash(regs)` are lowered; field-level access, the \
             `record_write`/`record_read` API, per-register `on` callbacks, and `addrmap` \
             composition are follow-up slices",
        ))
    }

    /// Build the frontdoor `Helper.<method>(args)` call edge. Validates
    /// the helper transactor declares the method at the expected arity —
    /// v1 would surface a missing method as a C++ compile error.
    fn regblock_call(
        &self,
        helper_field: &str,
        method: &str,
        args: Vec<Expr>,
    ) -> Result<Expr, LowerError> {
        let Some(&xid) = self.ctx.transactor_fields.get(helper_field) else {
            return Err(LowerError::Invalid(format!(
                "regblock `via` helper `{helper_field}` is not an active transactor field"
            )));
        };
        let schema = &self.ctx.transactors[xid.index()];
        let Some(m) = schema.method(method) else {
            return Err(LowerError::Invalid(format!(
                "regblock `via` helper `{}` has no `{method}` method",
                schema.name
            )));
        };
        if m.n_params != args.len() {
            return Err(LowerError::Invalid(format!(
                "regblock `via` helper `{}` method `{method}` takes {} argument(s), \
                 the frontdoor passes {}",
                schema.name,
                m.n_params,
                args.len()
            )));
        }
        Ok(Expr::Call(
            ir::CallTarget::TransactorMethod {
                bus_field: helper_field.to_string(),
                method: method.to_string(),
            },
            args,
        ))
    }
}

fn lit(v: u64) -> Expr {
    Expr::Literal {
        value: v,
        ty: IrType::Unknown,
    }
}

/// `Some(detail)` when `s` is a regblock record-API / per-register
/// callback residual rooted at one of `bindings` — the deferred slice.
/// Distinguishes the two shapes for an actionable message:
///   * `on regs.REG ... end on` — a per-register write callback.
///   * `regs.record_write(...)` / `let v = regs.record_read(...)` — the
///     passive record API.
/// `None` for any other statement (including in-subset register-level
/// reads/writes and `bitbash`).
pub(crate) fn detect_regblock_residual(
    s: &crate::ast::Stmt,
    bindings: &std::collections::HashSet<&str>,
) -> Option<String> {
    use crate::ast::StmtKind;
    // `regs.record_write(...)` / `regs.record_read(...)` call on a
    // regblock binding (in any expression — statement or `let`-RHS).
    fn record_api_call(e: &crate::ast::Expr, bindings: &std::collections::HashSet<&str>) -> Option<String> {
        let ExprKind::Call { callee, .. } = &*e.kind else {
            return None;
        };
        let ExprKind::Field { target, name } = &*callee.kind else {
            return None;
        };
        let ExprKind::Ident(id) = &*target.kind else {
            return None;
        };
        if !bindings.contains(id.name.as_str()) {
            return None;
        }
        (name.name == "record_write" || name.name == "record_read")
            .then(|| format!("the passive `{}.{}(...)` API", id.name, name.name))
    }
    match &s.kind {
        // `on regs.REG ... end on` — event is the `regs.REG` access.
        StmtKind::On(h) => {
            if let ExprKind::Field { target, name } = &*h.event.kind {
                if let ExprKind::Ident(id) = &*target.kind {
                    if bindings.contains(id.name.as_str()) {
                        return Some(format!(
                            "a per-register write callback `on {}.{}`",
                            id.name, name.name
                        ));
                    }
                }
            }
            None
        }
        StmtKind::Expr(e) => record_api_call(e, bindings),
        StmtKind::Let(l) => l.value.as_ref().and_then(|v| record_api_call(v, bindings)),
        _ => None,
    }
}
