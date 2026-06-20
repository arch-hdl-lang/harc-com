//! `covergroup_hooks` — late resolution of hook-triggered covergroups
//! (`covergroup G @(drv.send(t) post)`).
//!
//! Covergroup schemas lower EARLY (before the testbench/transactor
//! tables exist — `src/ir/lower/covergroups.rs`), so a `@(<call> pre|post)`
//! trigger cannot resolve its target at lowering time. Lowering instead
//! stashes the receiver field-access path and method name on
//! `CovTrigger::Hook`. This pass runs AFTER the whole program is lowered
//! and resolves each such trigger against the owning testbench's
//! transactor fields, recording the subscription on the target method's
//! `cov_hook_subs`. The tbir backend then emits the method's
//! `<Type>_<method>_pre`/`_post` hook-vector spine + fan-out and pushes
//! the cov field's sample closure onto that vector instead of `_checkers`
//! (mirrors v1's `emit_hook_vectors` + `emit_covergroup_hook_sample_registration`).
//!
//! Scope (parity with v1's shipped surface, kept minimal): the receiver
//! must resolve to a single transactor testbench field whose transactor
//! declares the named `hookable` method. Nested env/component paths
//! (`env.mon.observed`) and component (non-transactor) hookable methods
//! are out of the tbir parity subset and rejected with a clear error.

use crate::ir::lower::LowerError;
use crate::ir::{CovTrigger, CovgroupId, TbProgram};

/// Resolve every hook-triggered covergroup's target and record the
/// subscription on the matching transactor method. Mutating; leaves the
/// program ready for emission.
pub fn run(prog: &mut TbProgram) -> Result<(), LowerError> {
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
        // Receiver path must be a single transactor field in this subset.
        let [field] = p.receiver_path.as_slice() else {
            return Err(LowerError::Invalid(format!(
                "covergroup `{cg_name}` hook trigger `{}` (cov field `{}`): nested \
                 receiver paths are not supported by the tbir backend — name a \
                 transactor testbench field directly (e.g. `drv.{}`)",
                p.receiver_path.join("."),
                p.cov_field,
                p.method
            )));
        };
        let Some((_, xid)) = tb.transactor_fields.iter().find(|(name, _)| name == field) else {
            return Err(LowerError::Invalid(format!(
                "covergroup `{cg_name}` hook trigger receiver `{field}` (cov field `{}`) \
                 does not name a transactor testbench field on `{}`",
                p.cov_field, tb.name
            )));
        };
        let xid = *xid;
        // The transactor must declare the named hookable method.
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
        // Trigger argument names must match the hookable method's
        // parameter names, in order — v1's emit-time check. This binds
        // each `cover <param>.<field>` target to a real by-value closure
        // argument (named after the method param) in the sampler.
        let method_schema = &prog.transactors[xid.index()].methods[midx];
        let func = prog.function(method_schema.function);
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
                     parameter `{param}` on `{field}.{}` (cov field `{}`)",
                    p.method, p.cov_field
                )));
            }
        }
        prog.transactors[xid.index()].methods[midx]
            .cov_hook_subs
            .push((p.covgroup, p.side));
    }
    Ok(())
}
