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
    RegAccess, RegFieldSchema, RegRegisterSchema, RegblockSchema, Stmt, UnOp,
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
        let width = reg.width.unwrap_or(default_w);
        if width == 0 || width > 64 {
            return Err(unsupported(
                &format!("regblock `{name}` register `{rname}` width {width}"),
                "the tbir value model is 64-bit; register widths must be 1..=64",
            ));
        }
        // Field-level decomposition: lower each `field N : T @ <pos>`
        // into its mask/shift metadata. The mirror stays whole-register
        // (one record field per register); field access is a masked
        // read-modify-write on that cell, mirroring v1's bit-slice
        // extract/insert.
        let mut reg_fields: Vec<RegFieldSchema> = Vec::new();
        for fld in &reg.fields {
            let fname = &fld.name.name;
            if reg_fields.iter().any(|x| x.name == *fname) {
                return Err(LowerError::Invalid(format!(
                    "regblock `{name}` register `{rname}` declares field `{fname}` more than once"
                )));
            }
            let bit_width = field_bit_width(&fld.ty);
            if bit_width == 0 {
                return Err(unsupported(
                    &format!(
                        "regblock `{name}` register `{rname}` field `{fname}` of zero width"
                    ),
                    "",
                ));
            }
            if fld.bit_pos as u64 + bit_width as u64 > width as u64 {
                return Err(LowerError::Invalid(format!(
                    "regblock `{name}` register `{rname}` field `{fname}` ([{}+:{bit_width}]) \
                     exceeds the register width {width}",
                    fld.bit_pos
                )));
            }
            reg_fields.push(RegFieldSchema {
                name: fname.clone(),
                bit_pos: fld.bit_pos,
                bit_width,
                access: lower_access(fld.access),
            });
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
            vec_len: None,
            default: reset,
            non_random: false,
            attr_src: Vec::new(),
        });
        registers.push(RegRegisterSchema {
            name: rname.clone(),
            offset,
            width,
            access,
            fields: reg_fields,
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

/// Field bit-width from its type, mirroring v1's `field_bit_width`:
/// `bit`/`bool` → 1; `uint<N>`/`sint<N>`/`bits<N>` → N; any other shape
/// → 1 (the conservative single-bit fallback v1 also uses, e.g. for an
/// unparameterized or enum-typed field). Width is folded only from a
/// plain integer literal type-arg — the corpus uses literals exclusively.
fn field_bit_width(t: &crate::ast::TypeExpr) -> u32 {
    use crate::ast::{BuiltinTy, TypeArg, TypeExpr};
    let TypeExpr::Builtin { name, args, .. } = t else {
        return 1;
    };
    match name {
        BuiltinTy::Bit | BuiltinTy::Bool | BuiltinTy::BoolLower => 1,
        BuiltinTy::UInt | BuiltinTy::SInt | BuiltinTy::Bits => match args.first() {
            Some(TypeArg::Expr(e)) => match &*e.kind {
                ExprKind::Int(s) => s.replace('_', "").parse::<u32>().unwrap_or(1),
                _ => 1,
            },
            _ => 1,
        },
        _ => 1,
    }
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
        self.lower_reg_write(local, &bctx.helper_field, &reg, value)
    }

    /// Shared register-level write lowering for the flat regblock
    /// (`regs.REG`) and addrmap (`chip.inst.REG`) paths. `mirror` is the
    /// whole-register mirror local; `reg.offset` is the effective bus
    /// offset.
    ///
    /// Lowering (RW/WO): mirror `RecordFieldWrite` then a discarded
    /// `Helper.write(off, value)` call edge. RO: mirror update only —
    /// the bus write is suppressed (v1's `ro` semantics).
    pub(crate) fn lower_reg_write(
        &mut self,
        mirror: ir::LocalId,
        helper_field: &str,
        reg: &RegRegisterSchema,
        value: &crate::ast::Expr,
    ) -> Result<bool, LowerError> {
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
            local: mirror,
            field: reg.name.clone(),
            index: None,
            value: v.clone(),
        });
        if writes_bus {
            let call = self.regblock_call(helper_field, "write", vec![lit(reg.offset), v])?;
            self.push(Stmt::TransactorCall { dest: None, call });
        }
        Ok(true)
    }

    /// `regs.REG.FIELD = value` field-level frontdoor write. Returns
    /// `Ok(true)` when `target` is a field write on a regblock binding
    /// (and lowers it), `Ok(false)` when `target` is not a regblock
    /// field access.
    ///
    /// Lowering mirrors v1's bit-slice insert: read-modify-write the
    /// whole-register mirror cell (mask out the FIELD bits, OR in the
    /// new value shifted to POS) then, for a bus-writing field, a
    /// full-register `Helper.write(off, mirror.REG)` (v1 writes the
    /// updated whole-register word, not the field). RO fields update the
    /// mirror only — the bus write is suppressed.
    pub(crate) fn try_lower_regblock_subfield_write(
        &mut self,
        target: &crate::ast::Expr,
        value: &crate::ast::Expr,
    ) -> Result<bool, LowerError> {
        let Some((binding, reg, fld)) = self.as_regblock_subfield(target) else {
            return Ok(false);
        };
        let bctx = self.ctx.regblock_bindings[&binding].clone();
        let Some(local) = self.lookup(&binding) else {
            return Err(LowerError::Invalid(format!(
                "regblock binding `{binding}` is not in scope at its field-write site"
            )));
        };
        self.lower_field_write(local, &bctx.helper_field, &reg, &fld, value)
    }

    /// Shared field-level write lowering for both the flat regblock
    /// (`regs.REG.FIELD`) and addrmap (`chip.inst.REG.FIELD`) paths.
    /// `mirror` is the whole-register mirror local; `offset` is the
    /// effective bus offset (regblock: reg offset; addrmap: base +
    /// reg offset).
    pub(crate) fn lower_field_write(
        &mut self,
        mirror: ir::LocalId,
        helper_field: &str,
        reg: &RegRegisterSchema,
        fld: &RegFieldSchema,
        value: &crate::ast::Expr,
    ) -> Result<bool, LowerError> {
        let v = self.lower_expr_no_ports(value)?;
        let mask = field_mask(fld.bit_width);
        let pos = fld.bit_pos as u64;
        let cur = Expr::RecordField {
            local: mirror,
            field: reg.name.clone(),
            index: None,
        };
        // (mirror.REG & ~(mask << pos)) | ((v & mask) << pos)
        let cleared = bin(
            BinOp::BitAnd,
            cur,
            Expr::Unary(UnOp::BitNot, Box::new(lit(mask << pos))),
        );
        let inserted = bin(
            BinOp::Shl,
            bin(BinOp::BitAnd, v, lit(mask)),
            lit(pos),
        );
        let new_word = bin(BinOp::BitOr, cleared, inserted);
        self.push(Stmt::RecordFieldWrite {
            local: mirror,
            field: reg.name.clone(),
            index: None,
            value: new_word,
        });
        if fld.access.writes_to_bus() {
            // Full-register bus write of the updated mirror word.
            let word = Expr::RecordField {
                local: mirror,
                field: reg.name.clone(),
                index: None,
            };
            let call = self.regblock_call(helper_field, "write", vec![lit(reg.offset), word])?;
            self.push(Stmt::TransactorCall { dest: None, call });
        }
        Ok(true)
    }

    /// `let x = regs.REG.FIELD` field-level frontdoor read, declaring a
    /// fresh local `dest_name`. Returns `Ok(true)` when `value` is a
    /// field read on a regblock binding.
    pub(crate) fn try_lower_regblock_subfield_read_let(
        &mut self,
        dest_name: &str,
        value: &crate::ast::Expr,
    ) -> Result<bool, LowerError> {
        let Some((binding, reg, fld)) = self.as_regblock_subfield(value) else {
            return Ok(false);
        };
        let bctx = self.ctx.regblock_bindings[&binding].clone();
        let Some(mirror) = self.lookup(&binding) else {
            return Err(LowerError::Invalid(format!(
                "regblock binding `{binding}` is not in scope at its field-read site"
            )));
        };
        let helper_ty = self.regblock_helper_type(&bctx.helper_field, "read", 1)?;
        let extract = self.field_read_expr(mirror, &helper_ty, &reg, &fld);
        let id = self.declare(dest_name);
        self.push(Stmt::Assign(id, extract));
        Ok(true)
    }

    /// Build the field-extract expression: `((mirror.REG = <H>_read(off))
    /// >> POS) & MASK` for a bus-reading field (one bus read + mirror
    /// predict, then bit-extract), or `(mirror.REG >> POS) & MASK` for a
    /// WO field (mirror-only). Mirrors v1's shifted read.
    pub(crate) fn field_read_expr(
        &self,
        mirror: ir::LocalId,
        helper_ty: &str,
        reg: &RegRegisterSchema,
        fld: &RegFieldSchema,
    ) -> Expr {
        let mask = field_mask(fld.bit_width);
        let pos = fld.bit_pos as u64;
        let word = if fld.access.reads_from_bus() {
            Expr::RegRead {
                mirror,
                helper_ty: helper_ty.to_string(),
                field: reg.name.clone(),
                offset: reg.offset,
                reads_bus: true,
            }
        } else {
            Expr::RecordField {
                local: mirror,
                field: reg.name.clone(),
                index: None,
            }
        };
        bin(BinOp::BitAnd, bin(BinOp::Shr, word, lit(pos)), lit(mask))
    }

    /// Lower a field read in a general EXPRESSION position (assert
    /// condition / format arg) — `regs.REG.FIELD` that is NOT a
    /// `let`-RHS. Returns the field-extract expression. The bus-read
    /// count matches v1: one read per textual occurrence.
    pub(crate) fn lower_regblock_subfield_read_expr(
        &self,
        binding: &str,
        reg_name: &str,
        fld_name: &str,
    ) -> Result<Expr, LowerError> {
        let bctx = &self.ctx.regblock_bindings[binding];
        let reg = bctx
            .registers
            .iter()
            .find(|r| r.name == reg_name)
            .expect("as_regblock_subfield validated the register");
        let fld = reg
            .fields
            .iter()
            .find(|f| f.name == fld_name)
            .expect("as_regblock_subfield validated the field")
            .clone();
        let Some(mirror) = self.lookup(binding) else {
            return Err(LowerError::Invalid(format!(
                "regblock binding `{binding}` is not in scope at its field-read site"
            )));
        };
        let helper_ty = self.regblock_helper_type(&bctx.helper_field, "read", 1)?;
        Ok(self.field_read_expr(mirror, &helper_ty, reg, &fld))
    }

    /// `Some((binding, register, field))` when `e` is a three-level
    /// `<binding>.<REG>.<FIELD>` access on a regblock binding where REG
    /// is a register and FIELD is one of its declared fields.
    pub(crate) fn as_regblock_subfield(
        &self,
        e: &crate::ast::Expr,
    ) -> Option<(String, RegRegisterSchema, RegFieldSchema)> {
        let ExprKind::Field {
            target: mid,
            name: fld_name,
        } = &*e.kind
        else {
            return None;
        };
        let ExprKind::Field {
            target: outer,
            name: reg_name,
        } = &*mid.kind
        else {
            return None;
        };
        let ExprKind::Ident(id) = &*outer.kind else {
            return None;
        };
        let bctx = self.ctx.regblock_bindings.get(&id.name)?;
        let reg = bctx.registers.iter().find(|r| r.name == reg_name.name)?;
        let fld = reg.fields.iter().find(|f| f.name == fld_name.name)?;
        Some((id.name.clone(), reg.clone(), fld.clone()))
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
        self.lower_reg_read_let(mirror, &bctx.helper_field, dest_name, &reg)?;
        Ok(true)
    }

    /// Shared register-level `let`-read lowering for the flat regblock
    /// (`regs.REG`) and addrmap (`chip.inst.REG`) paths. `mirror` is the
    /// whole-register mirror local; `reg.offset` is the effective bus
    /// offset.
    pub(crate) fn lower_reg_read_let(
        &mut self,
        mirror: ir::LocalId,
        helper_field: &str,
        dest_name: &str,
        reg: &RegRegisterSchema,
    ) -> Result<(), LowerError> {
        if reg.access.reads_from_bus() {
            let call = self.regblock_call(helper_field, "read", vec![lit(reg.offset)])?;
            let id = self.declare(dest_name);
            self.push(Stmt::TransactorCall {
                dest: Some(id),
                call,
            });
            // Read-side mirror predict: store the bus-returned value.
            self.push(Stmt::RecordFieldWrite {
                local: mirror,
                field: reg.name.clone(),
                index: None,
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
                    index: None,
                },
            ));
        }
        Ok(())
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
        Ok(self.reg_read_expr(mirror, &helper_ty, reg))
    }

    /// Shared register-level read-expression for the flat regblock
    /// (`regs.REG`) and addrmap (`chip.inst.REG`) paths. Returns v1's
    /// inline assignment-expression `RegRead`.
    pub(crate) fn reg_read_expr(
        &self,
        mirror: ir::LocalId,
        helper_ty: &str,
        reg: &RegRegisterSchema,
    ) -> Expr {
        Expr::RegRead {
            mirror,
            helper_ty: helper_ty.to_string(),
            field: reg.name.clone(),
            offset: reg.offset,
            reads_bus: reg.access.reads_from_bus(),
        }
    }

    /// Public accessor for the `via`-helper type resolution, shared with
    /// the addrmap path.
    pub(crate) fn regblock_helper_type_pub(
        &self,
        helper_field: &str,
        method: &str,
        arity: usize,
    ) -> Result<String, LowerError> {
        self.regblock_helper_type(helper_field, method, arity)
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

    /// `regs.record_write(addr, data)` — **passive** mirror update of an
    /// observed bus write (no bus traffic; the monitor already saw it).
    /// Returns `Ok(true)` when `e` is a `record_write` call on a regblock
    /// binding (and lowers it), `Ok(false)` when it is not.
    ///
    /// Lowering: with a compile-time-constant `addr`, decode it to the
    /// matching register at lowering time and emit a single masked mirror
    /// `RecordFieldWrite` (`mirror.REG = data & mask`). No callback
    /// dispatch — the per-register `on regs.REG` callback is a `[&]`-
    /// capturing closure over run-scope locals, which the function-per-
    /// CFG IR cannot express (rejected precisely upstream, like the
    /// `axilite_hooks` pre/post method hooks). A non-constant `addr` (a
    /// runtime decode chain) and an `addr` that matches no register are
    /// rejected precisely.
    pub(crate) fn try_lower_record_write(
        &mut self,
        e: &crate::ast::Expr,
    ) -> Result<bool, LowerError> {
        let Some((binding, name, args)) = self.as_record_api_call(e) else {
            return Ok(false);
        };
        if name != "record_write" {
            return Ok(false);
        }
        if args.len() != 2 {
            return Err(LowerError::Invalid(format!(
                "`{binding}.record_write` takes (addr, data) — got {} argument(s)",
                args.len()
            )));
        }
        let reg = self.resolve_record_api_reg(&binding, "record_write", call_arg(&args[0]))?;
        let Some(mirror) = self.lookup(&binding) else {
            return Err(LowerError::Invalid(format!(
                "regblock binding `{binding}` is not in scope at its record_write site"
            )));
        };
        // Mask the observed value to the register width before mirroring
        // (v1 stores `data & mask`), so a `record_read` of the same
        // address reflects the truncated cell.
        let v = self.lower_expr_no_ports(call_arg(&args[1]))?;
        let mask = if reg.width >= 64 { u64::MAX } else { (1u64 << reg.width) - 1 };
        let masked = bin(BinOp::BitAnd, v, lit(mask));
        self.push(Stmt::RecordFieldWrite {
            local: mirror,
            field: reg.name.clone(),
            index: None,
            value: masked,
        });
        Ok(true)
    }

    /// `let v = regs.record_read(addr)` — **passive** mirror read keyed
    /// by address (decode + mirror read, no bus). Returns `Ok(true)` when
    /// `value` is a `record_read` call on a regblock binding (and lowers
    /// it into `dest_name`), `Ok(false)` when it is not. A non-constant
    /// `addr` or an `addr` matching no register is rejected precisely.
    pub(crate) fn try_lower_record_read_let(
        &mut self,
        dest_name: &str,
        value: &crate::ast::Expr,
    ) -> Result<bool, LowerError> {
        let Some((binding, name, args)) = self.as_record_api_call(value) else {
            return Ok(false);
        };
        if name != "record_read" {
            return Ok(false);
        }
        if args.len() != 1 {
            return Err(LowerError::Invalid(format!(
                "`{binding}.record_read` takes (addr) — got {} argument(s)",
                args.len()
            )));
        }
        let reg = self.resolve_record_api_reg(&binding, "record_read", call_arg(&args[0]))?;
        let Some(mirror) = self.lookup(&binding) else {
            return Err(LowerError::Invalid(format!(
                "regblock binding `{binding}` is not in scope at its record_read site"
            )));
        };
        let id = self.declare(dest_name);
        self.push(Stmt::Assign(
            id,
            Expr::RecordField {
                local: mirror,
                field: reg.name.clone(),
                index: None,
            },
        ));
        Ok(true)
    }

    /// Decode a `record_*` address argument to its register. Requires a
    /// compile-time-constant address (literal / const) that matches a
    /// declared register offset; rejects a runtime address (the v1
    /// runtime decode chain is not lowered in this subset) and an
    /// unmatched address precisely.
    fn resolve_record_api_reg(
        &self,
        binding: &str,
        api: &str,
        addr: &crate::ast::Expr,
    ) -> Result<RegRegisterSchema, LowerError> {
        let Some(addr_val) = self.const_eval_index(addr) else {
            return Err(unsupported(
                &format!("`{binding}.{api}(...)` with a non-constant address"),
                "the passive record API decodes the address at compile time; \
                 pass a literal or const offset",
            ));
        };
        let bctx = &self.ctx.regblock_bindings[binding];
        bctx.registers
            .iter()
            .find(|r| r.offset == addr_val)
            .cloned()
            .ok_or_else(|| {
                LowerError::Invalid(format!(
                    "`{binding}.{api}(0x{addr_val:x}, ...)`: address matches no register \
                     offset in the regblock"
                ))
            })
    }

    /// `Some((binding, method, args))` when `e` is a `<binding>.<method>(
    /// args)` call on a regblock binding (`method` ∈ record API names).
    fn as_record_api_call<'a>(
        &self,
        e: &'a crate::ast::Expr,
    ) -> Option<(String, String, &'a [CallArg])> {
        let ExprKind::Call { callee, args } = &*e.kind else {
            return None;
        };
        let ExprKind::Field { target, name } = &*callee.kind else {
            return None;
        };
        let ExprKind::Ident(id) = &*target.kind else {
            return None;
        };
        if !self.is_regblock_binding(&id.name) {
            return None;
        }
        if name.name != "record_write" && name.name != "record_read" {
            return None;
        }
        Some((id.name.clone(), name.name.clone(), args.as_slice()))
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
            // `regs.REG.FIELD` — three-level field-level access. Reaches
            // here only when the register or the field is undeclared
            // (valid fields are lowered upstream).
            ExprKind::Field { target, name } if matches!(&*target.kind, ExprKind::Field { .. }) => {
                format!(
                    "field-level access `regs.<reg>.{}` (no such register/field)",
                    name.name
                )
            }
            ExprKind::Field { name, .. } => format!(
                "access to `{}.{}` (not a declared register)",
                binding, name.name
            ),
            _ => format!("a {ctx_label} on regblock binding `{binding}`"),
        };
        Err(unsupported(
            &format!("{detail} (regblock `{binding}`)"),
            "register-level `regs.NAME = v` writes/`regs.NAME` reads, field-level \
             `regs.REG.FIELD` writes/reads (incl. assert/format positions), and \
             `bitbash(regs)` are lowered; the `record_write`/`record_read` API and \
             per-register `on` callbacks are follow-up slices",
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

/// The positional expression of a call argument (`record_*` calls take
/// only positional args; a named arg's value is used the same way).
fn call_arg(a: &CallArg) -> &crate::ast::Expr {
    match a {
        CallArg::Expr(e) => e,
        CallArg::Named { value, .. } => value,
    }
}

fn bin(op: BinOp, a: Expr, b: Expr) -> Expr {
    Expr::Binary(op, Box::new(a), Box::new(b))
}

/// Right-aligned bit mask for a `width`-bit field. Mirrors v1's
/// `field_mask_literal`: clamped at 32 bits (Phase 1b fields cap at
/// register width 32, so a wider mask never arises in the corpus; the
/// 64-bit mirror makes the clamp value-identical for the fixtures).
fn field_mask(width: u32) -> u64 {
    if width >= 32 {
        0xFFFF_FFFF
    } else {
        (1u64 << width) - 1
    }
}

/// `Some(detail)` when `s` is a per-register `on regs.REG ... end on`
/// write callback rooted at one of `bindings` — the deferred slice.
///
/// The passive `record_write`/`record_read` API IS lowered (mirror-only,
/// constant-address decode), so it is no longer flagged here. The
/// callback, by contrast, lowers (in v1) to a `[&]`-capturing closure
/// over run-scope locals (the mirror cell, the callbacks holder, the
/// recursion-depth counter) that fires from inside `record_write`. The
/// function-per-CFG TB-IR cannot express a closure lexically nested in
/// the run coroutine capturing its locals — the SAME blocker as the
/// `axilite_hooks` pre/post method hooks — so it is rejected precisely.
/// `None` for any other statement (including in-subset register-level
/// reads/writes, `bitbash`, and the passive record API).
pub(crate) fn detect_regblock_residual(
    s: &crate::ast::Stmt,
    bindings: &std::collections::HashSet<&str>,
) -> Option<String> {
    use crate::ast::StmtKind;
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
        _ => None,
    }
}
