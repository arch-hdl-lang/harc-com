//! `covergroup_hooks` — late resolution of hook-triggered covergroups
//! (`covergroup G @(drv.send(t) post)`).
//!
//! Covergroup schemas lower EARLY (before the testbench/transactor
//! tables exist — `src/ir/lower/covergroups.rs`), so a `@(<call> pre|post)`
//! trigger cannot resolve its target at lowering time. Lowering instead
//! stashes the receiver field-access path and method name on
//! `CovTrigger::Hook`. This pass runs AFTER the whole program is lowered
//! and resolves each such trigger against the owning testbench's
//! transactor/component fields, recording the subscription on the target
//! method's `cov_hook_subs`. The tbir backend then emits the method's
//! `<Type>_<method>_pre`/`_post` hook-vector spine + fan-out and pushes
//! the cov field's sample closure onto that vector instead of `_checkers`
//! (mirrors v1's `emit_hook_vectors` + `emit_covergroup_hook_sample_registration`).
//!
//! Scope (parity with v1's shipped surface, kept minimal): the receiver
//! must resolve to a single transactor or composite-component testbench field
//! whose type declares the named `hookable` method. Nested env/component paths
//! (`env.mon.observed`) are out of the tbir parity subset and rejected with a
//! clear error.

use crate::ir::lower::LowerError;
use crate::ir::{CallTarget, CovTrigger, CovgroupId, Expr, FunctionKind, IrType, TbProgram};
use std::collections::HashMap;

enum HookTarget {
    Transactor {
        xid: crate::ir::TransactorId,
        midx: usize,
        type_name: String,
        function: crate::ir::FunctionId,
    },
    Component {
        cid: crate::ir::ComponentId,
        midx: usize,
        type_name: String,
        function: crate::ir::FunctionId,
    },
}

/// Resolve every hook-triggered covergroup's target and record the
/// subscription on the matching transactor method. Mutating; leaves the
/// program ready for emission.
pub fn run(prog: &mut TbProgram) -> Result<(), LowerError> {
    // Clock-triggered schemas have no hook-parameter environment, so their
    // complete expression typing can run immediately. Previously only hook
    // schemas reached this checker, allowing malformed helper calls in an
    // otherwise identical posedge covergroup to pass verification.
    let no_hook_params = HashMap::new();
    for (index, schema) in prog.covgroups.iter().enumerate() {
        if matches!(schema.trigger, CovTrigger::PosedgeDutClk) {
            validate_covergroup_expr_types(prog, index, &no_hook_params, false)?;
        }
    }

    // Collect (covgroup, receiver_path, method, side) for every cov field
    // bound to a hook-triggered covergroup, per testbench. A covergroup
    // schema is shared by id; the subscription is recorded once per
    // (testbench cov field) so a group sampled by two fields fans out
    // twice — matching v1's per-field registration.
    struct Pending {
        covgroup: CovgroupId,
        receiver_path: Vec<String>,
        method: String,
        param_names: Vec<String>,
        side: crate::ast::HookSide,
        tb_index: usize,
        cov_field: String,
    }
    let mut pending: Vec<Pending> = Vec::new();
    for (tb_index, tb) in prog.testbenches.iter().enumerate() {
        for (cov_field, cg) in &tb.cov_fields {
            let schema = &prog.covgroups[cg.index()];
            if let CovTrigger::Hook {
                receiver_path,
                method,
                param_names,
                side,
            } = &schema.trigger
            {
                pending.push(Pending {
                    covgroup: *cg,
                    receiver_path: receiver_path.clone(),
                    method: method.clone(),
                    param_names: param_names.clone(),
                    side: *side,
                    tb_index,
                    cov_field: cov_field.clone(),
                });
            }
        }
    }

    for p in pending {
        let tb = &prog.testbenches[p.tb_index];
        let cg_name = prog.covgroups[p.covgroup.index()].name.clone();
        // Receiver path must be a single testbench field in this subset.
        let [field] = p.receiver_path.as_slice() else {
            return Err(LowerError::Invalid(format!(
                "covergroup `{cg_name}` hook trigger `{}` (cov field `{}`): nested \
                 receiver paths are not supported by the tbir backend — name a \
                 transactor or component testbench field directly (e.g. `drv.{}`)",
                p.receiver_path.join("."),
                p.cov_field,
                p.method
            )));
        };
        let target =
            if let Some((_, xid)) = tb.transactor_fields.iter().find(|(name, _)| name == field) {
                let xid = *xid;
                let xname = prog.transactors[xid.index()].name.clone();
                let Some(midx) = prog.transactors[xid.index()]
                    .methods
                    .iter()
                    .position(|m| m.name == p.method)
                else {
                    return Err(LowerError::Invalid(format!(
                        "covergroup `{cg_name}` hook trigger `{field}.{}` (cov field `{}`) \
                     does not resolve to a method on transactor `{xname}`",
                        p.method, p.cov_field
                    )));
                };
                if !prog.transactors[xid.index()].methods[midx].hookable {
                    return Err(LowerError::Invalid(format!(
                        "covergroup `{cg_name}` hook trigger `{field}.{}` (cov field `{}`) \
                     does not name a `hookable` method on transactor `{xname}`",
                        p.method, p.cov_field
                    )));
                }
                HookTarget::Transactor {
                    xid,
                    midx,
                    type_name: xname,
                    function: prog.transactors[xid.index()].methods[midx].function,
                }
            } else if let Some(binding) = tb.component_fields.iter().find(|b| b.field == *field) {
                let cid = binding.component;
                let cname = prog.components[cid.index()].name.clone();
                let Some(midx) = prog.components[cid.index()]
                    .methods
                    .iter()
                    .position(|m| m.name == p.method)
                else {
                    return Err(LowerError::Invalid(format!(
                        "covergroup `{cg_name}` hook trigger `{field}.{}` (cov field `{}`) \
                     does not resolve to a method on component `{cname}`",
                        p.method, p.cov_field
                    )));
                };
                if !prog.components[cid.index()].methods[midx].hookable {
                    return Err(LowerError::Invalid(format!(
                        "covergroup `{cg_name}` hook trigger `{field}.{}` (cov field `{}`) \
                     must resolve to a `hookable` method on component `{cname}`",
                        p.method, p.cov_field
                    )));
                }
                let target_method = &prog.components[cid.index()].methods[midx];
                if !crate::ir::component_mode_includes_activation(
                    binding.mode,
                    target_method.activation,
                ) {
                    return Err(LowerError::Invalid(format!(
                        "covergroup `{cg_name}` hook trigger `{field}.{}` (cov field `{}`) \
                         targets active-only method on passive component binding `{field}`",
                        p.method, p.cov_field
                    )));
                }
                HookTarget::Component {
                    cid,
                    midx,
                    type_name: cname,
                    function: prog.components[cid.index()].methods[midx].function,
                }
            } else {
                return Err(LowerError::Invalid(format!(
                    "covergroup `{cg_name}` hook trigger receiver `{field}` (cov field `{}`) \
                 does not name a transactor or component testbench field on `{}`",
                    p.cov_field, tb.name
                )));
            };
        // Trigger argument names must match the hookable method's
        // parameter names, in order — v1's emit-time check. This binds
        // each `cover <param>.<field>` target to a real by-value closure
        // argument (named after the method param) in the sampler.
        let func = prog.function(match &target {
            HookTarget::Transactor { function, .. } | HookTarget::Component { function, .. } => {
                *function
            }
        });
        let method_params: Vec<&str> = func.params.iter().map(|p| p.name.as_str()).collect();
        if p.param_names.len() != method_params.len() {
            return Err(LowerError::Invalid(format!(
                "covergroup `{cg_name}` hook trigger `{field}.{}` (cov field `{}`) expects {} \
                 argument(s), got {}",
                p.method,
                p.cov_field,
                method_params.len(),
                p.param_names.len()
            )));
        }
        for (arg, param) in p.param_names.iter().zip(method_params.iter()) {
            if arg != param || arg == "_" {
                return Err(LowerError::Invalid(format!(
                    "covergroup `{cg_name}` hook trigger argument `{arg}` must match hook \
                     parameter `{param}` on `{field}.{} <{}>` (cov field `{}`)",
                    p.method,
                    match &target {
                        HookTarget::Transactor { type_name, .. }
                        | HookTarget::Component { type_name, .. } => type_name,
                    },
                    p.cov_field
                )));
            }
        }
        let hook_param_types: HashMap<String, IrType> = func
            .params
            .iter()
            .map(|p| (p.name.clone(), p.ty.clone()))
            .collect();
        validate_covergroup_expr_types(prog, p.covgroup.index(), &hook_param_types, true)?;
        match target {
            HookTarget::Transactor { xid, midx, .. } => prog.transactors[xid.index()].methods[midx]
                .cov_hook_subs
                .push((p.covgroup, p.side)),
            HookTarget::Component { cid, midx, .. } => prog.components[cid.index()].methods[midx]
                .cov_hook_subs
                .push((p.covgroup, p.side)),
        }
    }
    Ok(())
}

/// Every RUNTIME expression inside one bin member — the member itself
/// for an exact value, or the present ends of a range. Constant bounds
/// are skipped: they folded to a `u64` and carry no type to check.
fn bin_bounds(v: &crate::ir::CovBinValue) -> Vec<&Expr> {
    use crate::ir::{CovBinBound, CovBinValue};
    fn runtime(b: &CovBinBound) -> Option<&Expr> {
        match b {
            CovBinBound::Runtime(e) => Some(e),
            CovBinBound::Const(_) => None,
        }
    }
    match v {
        CovBinValue::Eq(b) => runtime(b).into_iter().collect(),
        CovBinValue::Range { lo, hi } => lo
            .as_ref()
            .and_then(runtime)
            .into_iter()
            .chain(hi.as_ref().and_then(runtime))
            .collect(),
    }
}

fn validate_covergroup_expr_types(
    prog: &TbProgram,
    covgroup: usize,
    hook_params: &HashMap<String, IrType>,
    hook_context: bool,
) -> Result<(), LowerError> {
    let schema = &prog.covgroups[covgroup];
    for point in &schema.points {
        let ty = coverpoint_expr_type(prog, hook_params, &point.target).map_err(|msg| {
            LowerError::Invalid(format!(
                "covergroup `{}` point `{}` {}target type error: {msg}",
                schema.name,
                point.name,
                if hook_context { "hook " } else { "" }
            ))
        })?;
        if !is_scalar(&ty) {
            return Err(LowerError::Invalid(format!(
                "covergroup `{}` point `{}` {}target must be scalar, got {}",
                schema.name,
                point.name,
                if hook_context { "hook " } else { "" },
                type_name(prog, &ty)
            )));
        }

        // Bin members and range bounds need the same check as the target:
        // each is compared against the sampler's scalar `_v`.
        for bin in &point.bins {
            for bound in bin.values.iter().flat_map(bin_bounds) {
                let ty = coverpoint_expr_type(prog, hook_params, bound).map_err(|msg| {
                    LowerError::Invalid(format!(
                        "covergroup `{}` point `{}` bin `{}` {}type error: {msg}",
                        schema.name,
                        point.name,
                        bin.name,
                        if hook_context { "hook " } else { "" }
                    ))
                })?;
                if !is_scalar(&ty) {
                    return Err(LowerError::Invalid(format!(
                        "covergroup `{}` point `{}` bin `{}` must compare against a scalar, got {}",
                        schema.name,
                        point.name,
                        bin.name,
                        type_name(prog, &ty)
                    )));
                }
            }
        }
    }
    Ok(())
}

fn coverpoint_expr_type(
    prog: &TbProgram,
    hook_params: &HashMap<String, IrType>,
    expr: &Expr,
) -> Result<IrType, String> {
    match expr {
        Expr::Literal { ty, .. } => Ok(ty.clone()),
        Expr::WideLiteral(words) => Ok(IrType::UInt(
            words
                .len()
                .checked_mul(32)
                .and_then(|w| u32::try_from(w).ok()),
        )),
        Expr::Port(port) => {
            if let Some(crate::ir::LaneIndex::Var(index)) = &port.lane {
                let index_ty = coverpoint_expr_type(prog, hook_params, index)?;
                require_scalar(prog, &index_ty, "DUT lane index")?;
                reject_wide_composition_type(&index_ty, "DUT lane index")?;
            }
            Ok(IrType::UInt(port.width))
        }
        Expr::Unary(_, inner) => {
            let ty = coverpoint_expr_type(prog, hook_params, inner)?;
            require_scalar(prog, &ty, "unary operand")?;
            reject_wide_composition_type(&ty, "unary expression")?;
            Ok(ty)
        }
        Expr::Binary(_, lhs, rhs) => {
            let lhs_ty = coverpoint_expr_type(prog, hook_params, lhs)?;
            let rhs_ty = coverpoint_expr_type(prog, hook_params, rhs)?;
            require_scalar(prog, &lhs_ty, "binary lhs")?;
            require_scalar(prog, &rhs_ty, "binary rhs")?;
            reject_wide_composition_type(&lhs_ty, "binary expression lhs")?;
            reject_wide_composition_type(&rhs_ty, "binary expression rhs")?;
            Ok(IrType::UInt(Some(64)))
        }
        Expr::Ternary(cond, then_expr, else_expr) => {
            let cond_ty = coverpoint_expr_type(prog, hook_params, cond)?;
            let then_ty = coverpoint_expr_type(prog, hook_params, then_expr)?;
            let else_ty = coverpoint_expr_type(prog, hook_params, else_expr)?;
            require_scalar(prog, &cond_ty, "ternary condition")?;
            require_scalar(prog, &then_ty, "ternary then branch")?;
            require_scalar(prog, &else_ty, "ternary else branch")?;
            reject_wide_composition_type(&cond_ty, "ternary condition")?;
            reject_wide_composition_type(&then_ty, "ternary then branch")?;
            reject_wide_composition_type(&else_ty, "ternary else branch")?;
            Ok(common_scalar_type(&then_ty, &else_ty))
        }
        Expr::BitSlice { hi, lo, target } => {
            let target_ty = coverpoint_expr_type(prog, hook_params, target)?;
            require_scalar(prog, &target_ty, "bit-slice target")?;
            Ok(IrType::UInt(Some(hi - lo + 1)))
        }
        Expr::BitSliceDyn { target, hi, lo } => {
            let target_ty = coverpoint_expr_type(prog, hook_params, target)?;
            let hi_ty = coverpoint_expr_type(prog, hook_params, hi)?;
            let lo_ty = coverpoint_expr_type(prog, hook_params, lo)?;
            require_scalar(prog, &target_ty, "runtime bit-slice target")?;
            require_scalar(prog, &hi_ty, "runtime bit-slice high bound")?;
            require_scalar(prog, &lo_ty, "runtime bit-slice low bound")?;
            reject_wide_composition_type(&hi_ty, "runtime bit-slice high bound")?;
            reject_wide_composition_type(&lo_ty, "runtime bit-slice low bound")?;
            Ok(IrType::UInt(None))
        }
        Expr::WidthCast {
            kind, width, inner, ..
        } => {
            let inner_ty = coverpoint_expr_type(prog, hook_params, inner)?;
            require_scalar(prog, &inner_ty, "cast operand")?;
            Ok(match kind {
                crate::ir::WidthCastKind::Sext => IrType::SInt(Some(*width)),
                _ => IrType::UInt(Some(*width)),
            })
        }
        Expr::CovHookArg { param } => hook_params
            .get(param)
            .cloned()
            .ok_or_else(|| format!("unknown hook parameter `{param}`")),
        Expr::CovHookParam {
            param,
            field,
            index,
        } => {
            let param_ty = hook_params
                .get(param)
                .ok_or_else(|| format!("unknown hook parameter `{param}`"))?;
            let IrType::Record(rid) = param_ty else {
                return Err(format!(
                    "`{param}.{field}` requires record hook parameter `{param}`, got {}",
                    type_name(prog, param_ty)
                ));
            };
            let record = &prog.records[rid.index()];
            let field_schema = record.field(field).ok_or_else(|| {
                format!(
                    "record hook parameter `{param}` of type `{}` has no field `{field}`",
                    record.name
                )
            })?;
            match (index.is_some(), field_schema.vec_len) {
                (true, None) => {
                    return Err(format!(
                        "field `{}` on record `{}` is not a Vec field and cannot be indexed",
                        field_schema.name, record.name
                    ));
                }
                (false, Some(_)) => {
                    return Err(format!(
                        "field `{}` on record `{}` is a Vec field and must be indexed",
                        field_schema.name, record.name
                    ));
                }
                _ => {}
            }
            if let Some(index) = index {
                let index_ty = coverpoint_expr_type(prog, hook_params, index)?;
                require_scalar(prog, &index_ty, "hook field index")?;
                reject_wide_composition_type(&index_ty, "hook field index")?;
            }
            Ok(field_schema.ty.clone())
        }
        Expr::Call(target, args) => match target {
            CallTarget::Helper { name, ret } => {
                let helper = prog
                    .functions
                    .iter()
                    .find(|f| f.kind == FunctionKind::Helper && f.name == *name)
                    .ok_or_else(|| format!("unknown pure helper `{name}`"))?;
                if args.len() != helper.params.len() {
                    return Err(format!(
                        "helper `{name}` takes {} argument(s), call passes {}",
                        helper.params.len(),
                        args.len()
                    ));
                }
                for (idx, (arg, param)) in args.iter().zip(&helper.params).enumerate() {
                    let actual = coverpoint_expr_type(prog, hook_params, arg)?;
                    if !scalar_compatible(&param.ty, &actual) {
                        return Err(format!(
                            "helper `{name}` argument {} expects {}, got {}",
                            idx + 1,
                            type_name(prog, &param.ty),
                            type_name(prog, &actual)
                        ));
                    }
                }
                let actual_ret = helper
                    .ret
                    .map(|r| helper.locals[r.index()].ty.clone())
                    .ok_or_else(|| format!("helper `{name}` has no return value"))?;
                require_scalar(prog, &actual_ret, &format!("helper `{name}` return"))?;
                if actual_ret != *ret {
                    return Err(format!(
                        "helper `{name}` call return type disagrees with declaration"
                    ));
                }
                Ok(ret.clone())
            }
            CallTarget::ExternFn { name, ret } => {
                for (idx, arg) in args.iter().enumerate() {
                    let actual = coverpoint_expr_type(prog, hook_params, arg)?;
                    require_scalar(
                        prog,
                        &actual,
                        &format!("extern function `{name}` argument {}", idx + 1),
                    )?;
                }
                require_scalar(prog, ret, &format!("extern function `{name}` return"))?;
                Ok(ret.clone())
            }
            other => Err(format!("unsupported coverpoint call target `{other:?}`")),
        },
        other => Err(format!("unsupported coverpoint expression `{other:?}`")),
    }
}

fn require_scalar(prog: &TbProgram, ty: &IrType, what: &str) -> Result<(), String> {
    if is_scalar(ty) {
        Ok(())
    } else {
        Err(format!(
            "{what} must be scalar, got {}",
            type_name(prog, ty)
        ))
    }
}

fn reject_wide_composition_type(ty: &IrType, what: &str) -> Result<(), String> {
    if matches!(ty, IrType::UInt(Some(width)) | IrType::SInt(Some(width)) if *width > 64) {
        Err(format!(
            "{what} uses a value wider than 64 bits; composed wide cover expressions need type-directed operand coercion"
        ))
    } else {
        Ok(())
    }
}

fn is_scalar(ty: &IrType) -> bool {
    matches!(
        ty,
        IrType::UInt(_) | IrType::SInt(_) | IrType::Bool | IrType::Unknown
    )
}

fn scalar_compatible(expected: &IrType, actual: &IrType) -> bool {
    if !is_scalar(expected) || !is_scalar(actual) {
        return false;
    }
    if expected == actual
        || matches!(expected, IrType::Unknown)
        || matches!(actual, IrType::Unknown)
        || matches!(expected, IrType::UInt(None) | IrType::SInt(None))
        || matches!(actual, IrType::UInt(None) | IrType::SInt(None))
    {
        return true;
    }
    match (expected, actual) {
        (IrType::UInt(Some(ew)), IrType::UInt(Some(aw)))
        | (IrType::SInt(Some(ew)), IrType::SInt(Some(aw))) => aw <= ew,
        (IrType::UInt(Some(ew)), IrType::Bool) | (IrType::SInt(Some(ew)), IrType::Bool) => *ew >= 1,
        _ => false,
    }
}

fn common_scalar_type(lhs: &IrType, rhs: &IrType) -> IrType {
    match (lhs, rhs) {
        (IrType::SInt(Some(lw)), IrType::SInt(Some(rw))) => IrType::SInt(Some((*lw).max(*rw))),
        (IrType::UInt(Some(lw)), IrType::UInt(Some(rw))) => IrType::UInt(Some((*lw).max(*rw))),
        (IrType::Bool, IrType::Bool) => IrType::Bool,
        (IrType::SInt(_), _) | (_, IrType::SInt(_)) => IrType::SInt(None),
        _ => IrType::UInt(None),
    }
}

fn type_name(prog: &TbProgram, ty: &IrType) -> String {
    match ty {
        IrType::UInt(Some(w)) => format!("uint<{w}>"),
        IrType::UInt(None) => "uint".to_string(),
        IrType::SInt(Some(w)) => format!("sint<{w}>"),
        IrType::SInt(None) => "sint".to_string(),
        IrType::Bool => "bool".to_string(),
        IrType::Event(_) => "event channel".to_string(),
        IrType::Record(r) => format!("record `{}`", prog.records[r.index()].name),
        IrType::RecordSeq(r) => format!("TSeq<{}>", prog.records[r.index()].name),
        IrType::Seq(elem) => format!("TSeq<{}>", type_name(prog, elem)),
        IrType::Component(c) => format!("component `{}`", prog.components[c.index()].name),
        IrType::Unknown => "unknown".to_string(),
    }
}
