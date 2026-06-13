//! `transactor` declaration lowering (docs/tb-ir-design.md
//! §"Function-kind handling": one `TbFunction` per method).
//!
//! Subset: the **unbound DUT-poking BFM** form —
//!
//! ```text
//! transactor CamDualXactor
//!     dut : Mshr_Addr_Cam_Dual          // exactly one module-typed field
//!     when active
//!         hookable write1(idx: uint<4>, ...)   // scalar params <= 64 bits
//!             dut.write_valid = 1
//!             wait 1 cycle
//!             ...
//!         end write1
//!     end when
//! end transactor CamDualXactor
//! ```
//!
//! Each method body lowers with the transactor's module-typed field as
//! the DUT name (replacing v1's emission-time `field_subs` substitution
//! with lowering-time resolution), producing an ordinary CFG whose DUT
//! accesses are `PortRef`s. Method waits keep v1's synchronous hookable
//! semantics: the tbir backend emits them as `tick()` loops, never as
//! scheduler suspensions — so clock-qualified waits and timed
//! `wait until` (whose v1 sync shapes are out of this slice) are
//! rejected inside method bodies.
//!
//! Everything outside the subset — `bound to <BusType>`, generics,
//! event ports, scalar state fields, `on` handlers, TLM target
//! threads, watchdogs — is an explicit `Unsupported`.

use super::{FuncBuilder, LowerCtx, LowerError, helpers, unsupported};
use crate::ast::{ComponentItem, HookableMethod, TransactorDecl, TypeArg, TypeExpr};
use crate::ir::{
    FunctionId, FunctionKind, TbFunction, Terminator, TransactorId, TransactorMethodSchema,
    TransactorSchema, TypedParam,
};
use std::collections::{HashMap, HashSet};

/// Lower one `transactor` declaration into a schema plus one
/// `TbFunction` per method. `next_fn` is the id the FIRST method
/// function will get (the caller pushes the returned functions in
/// order).
pub(crate) fn lower_transactor(
    id: TransactorId,
    t: &TransactorDecl,
    next_fn: FunctionId,
    helper_registry: &helpers::HelperRegistry<'_>,
    record_ctx: &LowerCtx,
) -> Result<(TransactorSchema, Vec<TbFunction>), LowerError> {
    let tname = &t.name.name;
    if !t.params.is_empty() {
        return Err(unsupported(
            &format!("transactor `{tname}` with generic parameters"),
            "",
        ));
    }
    if t.bound_to.is_some() {
        return Err(unsupported(
            &format!("transactor `{tname}` bound to a bus type"),
            "only unbound DUT-poking transactors are lowered; the bus-bound \
             (target-side TLM) form is a follow-up slice",
        ));
    }

    // Walk always-on items then the `when active` body — the same
    // flattening v1's `synth_component_from_transactor` performs with
    // include_active = true. All methods require an `active` instance
    // in this subset (passive instances are rejected at the testbench
    // field), so active-only placement carries no behavior here.
    let mut dut: Option<(String, String)> = None; // (field, module type)
    let mut methods_ast: Vec<&HookableMethod> = Vec::new();
    let all_items = t
        .items
        .iter()
        .chain(t.when_active.iter().flatten());
    for ci in all_items {
        match ci {
            ComponentItem::Field(f) => {
                let fname = &f.name.name;
                if f.direction.is_some() {
                    return Err(unsupported(
                        &format!("transactor `{tname}` event/directional field `{fname}`"),
                        "event-driven transactors await the event slice",
                    ));
                }
                let TypeExpr::Named { name, .. } = &f.ty else {
                    return Err(unsupported(
                        &format!("transactor `{tname}` state field `{fname}`"),
                        "only one module-typed (DUT handle) field is lowered",
                    ));
                };
                let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                if record_ctx.record_ids.contains_key(simple) {
                    return Err(unsupported(
                        &format!(
                            "transactor `{tname}` field `{fname}` of transaction type `{simple}`"
                        ),
                        "",
                    ));
                }
                if f.default.is_some() {
                    return Err(unsupported(
                        &format!("transactor `{tname}` field `{fname}` with a default value"),
                        "",
                    ));
                }
                if dut.is_some() {
                    return Err(unsupported(
                        &format!(
                            "transactor `{tname}` with more than one module-typed field \
                             (`{}`, `{fname}`)",
                            dut.as_ref().unwrap().0
                        ),
                        "",
                    ));
                }
                dut = Some((fname.clone(), simple.to_string()));
            }
            ComponentItem::Hookable(h) => methods_ast.push(h),
            ComponentItem::OnHandler(_) => {
                return Err(unsupported(
                    &format!("transactor `{tname}` `on` handlers"),
                    "event-driven transactors await the event slice",
                ));
            }
            ComponentItem::TargetTlmThread(_) => {
                return Err(unsupported(
                    &format!("transactor `{tname}` TLM target threads"),
                    "",
                ));
            }
            ComponentItem::Watchdog(_) => {
                return Err(unsupported(&format!("transactor `{tname}` watchdogs"), ""));
            }
            ComponentItem::Connect(_) => {
                return Err(unsupported(
                    &format!("transactor `{tname}` connect blocks"),
                    "",
                ));
            }
            ComponentItem::Lifecycle(..) | ComponentItem::Apply(_) => {
                return Err(unsupported(
                    &format!("transactor `{tname}` lifecycle/apply items"),
                    "",
                ));
            }
        }
    }

    let Some((dut_field, dut_type)) = dut else {
        return Err(unsupported(
            &format!("transactor `{tname}` without a module-typed (DUT handle) field"),
            "unbound transactors drive the DUT directly and need exactly one",
        ));
    };

    let mut schema = TransactorSchema {
        name: tname.clone(),
        dut_field: dut_field.clone(),
        dut_type,
        methods: Vec::new(),
    };
    // Method bodies resolve DUT accesses against the transactor's own
    // module-typed field name; everything else mirrors the file-level
    // helper context (records visible, no clocks, no testbench).
    let method_ctx = LowerCtx {
        dut_field: dut_field.clone(),
        tb_field: None,
        cov_fields: HashMap::new(),
        covgroups: Vec::new(),
        clock_names: Vec::new(),
        record_ids: record_ctx.record_ids.clone(),
        records: record_ctx.records.clone(),
        // Method bodies see neither bus bindings nor sibling transactor
        // instances — both are test-scope; nested call edges stay out
        // of method bodies structurally.
        bus_bindings: HashMap::new(),
        transactor_fields: HashMap::new(),
        transactors: Vec::new(),
        // Method bodies see no scoreboards either — scoreboards are
        // test-scope testbench fields, structurally invisible here.
        scoreboard_fields: HashMap::new(),
        scoreboards: Vec::new(),
        // Method bodies see file-scope consts; they have no testbench,
        // so no scalar fields, helper methods, or test-scope lets.
        consts: record_ctx.consts.clone(),
        tb_scalar_fields: HashSet::new(),
        tb_methods: HashMap::new(),
        test_scope_lets: HashSet::new(),
        regblock_bindings: HashMap::new(),
        regblock_init_order: Vec::new(),
        bare_transactor_fields: HashSet::new(),
    };

    let mut funcs = Vec::new();
    for h in methods_ast {
        let mname = &h.name.name;
        if schema.method(mname).is_some() {
            return Err(LowerError::Invalid(format!(
                "transactor `{tname}` declares method `{mname}` more than once"
            )));
        }
        check_scalar_ty(tname, mname, "return type", h.return_ty.as_ref())?;

        let fid = FunctionId(next_fn.0 + funcs.len() as u32);
        let mut b = FuncBuilder::new(&method_ctx, helper_registry);
        b.in_transactor_method = true;
        let mut params = Vec::with_capacity(h.params.len());
        for p in &h.params {
            check_scalar_ty(tname, mname, &format!("parameter `{}`", p.name.name), p.ty.as_ref())?;
            let ty = helpers::ir_type_of(p.ty.as_ref());
            let local = b.declare(&p.name.name);
            b.set_local_type(local, ty.clone());
            params.push(TypedParam {
                name: p.name.name.clone(),
                ty,
            });
        }
        if h.return_ty.is_some() {
            let ret = b.declare("__ret");
            b.helper_ret = Some(ret);
        }
        b.lower_block_stmts(&h.body)?;
        if !b.is_terminated() {
            b.terminate(Terminator::Return);
        }
        let mut f = b.finish(
            fid,
            format!("{tname}_{mname}"),
            FunctionKind::TransactorBody { transactor: id },
            None,
        )?;
        f.params = params;
        schema.methods.push(TransactorMethodSchema {
            name: mname.clone(),
            function: fid,
            n_params: f.params.len(),
            has_ret: f.ret.is_some(),
        });
        funcs.push(f);
    }

    Ok((schema, funcs))
}

/// Method params and returns must be scalar (bool / uint / sint) and
/// at most 64 bits wide — the tbir value model is `uint64_t`. v1 lowers
/// wider widths through `_harc_u128` / `VlWide`, which this slice does
/// not mirror; the rejection names the offending site.
fn check_scalar_ty(
    tname: &str,
    mname: &str,
    what: &str,
    ty: Option<&TypeExpr>,
) -> Result<(), LowerError> {
    let site = || format!("transactor method `{tname}.{mname}` {what}");
    match ty {
        None => Ok(()),
        Some(TypeExpr::Builtin { args, .. }) => {
            // Width arg, when present, must fit the u64 value model.
            if let Some(TypeArg::Expr(e)) = args.first() {
                if let crate::ast::ExprKind::Int(s) = &*e.kind {
                    if let Ok(w) = s.replace('_', "").parse::<u32>() {
                        if w > 64 {
                            return Err(unsupported(
                                &format!("{} wider than 64 bits (uint<{w}>)", site()),
                                "the tbir value model is 64-bit",
                            ));
                        }
                    }
                }
            }
            // ir_type_of's IrType covers the scalar builtins; anything
            // it can't classify still lowers as an untyped u64 local,
            // matching pure-helper parameter handling.
            Ok(())
        }
        Some(_) => Err(unsupported(
            &format!("{} with a non-scalar type", site()),
            "",
        )),
    }
}
