//! `addrmap` declaration lowering — the chip-level container that
//! composes one or more `regblock`s at distinct base addresses
//! (docs/ral-support.md §4).
//!
//! Subset (docs/tbir-mvp.md §regblock, addrmap residual): each
//! `instance NAME : R @ BASE [size S] [alias of OTHER]` is modeled as
//! its own whole-register mirror local (type = the regblock's synthetic
//! mirror record), with a per-instance register table whose offsets are
//! pre-shifted to the effective bus address `base(inst) + offset(REG)`.
//! This reuses the flat-regblock register/field access lowering verbatim
//! (`lower_reg_write` / `lower_reg_read_let` / `reg_read_expr` /
//! `lower_field_write` / `field_read_expr`) — the only addrmap-specific
//! work is resolving the 3-/4-level access path
//! (`chip.inst.REG[.FIELD]`) to the right mirror local + shifted table.
//!
//! `alias of OTHER`: the alias instance shares OTHER's mirror local (one
//! storage cell across windows, matching v1's "shares mirror" comment)
//! while keeping its OWN base for bus traffic — so a write through the
//! alias lands on the alias's bus address but moves the shared mirror.
//!
//! Validation mirrors v1's `check_addrmap_aliases` / `check_addrmap_
//! overlap`: alias targets must exist, must not themselves be aliases
//! (no chains), must reference the same regblock type, and sized windows
//! must not overlap.

use std::collections::HashMap;

use super::{unsupported, LowerError};
use crate::ast::{AddrmapDecl, ExprKind};
use crate::ir::{Expr, RecordId, RegRegisterSchema};

/// Per-instance access context within an addrmap binding.
#[derive(Debug, Clone)]
pub(crate) struct AddrmapInstanceCtx {
    /// Source instance name (`mm2s`).
    pub name: String,
    /// Mangled name of the mirror local this instance reads/writes
    /// through. For a plain instance this is its own mangled name; for
    /// an `alias of T` instance it is T's mangled name (shared cell).
    pub mirror_key: String,
    /// Registers with offsets pre-shifted to the effective bus address
    /// (`base + reg_off`). Fields keep their bit positions.
    pub registers: Vec<RegRegisterSchema>,
}

/// Per-binding context for `let chip : A = bind <helper>` access
/// resolution. Carried in `LowerCtx`.
#[derive(Debug, Clone)]
pub(crate) struct AddrmapBindingCtx {
    /// Transactor instance field the frontdoor `write`/`read` route
    /// through (the addrmap's `via` helper, bound at the `let` site).
    pub helper_field: String,
    /// Instances in declaration order.
    pub instances: Vec<AddrmapInstanceCtx>,
    /// Distinct mirror locals to declare + default-construct at the head
    /// of the Run function: `(mangled_local_name, record_id)`. Aliased
    /// instances are absent (they share their target's local).
    pub mirror_inits: Vec<(String, RecordId)>,
}

/// Mangle a `chip.inst` pair into a dotless local name usable as a C++
/// identifier. The binding name and instance name are both source-level
/// identifiers, so the join is unambiguous.
pub(crate) fn instance_mirror_key(chip: &str, inst: &str) -> String {
    format!("__addrmap_{chip}_{inst}")
}

/// Build the per-binding context for `let chip : A = bind <helper>`.
/// `record_of` resolves a regblock type name to its synthetic mirror
/// `RecordId`; `regblock_registers` resolves a regblock type name to its
/// register table. Both are validated up front so a malformed addrmap
/// fails at lowering, not at C++ compile.
pub(crate) fn build_binding_ctx(
    chip: &str,
    decl: &AddrmapDecl,
    helper_field: &str,
    record_of: &HashMap<String, RecordId>,
    regblock_registers: &HashMap<String, Vec<RegRegisterSchema>>,
) -> Result<AddrmapBindingCtx, LowerError> {
    // First pass: resolve each instance's base, regblock type, register
    // table, and validate alias targets.
    let mut bases: HashMap<String, (u64, Option<u64>, String)> = HashMap::new();
    let mut order: Vec<&crate::ast::InstanceDecl> = Vec::new();
    for inst in &decl.instances {
        if bases.contains_key(&inst.name.name) {
            return Err(LowerError::Invalid(format!(
                "addrmap `{chip}` declares instance `{}` more than once",
                inst.name.name
            )));
        }
        let base = fold_const(&inst.base_addr).ok_or_else(|| {
            unsupported(
                &format!(
                    "a non-literal `@ <base>` on addrmap `{chip}` instance `{}`",
                    inst.name.name
                ),
                "only plain integer literals are lowered",
            )
        })?;
        let size = match &inst.size {
            None => None,
            Some(e) => Some(fold_const(e).ok_or_else(|| {
                unsupported(
                    &format!(
                        "a non-literal `size` on addrmap `{chip}` instance `{}`",
                        inst.name.name
                    ),
                    "only plain integer literals are lowered",
                )
            })?),
        };
        if !regblock_registers.contains_key(&inst.regblock_ty.name) {
            return Err(LowerError::Invalid(format!(
                "addrmap `{chip}` instance `{}` references unknown regblock `{}`",
                inst.name.name, inst.regblock_ty.name
            )));
        }
        bases.insert(
            inst.name.name.clone(),
            (base, size, inst.regblock_ty.name.clone()),
        );
        order.push(inst);
    }

    // Validate aliases (mirrors v1 `check_addrmap_aliases`): target must
    // exist, must not be an alias itself, and must reference the same
    // regblock type.
    for inst in &order {
        if let Some(t) = &inst.alias_of {
            let Some((_, _, tty)) = bases.get(&t.name) else {
                return Err(LowerError::Invalid(format!(
                    "addrmap `{chip}`: instance `{}` aliases `{}`, but no such instance exists",
                    inst.name.name, t.name
                )));
            };
            let target = order
                .iter()
                .find(|i| i.name.name == t.name)
                .expect("base entry implies an order entry");
            if target.alias_of.is_some() {
                return Err(LowerError::Invalid(format!(
                    "addrmap `{chip}`: instance `{}` aliases `{}`, which is itself an alias — \
                     chained aliases are not supported",
                    inst.name.name, t.name
                )));
            }
            if *tty != inst.regblock_ty.name {
                return Err(LowerError::Invalid(format!(
                    "addrmap `{chip}`: instance `{}` (type `{}`) aliases `{}` (type `{tty}`) — \
                     an alias must share the target's regblock type",
                    inst.name.name, inst.regblock_ty.name, t.name
                )));
            }
        }
    }

    // Validate non-overlap of sized, non-aliased windows (mirrors v1
    // `check_addrmap_overlap`). Pairs where either side aliases the
    // other, or both alias the same target, are skipped.
    for (i, a) in order.iter().enumerate() {
        for b in order.iter().skip(i + 1) {
            // Skip alias-related pairs.
            let a_alias = a.alias_of.as_ref().map(|t| &t.name);
            let b_alias = b.alias_of.as_ref().map(|t| &t.name);
            if a_alias == Some(&b.name.name)
                || b_alias == Some(&a.name.name)
                || (a_alias.is_some() && a_alias == b_alias)
            {
                continue;
            }
            let (abase, asize, _) = &bases[&a.name.name];
            let (bbase, bsize, _) = &bases[&b.name.name];
            let (Some(asz), Some(bsz)) = (asize, bsize) else {
                continue;
            };
            let aend = abase + asz;
            let bend = bbase + bsz;
            if *abase < bend && *bbase < aend {
                return Err(LowerError::Invalid(format!(
                    "addrmap `{chip}`: instance windows `{}` (0x{abase:x}..0x{aend:x}) and `{}` \
                     (0x{bbase:x}..0x{bend:x}) overlap",
                    a.name.name, b.name.name
                )));
            }
        }
    }

    // Second pass: build the per-instance contexts + the mirror-init
    // list (distinct non-aliased locals).
    let mut instances = Vec::new();
    let mut mirror_inits = Vec::new();
    for inst in &order {
        let (base, _, ty) = &bases[&inst.name.name];
        let regs = &regblock_registers[ty];
        // Pre-shift register offsets by the instance base.
        let registers: Vec<RegRegisterSchema> = regs
            .iter()
            .map(|r| RegRegisterSchema {
                offset: base + r.offset,
                ..r.clone()
            })
            .collect();
        let mirror_key = match &inst.alias_of {
            Some(t) => instance_mirror_key(chip, &t.name),
            None => {
                let key = instance_mirror_key(chip, &inst.name.name);
                let rec = record_of[ty];
                mirror_inits.push((key.clone(), rec));
                key
            }
        };
        instances.push(AddrmapInstanceCtx {
            name: inst.name.name.clone(),
            mirror_key,
            registers,
        });
    }

    Ok(AddrmapBindingCtx {
        helper_field: helper_field.to_string(),
        instances,
        mirror_inits,
    })
}

/// Fold a base/size `Expr` to a constant. Only plain integer literals
/// are lowered (v1 const-folds arbitrary expressions; the corpus uses
/// literals exclusively).
fn fold_const(e: &crate::ast::Expr) -> Option<u64> {
    match &*e.kind {
        ExprKind::Int(s) => super::exprs::parse_int_literal(s),
        _ => None,
    }
}

impl super::FuncBuilder<'_> {
    /// `chip.inst.REG[.FIELD] = value` addrmap write. Returns `Ok(true)`
    /// when `target` is an addrmap access (and lowers it). Delegates to
    /// the shared register/field write lowering with the instance's
    /// shifted-offset register and shared mirror local.
    pub(crate) fn try_lower_addrmap_write(
        &mut self,
        target: &crate::ast::Expr,
        value: &crate::ast::Expr,
    ) -> Result<bool, LowerError> {
        // 4-level field write: chip.inst.REG.FIELD.
        if let Some((mirror_key, helper_field, reg, fld)) = self.as_addrmap_subfield(target) {
            let Some(mirror) = self.lookup(&mirror_key) else {
                return Err(LowerError::Invalid(format!(
                    "addrmap mirror `{mirror_key}` is not in scope at its field-write site"
                )));
            };
            return self.lower_field_write(mirror, &helper_field, &reg, &fld, value);
        }
        // 3-level register write: chip.inst.REG.
        if let Some((mirror_key, helper_field, reg)) = self.as_addrmap_register(target) {
            let Some(mirror) = self.lookup(&mirror_key) else {
                return Err(LowerError::Invalid(format!(
                    "addrmap mirror `{mirror_key}` is not in scope at its write site"
                )));
            };
            return self.lower_reg_write(mirror, &helper_field, &reg, value);
        }
        Ok(false)
    }

    /// `let v = chip.inst.REG[.FIELD]` addrmap read. Returns `Ok(true)`
    /// when `value` is an addrmap access (and lowers it).
    pub(crate) fn try_lower_addrmap_read_let(
        &mut self,
        dest_name: &str,
        value: &crate::ast::Expr,
    ) -> Result<bool, LowerError> {
        if let Some((mirror_key, helper_field, reg, fld)) = self.as_addrmap_subfield(value) {
            let Some(mirror) = self.lookup(&mirror_key) else {
                return Err(LowerError::Invalid(format!(
                    "addrmap mirror `{mirror_key}` is not in scope at its field-read site"
                )));
            };
            let helper_ty = self.regblock_helper_type_pub(&helper_field, "read", 1)?;
            let extract = self.field_read_expr(mirror, &helper_ty, &reg, &fld);
            let id = self.declare(dest_name);
            self.push(crate::ir::Stmt::Assign(id, extract));
            return Ok(true);
        }
        if let Some((mirror_key, helper_field, reg)) = self.as_addrmap_register(value) {
            let Some(mirror) = self.lookup(&mirror_key) else {
                return Err(LowerError::Invalid(format!(
                    "addrmap mirror `{mirror_key}` is not in scope at its read site"
                )));
            };
            self.lower_reg_read_let(mirror, &helper_field, dest_name, &reg)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Addrmap read in a general EXPRESSION position (assert condition /
    /// format arg) — `chip.inst.REG[.FIELD]` that is NOT a `let`-RHS.
    /// `Some(expr)` when `e` is an addrmap access. Same one-read-per-
    /// occurrence semantics as the flat regblock form.
    pub(crate) fn lower_addrmap_read_expr(
        &self,
        e: &crate::ast::Expr,
    ) -> Result<Option<Expr>, LowerError> {
        if let Some((mirror_key, helper_field, reg, fld)) = self.as_addrmap_subfield(e) {
            let Some(mirror) = self.lookup(&mirror_key) else {
                return Err(LowerError::Invalid(format!(
                    "addrmap mirror `{mirror_key}` is not in scope at its field-read site"
                )));
            };
            let helper_ty = self.regblock_helper_type_pub(&helper_field, "read", 1)?;
            return Ok(Some(self.field_read_expr(mirror, &helper_ty, &reg, &fld)));
        }
        if let Some((mirror_key, helper_field, reg)) = self.as_addrmap_register(e) {
            let Some(mirror) = self.lookup(&mirror_key) else {
                return Err(LowerError::Invalid(format!(
                    "addrmap mirror `{mirror_key}` is not in scope at its read site"
                )));
            };
            let helper_ty = self.regblock_helper_type_pub(&helper_field, "read", 1)?;
            return Ok(Some(self.reg_read_expr(mirror, &helper_ty, &reg)));
        }
        Ok(None)
    }

    /// `Some((mirror_key, helper_field, register))` when `e` is a
    /// three-level `chip.inst.REG` access on an addrmap binding.
    fn as_addrmap_register(
        &self,
        e: &crate::ast::Expr,
    ) -> Option<(String, String, RegRegisterSchema)> {
        let ExprKind::Field {
            target: mid,
            name: reg_name,
        } = &*e.kind
        else {
            return None;
        };
        let ExprKind::Field {
            target: outer,
            name: inst_name,
        } = &*mid.kind
        else {
            return None;
        };
        let ExprKind::Ident(chip) = &*outer.kind else {
            return None;
        };
        let actx = self.ctx.addrmap_bindings.get(&chip.name)?;
        let inst = actx.instances.iter().find(|i| i.name == inst_name.name)?;
        let reg = inst.registers.iter().find(|r| r.name == reg_name.name)?;
        Some((
            inst.mirror_key.clone(),
            actx.helper_field.clone(),
            reg.clone(),
        ))
    }

    /// `Some((mirror_key, helper_field, register, field))` when `e` is a
    /// four-level `chip.inst.REG.FIELD` access on an addrmap binding.
    fn as_addrmap_subfield(
        &self,
        e: &crate::ast::Expr,
    ) -> Option<(String, String, RegRegisterSchema, crate::ir::RegFieldSchema)> {
        let ExprKind::Field {
            target: lvl3,
            name: fld_name,
        } = &*e.kind
        else {
            return None;
        };
        let ExprKind::Field {
            target: lvl2,
            name: reg_name,
        } = &*lvl3.kind
        else {
            return None;
        };
        let ExprKind::Field {
            target: lvl1,
            name: inst_name,
        } = &*lvl2.kind
        else {
            return None;
        };
        let ExprKind::Ident(chip) = &*lvl1.kind else {
            return None;
        };
        let actx = self.ctx.addrmap_bindings.get(&chip.name)?;
        let inst = actx.instances.iter().find(|i| i.name == inst_name.name)?;
        let reg = inst.registers.iter().find(|r| r.name == reg_name.name)?;
        let fld = reg.fields.iter().find(|f| f.name == fld_name.name)?;
        Some((
            inst.mirror_key.clone(),
            actx.helper_field.clone(),
            reg.clone(),
            fld.clone(),
        ))
    }

    /// `true` when `id` names an addrmap binding in scope.
    pub(crate) fn is_addrmap_binding(&self, name: &str) -> bool {
        self.ctx.addrmap_bindings.contains_key(name)
    }

    /// The root binding name of any access chain rooted at an addrmap
    /// binding, if any. Used to give a precise out-of-subset rejection.
    fn addrmap_access_root(&self, e: &crate::ast::Expr) -> Option<String> {
        let mut cur = e;
        loop {
            match &*cur.kind {
                ExprKind::Field { target, .. } => cur = target,
                ExprKind::Call { callee, .. } => cur = callee,
                ExprKind::Index { target, .. } => cur = target,
                ExprKind::Paren(inner) => cur = inner,
                ExprKind::Ident(id) => {
                    return self.is_addrmap_binding(&id.name).then(|| id.name.clone());
                }
                _ => return None,
            }
        }
    }

    /// Reject an out-of-subset access on an addrmap binding (an unknown
    /// instance/register/field, or a call) with a precise message.
    /// Called after the in-subset 3-/4-level read/write paths declined
    /// `e`. A no-op when `e` is not an addrmap access at all.
    pub(crate) fn reject_out_of_subset_addrmap_access(
        &self,
        e: &crate::ast::Expr,
        ctx_label: &str,
    ) -> Result<(), LowerError> {
        let Some(binding) = self.addrmap_access_root(e) else {
            return Ok(());
        };
        let detail = match &*e.kind {
            ExprKind::Call { callee, .. } => match &*callee.kind {
                ExprKind::Field { name, .. } => {
                    format!("a method call `.{}(...)` on an addrmap binding", name.name)
                }
                _ => "a call on an addrmap binding".to_string(),
            },
            ExprKind::Field { name, .. } => format!(
                "access `{}...{}` (no such instance/register/field)",
                binding, name.name
            ),
            _ => format!("a {ctx_label} on addrmap binding `{binding}`"),
        };
        Err(unsupported(
            &format!("{detail} (addrmap `{binding}`)"),
            "3-level `chip.inst.REG` and 4-level `chip.inst.REG.FIELD` reads/writes \
             (incl. assert/format positions) are lowered",
        ))
    }
}
