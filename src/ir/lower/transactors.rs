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
//! Persistent scalar state fields (`last_read : uint<32> default 0`)
//! materialize on a per-instance state struct, exactly like the
//! bound-to target form: method bodies read/write them by bare name and
//! the test reads them back as `<instance>.<field>`. The DUT-poking BFM
//! still requires exactly one module-typed DUT handle field.
//!
//! Everything outside the subset — `bound to <BusType>` (initiator side),
//! generics, event ports, `on` handlers, TLM target threads — is an
//! explicit `Unsupported`. A `watchdog` is the exception: v1 emits its
//! body and never schedules it, so that one is a `NotImplemented`.

use super::{
    helpers, not_implemented, unsupported, FuncBuilder, LowerCtx, LowerError, SideTables, V1Status,
};
use crate::ast::{
    BusDecl, ComponentField, ComponentItem, HookableMethod, Param, TargetTlmThread, TransactorDecl,
    TypeArg, TypeExpr,
};
use crate::ir::{
    self, FunctionId, FunctionKind, IrType, StateFieldKind, StateFieldSchema,
    TargetTlmMethodSchema, TbFunction, Terminator, TransactorId, TransactorMethodSchema,
    TransactorSchema, TypedParam,
};
use std::cell::RefCell;
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
    buses: &HashMap<String, &BusDecl>,
    downstream_binds: &HashMap<String, BusDecl>,
    side_tables: &RefCell<SideTables>,
) -> Result<(TransactorSchema, Vec<TbFunction>), LowerError> {
    let tname = &t.name.name;
    if !t.params.is_empty() {
        // The THIRD landing of the component-parameter construct, after
        // the analysis-source and composite arms in `components.rs`, and
        // it behaves the same. v1 never reads a `#(...)` list:
        //
        //   * unused — output is byte-identical to the same transactor
        //     without the parameter (offsets normalized; the only
        //     residue is a source position inside a string literal).
        //   * referenced from a METHOD BODY while a file-scope `const N`
        //     exists — v1 emits the const at namespace scope and the use
        //     lands ~90 lines later, so it COMPILES and the transactor
        //     silently uses the const instead of the `#(...)` argument.
        //     Byte-identical to the const-only source with no parameter
        //     at all, which is what makes the argument provably
        //     invisible rather than merely undetected.
        //   * referenced with no const to fall back on, or from a field
        //     default (emitted INSIDE the struct, ahead of the const) —
        //     an undeclared name, so it does not compile.
        //
        // `SilentlyMisLowers` is the worst of these and so the label.
        return Err(not_implemented(
            &format!("transactor `{tname}` with generic parameters"),
            "v1 drops the parameter list entirely: an unused parameter vanishes along \
             with any `#(...)` argument at the instantiation, and a reference to one \
             either fails to resolve or silently picks up a same-named file-scope \
             `const`, depending on where in the emitted file the reference lands",
            V1Status::SilentlyMisLowers,
        ));
    }
    if t.bound_to.is_some() {
        // Two bound-to forms, distinguished by item shape:
        //   * `hookable` methods that drive the bound bus's handshake
        //     channels → the INITIATOR-side BFM (regblock `via`
        //     helpers). The methods are test-called.
        //   * `thread bus.<method>(...)` responders → the target-side
        //     TLM actor (request-served). #371.
        // A bound-to transactor with any `hookable` is the initiator
        // form; one with target threads is the target form. (A file
        // mixing both is rejected inside the initiator path.)
        let has_hookable = t
            .items
            .iter()
            .chain(t.when_active.iter().flatten())
            .any(|ci| matches!(ci, ComponentItem::Hookable(_)));
        if has_hookable {
            return lower_bound_initiator_transactor(
                t,
                next_fn,
                helper_registry,
                record_ctx,
                buses,
                side_tables,
            );
        }
        return lower_bound_target_transactor(
            t,
            next_fn,
            helper_registry,
            record_ctx,
            buses,
            downstream_binds,
            side_tables,
        );
    }

    // Walk always-on items then the `when active` body — the same
    // flattening v1's `synth_component_from_transactor` performs with
    // include_active = true. We still preserve whether a method came
    // from `when active`, because an always-on sibling must not
    // backdoor-call an active-only one.
    let mut dut: Option<(String, String)> = None; // (field, module type)
    let mut methods_ast: Vec<(&HookableMethod, bool)> = Vec::new();
    // Persistent scalar state fields (`last_read : uint<32> default 0`)
    // materialize on a per-instance state struct, exactly like the
    // bound-to target form. Method bodies read/write them by bare name
    // (routed to `TransactorState`/`TransactorStateWrite` via the
    // builder's `target_state_fields` set); the test reads them back as
    // `<instance>.<field>`.
    let mut state_fields: Vec<StateFieldSchema> = Vec::new();
    let mut state_names: HashMap<String, StateFieldKind> = HashMap::new();
    for ci in &t.items {
        match ci {
            ComponentItem::Hookable(h) => methods_ast.push((h, false)),
            ComponentItem::Field(f) => {
                let fname = &f.name.name;
                if f.direction.is_some() {
                    return Err(unsupported(
                        &format!("transactor `{tname}` event/directional field `{fname}`"),
                        "event-driven transactors await the event slice",
                    ));
                }
                if let TypeExpr::Named { name, .. } = &f.ty {
                    let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                    // A whole value-record held as transactor state
                    // (`cur : Beat`). Same schema the bound-to path
                    // produces, and the same duplicate check the scalar
                    // branch below makes — without it `cur : uint<32>`
                    // plus `cur : Beat` emitted two `cur` members.
                    if record_ctx.record_ids.contains_key(simple) {
                        let sf = lower_state_field(tname, f, &record_ctx.record_ids, record_ctx)?;
                        if state_names
                            .insert(sf.name.clone(), sf.kind.clone())
                            .is_some()
                        {
                            return Err(LowerError::Invalid(format!(
                                "transactor `{tname}` declares state field `{}` more than once",
                                sf.name
                            )));
                        }
                        state_fields.push(sf);
                        continue;
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
                } else {
                    let sf = lower_state_field(tname, f, &record_ctx.record_ids, record_ctx)?;
                    if state_names
                        .insert(sf.name.clone(), sf.kind.clone())
                        .is_some()
                    {
                        return Err(LowerError::Invalid(format!(
                            "transactor `{tname}` declares state field `{}` more than once",
                            sf.name
                        )));
                    }
                    state_fields.push(sf);
                }
            }
            ComponentItem::OnHandler(_) => {
                return Err(unsupported(
                    &format!("transactor `{tname}` `on` handlers"),
                    "event-driven transactors await the event slice",
                ));
            }
            ComponentItem::TargetTlmThread(_) => {
                // v1 emits NOTHING for a target thread on an unbound
                // transactor: its C++ is byte-identical with and without
                // the `thread` item. The negative anchor is the bound-to
                // TARGET form, where the same item changes 42 lines — so
                // v1 implements target threads where it owns them, and
                // silently drops this one.
                return Err(not_implemented(
                    &format!("transactor `{tname}` TLM target threads"),
                    "a target thread is served through a `bound to <bus>` transactor; \
                     on an unbound one v1 discards it silently",
                    V1Status::SilentlyMisLowers,
                ));
            }
            ComponentItem::Watchdog(_) => {
                // v1 emits a complete `<T>_watchdog` lambda — pre/post
                // hook vectors, the `max_idle` check against
                // `_last_in_cycle`/`_last_out_cycle`, the FAIL line, the
                // error bump — and then never calls it. An AGENT
                // watchdog gets a periodic `_checkers` closure installed
                // at its instantiation site (`Producer_watchdog(_tb.prod)`);
                // a transactor watchdog gets no call site at all, in the
                // outer, `when active`, and passive landings alike. So
                // the construct compiles under v1 and the watchdog
                // silently never fires — the worst outcome available,
                // and not something to point a user at.
                return Err(not_implemented(
                    &format!("transactor `{tname}` watchdogs"),
                    "v1 emits the watchdog body but never schedules it, so it never \
                     fires; declare the watchdog on an `agent` instead",
                    V1Status::SilentlyMisLowers,
                ));
            }
            ComponentItem::Connect(_) => {
                return Err(not_implemented(
                    &format!("transactor `{tname}` connect blocks"),
                    "v1 parses the block and emits NOTHING for it — the edges are silently \
                     dropped; wire the endpoints from an `env` `connect` instead",
                    V1Status::SilentlyMisLowers,
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
    for ci in t.when_active.iter().flatten() {
        match ci {
            ComponentItem::Field(f) => {
                let fname = &f.name.name;
                if f.direction.is_some() {
                    return Err(unsupported(
                        &format!("transactor `{tname}` event/directional field `{fname}`"),
                        "event-driven transactors await the event slice",
                    ));
                }
                if let TypeExpr::Named { name, .. } = &f.ty {
                    let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                    // Record state is legal in BOTH declaration
                    // positions — v1 compiles a `cur : Beat` written
                    // inside `when active` exactly as it does one
                    // written above it, so closing only the outer
                    // position would leave half a feature.
                    if record_ctx.record_ids.contains_key(simple) {
                        let sf = lower_state_field(tname, f, &record_ctx.record_ids, record_ctx)?;
                        if state_names
                            .insert(sf.name.clone(), sf.kind.clone())
                            .is_some()
                        {
                            return Err(LowerError::Invalid(format!(
                                "transactor `{tname}` declares state field `{}` more than once",
                                sf.name
                            )));
                        }
                        state_fields.push(sf);
                        continue;
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
                } else {
                    let sf = lower_state_field(tname, f, &record_ctx.record_ids, record_ctx)?;
                    if state_names
                        .insert(sf.name.clone(), sf.kind.clone())
                        .is_some()
                    {
                        return Err(LowerError::Invalid(format!(
                            "transactor `{tname}` declares state field `{}` more than once",
                            sf.name
                        )));
                    }
                    state_fields.push(sf);
                }
            }
            ComponentItem::Hookable(h) => methods_ast.push((h, true)),
            ComponentItem::OnHandler(_) => {
                return Err(unsupported(
                    &format!("transactor `{tname}` `on` handlers"),
                    "event-driven transactors await the event slice",
                ));
            }
            ComponentItem::TargetTlmThread(_) => {
                // v1 emits NOTHING for a target thread on an unbound
                // transactor: its C++ is byte-identical with and without
                // the `thread` item. The negative anchor is the bound-to
                // TARGET form, where the same item changes 42 lines — so
                // v1 implements target threads where it owns them, and
                // silently drops this one.
                return Err(not_implemented(
                    &format!("transactor `{tname}` TLM target threads"),
                    "a target thread is served through a `bound to <bus>` transactor; \
                     on an unbound one v1 discards it silently",
                    V1Status::SilentlyMisLowers,
                ));
            }
            ComponentItem::Watchdog(_) => {
                // v1 emits a complete `<T>_watchdog` lambda — pre/post
                // hook vectors, the `max_idle` check against
                // `_last_in_cycle`/`_last_out_cycle`, the FAIL line, the
                // error bump — and then never calls it. An AGENT
                // watchdog gets a periodic `_checkers` closure installed
                // at its instantiation site (`Producer_watchdog(_tb.prod)`);
                // a transactor watchdog gets no call site at all, in the
                // outer, `when active`, and passive landings alike. So
                // the construct compiles under v1 and the watchdog
                // silently never fires — the worst outcome available,
                // and not something to point a user at.
                return Err(not_implemented(
                    &format!("transactor `{tname}` watchdogs"),
                    "v1 emits the watchdog body but never schedules it, so it never \
                     fires; declare the watchdog on an `agent` instead",
                    V1Status::SilentlyMisLowers,
                ));
            }
            ComponentItem::Connect(_) => {
                return Err(not_implemented(
                    &format!("transactor `{tname}` connect blocks"),
                    "v1 parses the block and emits NOTHING for it — the edges are silently \
                     dropped; wire the endpoints from an `env` `connect` instead",
                    V1Status::SilentlyMisLowers,
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
        bound_bus: None,
        state_fields,
        target_methods: Vec::new(),
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
        allow_scheduler_time_waits: true,
        record_ids: record_ctx.record_ids.clone(),
        records: record_ctx.records.clone(),
        // Method bodies see neither bus bindings nor sibling transactor
        // instances — both are test-scope; nested call edges stay out
        // of method bodies structurally.
        bus_bindings: HashMap::new(),
        bus_remaps: HashMap::new(),
        transactor_fields: HashMap::new(),
        passive_transactor_fields: std::collections::HashSet::new(),
        transactors: Vec::new(),
        // Method bodies see no scoreboards either — scoreboards are
        // test-scope testbench fields, structurally invisible here.
        scoreboard_fields: HashMap::new(),
        scoreboards: Vec::new(),
        // Method bodies see file-scope consts; they have no testbench,
        // so no scalar fields, helper methods, or test-scope lets.
        consts: record_ctx.consts.clone(),
        properties: record_ctx.properties.clone(),
        owner: None,
        const_signed: record_ctx.const_signed.clone(),
        tb_scalar_fields: HashSet::new(),
        tb_queue_fields: HashMap::new(),
        tb_record_fields: Vec::new(),
        regblock_callbacks: HashMap::new(),
        tb_methods: HashMap::new(),
        test_scope_lets: HashSet::new(),
        regblock_bindings: HashMap::new(),
        regblock_init_order: Vec::new(),
        addrmap_bindings: HashMap::new(),
        addrmap_init_order: Vec::new(),
        bare_transactor_fields: HashSet::new(),
        target_state: HashMap::new(),
        components: Vec::new(),
        component_fields: HashMap::new(),
        // A method body could host `randomize`, but the constraint-IR
        // problem table only catalogs test/tseq sites — so these stay
        // empty and a method-body `randomize` lowers with no problem-id
        // (the nullptr-descriptor fallback, matching v1).
        txn_keeps: HashMap::new(),
        randomize_problem_ids: HashMap::new(),
        tseqs: HashMap::new(),
        // Transactor-context lowering never resolves test-scope probes.
        probes: HashMap::new(),
        extern_fns: record_ctx.extern_fns.clone(),
    };

    let mut funcs = Vec::new();
    let mut sibling_methods = HashMap::new();
    for (h, active_only) in &methods_ast {
        let mname = h.name.name.clone();
        if sibling_methods
            .insert(
                mname.clone(),
                (
                    h.params
                        .iter()
                        .map(|p| p.name.name.clone())
                        .collect::<Vec<_>>(),
                    h.return_ty.is_some(),
                    *active_only,
                ),
            )
            .is_some()
        {
            return Err(LowerError::Invalid(format!(
                "transactor `{tname}` declares method `{mname}` more than once"
            )));
        }
    }
    for (h, active_only) in methods_ast {
        let mname = &h.name.name;
        check_scalar_ty(tname, mname, "return type", h.return_ty.as_ref())?;

        let fid = FunctionId(next_fn.0 + funcs.len() as u32);
        let mut b = FuncBuilder::new(&method_ctx, helper_registry, side_tables);
        b.in_transactor_method = true;
        b.self_transactor = Some(tname.clone());
        b.self_transactor_methods = sibling_methods.clone();
        b.self_transactor_method_active_only = active_only;
        b.current_body_name = Some(mname.clone());
        // Bare-name reads/writes of a state field route to
        // `TransactorState`/`TransactorStateWrite` with an empty instance
        // placeholder, filled at test-binding time (same as the bound-to
        // target form). Method params shadow state names (declared below,
        // looked up first), so this is safe to set up front.
        b.target_state_fields = state_names.clone();
        let mut params = Vec::with_capacity(h.params.len());
        for p in &h.params {
            let ty = method_param_ir_type(tname, mname, p, &method_ctx.record_ids)?;
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
            param_names: f.params.iter().map(|p| p.name.clone()).collect(),
            has_ret: f.ret.is_some(),
            active_only,
            pre_hooks: Vec::new(),
            post_hooks: Vec::new(),
            cov_hook_subs: Vec::new(),
        });
        funcs.push(f);
    }

    Ok((schema, funcs))
}

/// Lower a bound-to target-side TLM transactor (`transactor X bound to
/// <Bus>`): collect persistent scalar state fields and lower each
/// `thread bus.<method>(...)` responder body into a `TbFunction` (kind
/// `TransactorBody`) whose state accesses reference the state fields by
/// bare name (the instance is filled at test-binding time).
///
/// Subset gate: only `blocking` `tlm_method`s are SERVED here.
/// Target-side `out_of_order tags N` responder lanes (hidden tag wires /
/// multi-lane response routers) and `fork`-based responder workers
/// (a responder re-issuing a downstream TLM call) are rejected precisely
/// — both are a follow-up slice. (Initiator-side `fork`/`join_all` over
/// bus methods — test-scope `let x = fork mem.read_ooo(...)` — IS
/// lowered; see `bus::try_lower_tlm_fork`.)
fn lower_bound_target_transactor(
    t: &TransactorDecl,
    next_fn: FunctionId,
    helper_registry: &helpers::HelperRegistry<'_>,
    record_ctx: &LowerCtx,
    buses: &HashMap<String, &BusDecl>,
    downstream_binds: &HashMap<String, BusDecl>,
    side_tables: &RefCell<SideTables>,
) -> Result<(TransactorSchema, Vec<TbFunction>), LowerError> {
    let tname = &t.name.name;
    // Resolve the bound bus.
    let bus_name = match t.bound_to.as_ref() {
        Some(TypeExpr::Named { name, generics, .. }) => {
            if !generics.is_empty() {
                return Err(unsupported(
                    &format!("transactor `{tname}` bound to a generic-applied bus type"),
                    "",
                ));
            }
            name.segments
                .last()
                .map(|s| s.name.clone())
                .unwrap_or_default()
        }
        _ => {
            return Err(unsupported(
                &format!("transactor `{tname}` bound to a non-named bus type"),
                "",
            ));
        }
    };
    let Some(bus) = buses.get(&bus_name) else {
        return Err(LowerError::Invalid(format!(
            "transactor `{tname}` is bound to `{bus_name}`, which is not a `bus` declaration"
        )));
    };

    // Walk items (and the optional `when active` body, though target
    // responders live as always-on items): collect state fields and
    // target threads; reject the out-of-subset shapes precisely.
    let mut state_fields: Vec<StateFieldSchema> = Vec::new();
    let mut state_names: HashMap<String, StateFieldKind> = HashMap::new();
    let mut threads_ast: Vec<&TargetTlmThread> = Vec::new();
    let all_items = t.items.iter().chain(t.when_active.iter().flatten());
    for ci in all_items {
        match ci {
            ComponentItem::Field(f) => {
                let sf = lower_state_field(tname, f, &record_ctx.record_ids, record_ctx)?;
                if state_names
                    .insert(sf.name.clone(), sf.kind.clone())
                    .is_some()
                {
                    return Err(LowerError::Invalid(format!(
                        "transactor `{tname}` declares state field `{}` more than once",
                        sf.name
                    )));
                }
                state_fields.push(sf);
            }
            ComponentItem::TargetTlmThread(th) => threads_ast.push(th),
            ComponentItem::Hookable(h) => {
                return Err(unsupported(
                    &format!(
                        "bound-to transactor `{tname}` `hookable {}` (initiator-side method)",
                        h.name.name
                    ),
                    "the bus-bound BFM (initiator) form — driving handshake channels from \
                     hookable bodies — is a follow-up slice; only target-side `thread \
                     bus.<m>(...)` responders are lowered",
                ));
            }
            ComponentItem::OnHandler(_) => {
                // Reachable ONLY for a PERIODIC handler. `transactor_is_
                // component` returns `has_on_handler` for every bound-to
                // transactor, and that flag is set by NON-periodic
                // handlers alone — so an event subscriber, a
                // `bus.<ch>.handshake` monitor and a cycle-trigger all
                // route to the composite table and never arrive here.
                // `on <N> cycles` is the one shape that falls through.
                //
                // Which is why the old detail — "event-driven
                // transactors await the event slice" — named a
                // construct no program reaching this arm can contain.
                //
                // v1 emits a `_checkers` closure holding a `static
                // ..._last` stamp and the period, firing the body every
                // N cycles against the instance's state struct. Whether
                // that output COMPILES depends on where the period
                // expression's names land in the emitted file, and the
                // registration sits near the top of the run function:
                //
                //   * `on 5 cycles`, `on 2 + 3 cycles` — literals, fine.
                //   * `on NPER cycles` for a file-scope `const` —
                //     emitted at namespace scope ~80 lines earlier,
                //     fine.
                //   * `on read_count cycles` for a transactor state
                //     field — the instance is declared three lines
                //     earlier, fine.
                //   * `on limit cycles` for a `let` declared AFTER the
                //     transactor's own binding — emitted ~64 lines
                //     LATER. g++: "'limit' was not declared in this
                //     scope". `<N>` is any integer expression per spec
                //     §7.10, and the name resolver does not visit a
                //     bound-to transactor's `on` trigger, so this
                //     reaches here and type-checks.
                //   * the SAME `let` moved one line ABOVE that binding
                //     — emitted three lines before the registration,
                //     so it compiles and runs at the right rate (built
                //     and run: 4 firings in 21 cycles at period 5).
                //     The discriminator is the `let`'s position
                //     relative to the binding, not "an impl-scope
                //     `let`" as a category, and the detail below says
                //     so; a first version asserted the whole category
                //     and was false for this row.
                //   * `on limit cycles` again, with a file-scope
                //     `const limit` ALSO in the program — the worst
                //     one, and why this arm is not merely
                //     uncompilable. The closure resolves to the
                //     `constexpr` at namespace scope, so it COMPILES;
                //     the rest of the run body sees the `let` that
                //     shadows it. Built and RUN: `const limit = 7`
                //     with `let limit = 5` fires the handler twice in
                //     21 cycles instead of four. The handler runs at a
                //     rate the program never asks for, and nothing
                //     says so.
                //
                // So the discriminator is name resolution in the
                // emitted C++, not the shape of the trigger — the same
                // thing that defeated a syntactic split on the
                // scoreboard wiring arm, and the same silent
                // const-capture the transactor-parameter arm at the top
                // of this file reports. An arm's status is the worst
                // thing v1 does anywhere under it, so the whole arm is
                // `SilentlyMisLowers`, and the literal case pays for it
                // by losing a suggestion it would have deserved.
                //
                // Separately measured and not a gap: on a `passive`
                // instance a `when active`-scoped periodic handler is
                // correctly dropped — output byte-identical to the same
                // program without it. That is v1 obeying `when active`.
                return Err(not_implemented(
                    &format!("bound-to transactor `{tname}` periodic `on <N> cycles` handlers"),
                    "v1 emits a cycle-stamped checker closure, but registers it ahead of the \
                     transactor's own binding, so a period naming a `let` declared AFTER \
                     that binding either fails to compile or silently picks up a same-named \
                     file-scope `const` and runs at the wrong rate; a non-periodic `on` \
                     never reaches this path",
                    V1Status::SilentlyMisLowers,
                ));
            }
            ComponentItem::Watchdog(_) => {
                // Same rule as the unbound flavor: v1 emits the
                // watchdog body and never schedules it.
                return Err(not_implemented(
                    &format!("bound-to transactor `{tname}` watchdogs"),
                    "v1 emits the watchdog body but never schedules it, so it never \
                     fires; declare the watchdog on an `agent` instead",
                    V1Status::SilentlyMisLowers,
                ));
            }
            ComponentItem::Connect(_) => {
                return Err(not_implemented(
                    &format!("bound-to transactor `{tname}` connect blocks"),
                    "v1 parses the block and emits NOTHING for it — the edges are silently \
                     dropped",
                    V1Status::SilentlyMisLowers,
                ));
            }
            ComponentItem::Lifecycle(..) | ComponentItem::Apply(_) => {
                return Err(unsupported(
                    &format!("bound-to transactor `{tname}` lifecycle/apply items"),
                    "",
                ));
            }
        }
    }
    if threads_ast.is_empty() {
        return Err(unsupported(
            &format!(
                "bound-to transactor `{tname}` without any `thread bus.<method>(...)` responder"
            ),
            "a target-side TLM transactor must serve at least one bus method",
        ));
    }

    let mut schema = TransactorSchema {
        name: tname.clone(),
        // A bound target transactor has no private DUT handle; the
        // responder drives the bound bus's wires on the test DUT.
        dut_field: String::new(),
        dut_type: String::new(),
        methods: Vec::new(),
        bound_bus: Some(bus_name.clone()),
        state_fields,
        target_methods: Vec::new(),
    };

    // Responder bodies see file-scope consts and records; no testbench,
    // no sibling instances. State fields resolve via
    // `FuncBuilder::target_state_fields`, not the ctx.
    //
    // Downstream bus bindings ARE visible: a responder may re-issue a
    // TLM call against a test-scope bus binding (nested forwarding —
    // `let raw = back.read(addr)`, or `let d = fork back.read_ooo(addr)`
    // + `join_all`). The pre-scanned `name -> BusDecl` map makes `back`
    // resolve through the SAME initiator-side call machinery the run/check
    // body uses: `try_lower_bus_call` (blocking → a `TransactorMethod`
    // call edge) or `try_lower_tlm_fork` (`out_of_order` → `Stmt::TlmFork`
    // / `TlmJoinAll`), instead of the generic transactor-method
    // rejection. Either edge is resolved against the test's `bus_bindings`
    // at emit (the bound responder runs in test scope, where every
    // binding is live). What this does NOT enable is the responder
    // SERVING an `out_of_order` method (the OOO-RESPONDER LANE form,
    // gated below) — that is the multi-lane dispatcher/arbiter, a
    // follow-up slice distinct from re-issuing a downstream OOO call.
    let body_ctx = LowerCtx {
        dut_field: String::new(),
        tb_field: None,
        cov_fields: HashMap::new(),
        covgroups: Vec::new(),
        clock_names: Vec::new(),
        allow_scheduler_time_waits: false,
        record_ids: record_ctx.record_ids.clone(),
        records: record_ctx.records.clone(),
        bus_bindings: downstream_binds.clone(),
        // Responder bodies carry the placeholder bus prefix; remaps are
        // applied at bind time by `fill_initiator_bus_prefix`.
        bus_remaps: HashMap::new(),
        transactor_fields: HashMap::new(),
        passive_transactor_fields: std::collections::HashSet::new(),
        transactors: Vec::new(),
        scoreboard_fields: HashMap::new(),
        scoreboards: Vec::new(),
        consts: record_ctx.consts.clone(),
        properties: record_ctx.properties.clone(),
        owner: None,
        const_signed: record_ctx.const_signed.clone(),
        tb_scalar_fields: HashSet::new(),
        tb_queue_fields: HashMap::new(),
        tb_record_fields: Vec::new(),
        regblock_callbacks: HashMap::new(),
        tb_methods: HashMap::new(),
        test_scope_lets: HashSet::new(),
        regblock_bindings: HashMap::new(),
        regblock_init_order: Vec::new(),
        addrmap_bindings: HashMap::new(),
        addrmap_init_order: Vec::new(),
        bare_transactor_fields: HashSet::new(),
        target_state: HashMap::new(),
        components: Vec::new(),
        component_fields: HashMap::new(),
        // Responder bodies are not cataloged in the constraint-IR
        // problem table; a `randomize` here lowers with no problem-id.
        txn_keeps: HashMap::new(),
        randomize_problem_ids: HashMap::new(),
        tseqs: HashMap::new(),
        // Transactor-context lowering never resolves test-scope probes.
        probes: HashMap::new(),
        extern_fns: record_ctx.extern_fns.clone(),
    };

    let mut funcs = Vec::new();
    for th in threads_ast {
        // `thread bus.<method>(...)`: the method path is `bus.<method>`.
        let segs: Vec<&str> = th.method.segments.iter().map(|s| s.name.as_str()).collect();
        if segs.len() != 2 || segs[0] != "bus" {
            return Err(unsupported(
                &format!(
                    "transactor `{tname}` target thread `{}` (expected `thread bus.<method>(...)`)",
                    segs.join(".")
                ),
                "",
            ));
        }
        let mname = segs[1];
        if schema.target_methods.iter().any(|m| m.name == mname) {
            return Err(LowerError::Invalid(format!(
                "transactor `{tname}` declares target thread `bus.{mname}` more than once"
            )));
        }
        // The bus must declare a matching `tlm_method`. Both `blocking`
        // (single in-order responder coroutine) and `out_of_order tags N`
        // (N-lane dispatcher/lane/arbiter topology) are SERVED here; for
        // the latter we fold and range-check the literal tag count, which
        // emission threads into the multi-lane actor generation.
        let Some(method) = bus.tlm_methods.iter().find(|m| m.name.name == mname) else {
            return Err(LowerError::Invalid(format!(
                "transactor `{tname}` target thread `bus.{mname}`: bus `{bus_name}` has no \
                 `tlm_method {mname}`"
            )));
        };
        let ooo_tags = match method.mode.name.as_str() {
            "blocking" => None,
            "out_of_order" => {
                // The bus-level parser already requires `tags N` on an
                // `out_of_order` method, but re-check defensively.
                let Some(tags_expr) = method.out_of_order_tags.as_ref() else {
                    return Err(LowerError::Invalid(format!(
                        "transactor `{tname}` target thread `bus.{mname}`: `out_of_order` \
                         method has no `tags N` count"
                    )));
                };
                let Some(n) = super::exprs::parse_int_literal_expr(tags_expr) else {
                    return Err(unsupported(
                        &format!(
                            "transactor `{tname}` target thread `bus.{mname}`: \
                             `out_of_order tags <N>` requires a literal tag count for \
                             responder-lane lowering"
                        ),
                        "use an integer literal (`out_of_order tags 2`)",
                    ));
                };
                if n == 0 || n > 64 {
                    return Err(LowerError::Invalid(format!(
                        "transactor `{tname}` target thread `bus.{mname}`: supports \
                         1..64 out_of_order target tags, got {n}"
                    )));
                }
                Some(n)
            }
            other => {
                return Err(unsupported(
                    &format!(
                        "transactor `{tname}` target thread `bus.{mname}` serving a `{other}` method"
                    ),
                    "target-side TLM responders support `blocking` and `out_of_order tags N`",
                ));
            }
        };
        if th.params.len() != method.args.len() {
            return Err(LowerError::Invalid(format!(
                "transactor `{tname}` target thread `bus.{mname}`: expected {} arg(s), got {}",
                method.args.len(),
                th.params.len()
            )));
        }
        for (p, name) in th.params.iter().zip(method.args.iter()) {
            check_scalar_ty(
                tname,
                mname,
                &format!("parameter `{}`", p.name.name),
                p.ty.as_ref(),
            )?;
            // Cross-check the declared widths fit the u64 value model via
            // the bus method's declared arg type too.
            check_scalar_ty(
                tname,
                mname,
                &format!("argument `{}`", name.0.name),
                Some(&name.1),
            )?;
        }
        // The return type may be a record (`-> HarcBurstResp32x4`): the
        // responder builds it field-wise and the backend packs it onto
        // the response pin (`harc_drive_<R>`). A scalar return goes
        // through the ≤64-bit gate; any other non-scalar type is
        // rejected.
        let ret_record = method
            .ret
            .as_ref()
            .and_then(|t| record_id_of_type(&body_ctx, t));
        if let Some(ret) = method.ret.as_ref() {
            if ret_record.is_none() {
                check_scalar_ty(tname, mname, "return type", Some(ret))?;
            }
        }

        let fid = FunctionId(next_fn.0 + funcs.len() as u32);
        let mut b = FuncBuilder::new(&body_ctx, helper_registry, side_tables);
        b.target_state_fields = state_names.clone();
        let mut params = Vec::with_capacity(th.params.len());
        for p in &th.params {
            let ty = helpers::ir_type_of(p.ty.as_ref());
            let local = b.declare(&p.name.name);
            b.set_local_type(local, ty.clone());
            params.push(TypedParam {
                name: p.name.name.clone(),
                ty,
            });
        }
        let has_ret = method.ret.is_some();
        if has_ret {
            let ret = b.declare("__ret");
            // A record return slot carries its record type so the
            // backend drives it through the pack helper, and so a
            // `return <record-local>` type-checks (whole-record copy).
            if let Some(rid) = ret_record {
                b.set_local_type(ret, crate::ir::IrType::Record(rid));
            }
            b.helper_ret = Some(ret);
        }
        b.lower_block_stmts(&th.body)?;
        if !b.is_terminated() {
            b.terminate(Terminator::Return);
        }
        let mut f = b.finish(
            fid,
            format!("{tname}_target_{mname}"),
            FunctionKind::TransactorBody {
                transactor: TransactorId(0),
            },
            None,
        )?;
        // The transactor id is fixed up by the caller's push order; the
        // body never reads its own kind's id, so the placeholder is inert.
        if let FunctionKind::TransactorBody { transactor } = &mut f.kind {
            *transactor = TransactorId(record_ctx.transactors.len() as u32);
        }
        f.params = params;
        schema.target_methods.push(TargetTlmMethodSchema {
            name: mname.to_string(),
            function: fid,
            args: method.args.iter().map(|(n, _)| n.name.clone()).collect(),
            has_ret,
            ooo_tags,
        });
        funcs.push(f);
    }

    Ok((schema, funcs))
}

/// Placeholder bus-binding prefix used while lowering an initiator-side
/// BFM method body, before the test's `let helper = bind <axil>` names
/// the real binding. This is the bare `bus` keyword the BFM body uses to
/// name its bound bus (matching v1's `driver_bus_for_hookables`, where
/// `bus` inside a hookable resolves to the parent's bus binding): the
/// method-body `bus_bindings` map is keyed by it, so every
/// `bus.<ch>.<sig>` access lowers to a `PortRef` whose first path segment
/// is this string. The test-binding stage rewrites that segment to the
/// bound bus binding's name (the arch-com §19.6 flat prefix). It is the
/// flat prefix only inside the (instance-less) method body, so it cannot
/// collide with any test-scope binding.
pub(crate) const INITIATOR_BUS_PLACEHOLDER: &str = "bus";

/// Lower a bound-to **initiator-side** BFM transactor (`transactor X
/// bound to <Bus>` whose `hookable` methods drive the bound bus's
/// handshake channels). This is the regblock `via <Helper>` form and the
/// TLM-initiator BFM: each `hookable write(addr, data)` / `read(addr) ->
/// data` body issues bus requests via `bus.<ch>.send(...)` /
/// `bus.<ch>.recv()` / `bus.<ch>.<sig> = ...` and returns a response.
///
/// Each method lowers to a `TbFunction` (kind `TransactorBody`), exactly
/// like the unbound DUT-poking BFM — the schema records them on
/// `methods` (NOT `target_methods`), so a regblock frontdoor's
/// `Helper.write`/`Helper.read` call edges (#369) and bare
/// `helper.method(...)` calls resolve through the same
/// `CallTarget::TransactorMethod` dispatch. Inside the body, `bus`
/// resolves (via a `bus_bindings` entry keyed by the placeholder prefix)
/// to the bound `BusDecl`, so the existing channel-handshake lowering
/// (`lower_handshake_send`/`recv`, CFG-inlined to v1's 16-cycle-budget
/// valid/ready dance) applies verbatim. The placeholder bus prefix is
/// filled with the real binding name at test-binding time
/// (`fill_initiator_bus_prefix`).
///
/// Persistent scalar state fields (`last_read : uint<32> default 0`)
/// materialize on a per-instance state struct, exactly like the bound-to
/// target and unbound DUT-poking forms: method bodies read/write them by
/// bare name and the test reads them back as `<instance>.<field>`. The
/// per-instance state map and the body `TransactorState` placeholders are
/// filled at test-binding time, alongside the bus prefix.
///
/// Method waits keep v1's synchronous hookable semantics (the tbir
/// backend emits them as `tick()` loops). Out of subset, rejected
/// precisely: event/directional fields (`in event<T>` + `on <ev>` driving
/// the bound bus — a follow-up slice), `out_of_order` channels,
/// `fork`-issue, `bind ... with { ... }` remaps, and nested transactor
/// calls.
fn lower_bound_initiator_transactor(
    t: &TransactorDecl,
    next_fn: FunctionId,
    helper_registry: &helpers::HelperRegistry<'_>,
    record_ctx: &LowerCtx,
    buses: &HashMap<String, &BusDecl>,
    side_tables: &RefCell<SideTables>,
) -> Result<(TransactorSchema, Vec<TbFunction>), LowerError> {
    let tname = &t.name.name;
    let bus_name = match t.bound_to.as_ref() {
        Some(TypeExpr::Named { name, generics, .. }) => {
            if !generics.is_empty() {
                return Err(unsupported(
                    &format!("transactor `{tname}` bound to a generic-applied bus type"),
                    "",
                ));
            }
            name.segments
                .last()
                .map(|s| s.name.clone())
                .unwrap_or_default()
        }
        _ => {
            return Err(unsupported(
                &format!("transactor `{tname}` bound to a non-named bus type"),
                "",
            ));
        }
    };
    let Some(bus) = buses.get(&bus_name) else {
        return Err(LowerError::Invalid(format!(
            "transactor `{tname}` is bound to `{bus_name}`, which is not a `bus` declaration"
        )));
    };

    // Walk always-on + `when active` items: collect the hookable
    // methods and persistent scalar state fields; reject every
    // out-of-subset item shape precisely.
    let mut methods_ast: Vec<(&HookableMethod, bool)> = Vec::new();
    // Persistent scalar state fields (`last_read : uint<32> default 0`)
    // materialize on a per-instance state struct, exactly like the
    // bound-to target and unbound DUT-poking forms. Method bodies
    // read/write them by bare name (routed to `TransactorState`/
    // `TransactorStateWrite` via the builder's `target_state_fields`
    // set); the test reads them back as `<instance>.<field>`. The
    // per-instance state map + body placeholders are filled at
    // test-binding time, alongside the bus prefix.
    let mut state_fields: Vec<StateFieldSchema> = Vec::new();
    let mut state_names: HashMap<String, StateFieldKind> = HashMap::new();
    for ci in &t.items {
        match ci {
            ComponentItem::Hookable(h) => methods_ast.push((h, false)),
            ComponentItem::Field(f) => {
                // An event/directional field (`req : in event<T>`) on a
                // bound-to transactor is the event-driven driver form —
                // it routes to the component path, which does not yet
                // carry the bound-bus handshake context. Reject it
                // precisely (the unbound event-driven form is #382;
                // bound-to event-driven is a follow-up slice).
                if f.direction.is_some() {
                    return Err(unsupported(
                        &format!(
                            "initiator-side bound-to transactor `{tname}` event/directional \
                             field `{}`",
                            f.name.name
                        ),
                        "event-driven bound-to transactors (`in event<T>` + `on <ev>` driving \
                         the bound bus) are a follow-up slice; only `hookable`-method BFMs \
                         with scalar state are lowered",
                    ));
                }
                // A scalar persistent state field (`uint<N>`/`sint<N>`/
                // `bool` ≤64 bits with a plain-literal default). Reuse the
                // bound-to target state-field lowering; reject module/
                // transaction-typed and non-scalar fields inside it.
                let sf = lower_state_field(tname, f, &record_ctx.record_ids, record_ctx)?;
                if state_names
                    .insert(sf.name.clone(), sf.kind.clone())
                    .is_some()
                {
                    return Err(LowerError::Invalid(format!(
                        "initiator-side bound-to transactor `{tname}` declares state field \
                         `{}` more than once",
                        sf.name
                    )));
                }
                state_fields.push(sf);
            }
            ComponentItem::TargetTlmThread(th) => {
                return Err(unsupported(
                    &format!(
                        "initiator-side bound-to transactor `{tname}` mixing a `thread {}` \
                         responder with `hookable` initiator methods",
                        th.method
                            .segments
                            .iter()
                            .map(|s| s.name.as_str())
                            .collect::<Vec<_>>()
                            .join(".")
                    ),
                    "a transactor is either an initiator BFM (hookable methods) or a \
                     target responder (thread bus.<m> bodies), not both",
                ));
            }
            ComponentItem::OnHandler(_) => {
                // Periodic-only, for the same reason as the target-side
                // arm: `transactor_is_component` routes every
                // non-periodic `on` on a bound-to transactor to the
                // composite table, so `on <N> cycles` is the sole shape
                // that arrives. Measured at both positions (always-on
                // items and `when active`) — v1 emits the same
                // cycle-stamped `_checkers` closure either way, and
                // registers it in the same place, so it inherits the
                // same period-expression scoping problem. See the
                // target-side arm for the five measured rows.
                return Err(not_implemented(
                    &format!(
                        "initiator-side bound-to transactor `{tname}` periodic \
                         `on <N> cycles` handlers"
                    ),
                    "v1 emits a cycle-stamped checker closure, but registers it ahead of the \
                     transactor's own binding, so a period naming a `let` declared AFTER \
                     that binding either fails to compile or silently picks up a same-named \
                     file-scope `const` and runs at the wrong rate; a non-periodic `on` \
                     never reaches this path",
                    V1Status::SilentlyMisLowers,
                ));
            }
            ComponentItem::Watchdog(_) => {
                // Same rule as the unbound flavor: v1 emits the
                // watchdog body and never schedules it.
                return Err(not_implemented(
                    &format!("initiator-side bound-to transactor `{tname}` watchdogs"),
                    "v1 emits the watchdog body but never schedules it, so it never \
                     fires; declare the watchdog on an `agent` instead",
                    V1Status::SilentlyMisLowers,
                ));
            }
            ComponentItem::Connect(_) => {
                return Err(not_implemented(
                    &format!("initiator-side bound-to transactor `{tname}` connect blocks"),
                    "v1 parses the block and emits NOTHING for it — the edges are silently \
                     dropped",
                    V1Status::SilentlyMisLowers,
                ));
            }
            ComponentItem::Lifecycle(..) | ComponentItem::Apply(_) => {
                return Err(unsupported(
                    &format!("initiator-side bound-to transactor `{tname}` lifecycle/apply items"),
                    "",
                ));
            }
        }
    }
    for ci in t.when_active.iter().flatten() {
        match ci {
            ComponentItem::Hookable(h) => methods_ast.push((h, true)),
            ComponentItem::Field(f) => {
                if f.direction.is_some() {
                    return Err(unsupported(
                        &format!(
                            "initiator-side bound-to transactor `{tname}` event/directional \
                             field `{}`",
                            f.name.name
                        ),
                        "event-driven bound-to transactors (`in event<T>` + `on <ev>` driving \
                         the bound bus) are a follow-up slice; only `hookable`-method BFMs \
                         with scalar state are lowered",
                    ));
                }
                let sf = lower_state_field(tname, f, &record_ctx.record_ids, record_ctx)?;
                if state_names
                    .insert(sf.name.clone(), sf.kind.clone())
                    .is_some()
                {
                    return Err(LowerError::Invalid(format!(
                        "initiator-side bound-to transactor `{tname}` declares state field \
                         `{}` more than once",
                        sf.name
                    )));
                }
                state_fields.push(sf);
            }
            ComponentItem::TargetTlmThread(th) => {
                return Err(unsupported(
                    &format!(
                        "initiator-side bound-to transactor `{tname}` mixing a `thread {}` \
                         responder with `hookable` initiator methods",
                        th.method
                            .segments
                            .iter()
                            .map(|s| s.name.as_str())
                            .collect::<Vec<_>>()
                            .join(".")
                    ),
                    "a transactor is either an initiator BFM (hookable methods) or a \
                     target responder (thread bus.<m> bodies), not both",
                ));
            }
            ComponentItem::OnHandler(_) => {
                // Periodic-only, for the same reason as the target-side
                // arm: `transactor_is_component` routes every
                // non-periodic `on` on a bound-to transactor to the
                // composite table, so `on <N> cycles` is the sole shape
                // that arrives. Measured at both positions (always-on
                // items and `when active`) — v1 emits the same
                // cycle-stamped `_checkers` closure either way, and
                // registers it in the same place, so it inherits the
                // same period-expression scoping problem. See the
                // target-side arm for the five measured rows.
                return Err(not_implemented(
                    &format!(
                        "initiator-side bound-to transactor `{tname}` periodic \
                         `on <N> cycles` handlers"
                    ),
                    "v1 emits a cycle-stamped checker closure, but registers it ahead of the \
                     transactor's own binding, so a period naming a `let` declared AFTER \
                     that binding either fails to compile or silently picks up a same-named \
                     file-scope `const` and runs at the wrong rate; a non-periodic `on` \
                     never reaches this path",
                    V1Status::SilentlyMisLowers,
                ));
            }
            ComponentItem::Watchdog(_) => {
                // Same rule as the unbound flavor: v1 emits the
                // watchdog body and never schedules it.
                return Err(not_implemented(
                    &format!("initiator-side bound-to transactor `{tname}` watchdogs"),
                    "v1 emits the watchdog body but never schedules it, so it never \
                     fires; declare the watchdog on an `agent` instead",
                    V1Status::SilentlyMisLowers,
                ));
            }
            ComponentItem::Connect(_) => {
                return Err(not_implemented(
                    &format!("initiator-side bound-to transactor `{tname}` connect blocks"),
                    "v1 parses the block and emits NOTHING for it — the edges are silently \
                     dropped",
                    V1Status::SilentlyMisLowers,
                ));
            }
            ComponentItem::Lifecycle(..) | ComponentItem::Apply(_) => {
                return Err(unsupported(
                    &format!("initiator-side bound-to transactor `{tname}` lifecycle/apply items"),
                    "",
                ));
            }
        }
    }
    if methods_ast.is_empty() {
        return Err(unsupported(
            &format!("initiator-side bound-to transactor `{tname}` without any `hookable` method"),
            "",
        ));
    }

    let mut schema = TransactorSchema {
        name: tname.clone(),
        // An initiator BFM drives the bound bus's wires on the test DUT;
        // it has no private DUT handle field.
        dut_field: String::new(),
        dut_type: String::new(),
        methods: Vec::new(),
        bound_bus: Some(bus_name.clone()),
        state_fields,
        target_methods: Vec::new(),
    };

    // Method bodies see the bound bus under the placeholder prefix (so
    // `bus.<ch>.send/recv` and `bus.<ch>.<sig>` lower through the
    // existing channel-handshake machinery), file-scope consts and
    // records, and nothing else (no DUT field, no testbench, no sibling
    // instances). The bus prefix is filled at test-binding time.
    let mut bus_bindings: HashMap<String, BusDecl> = HashMap::new();
    bus_bindings.insert(INITIATOR_BUS_PLACEHOLDER.to_string(), (*bus).clone());
    let method_ctx = LowerCtx {
        dut_field: "dut".to_string(),
        tb_field: None,
        cov_fields: HashMap::new(),
        covgroups: Vec::new(),
        clock_names: Vec::new(),
        allow_scheduler_time_waits: true,
        record_ids: record_ctx.record_ids.clone(),
        records: record_ctx.records.clone(),
        bus_bindings,
        // Initiator-BFM method bodies carry the placeholder bus prefix;
        // remaps are applied at bind time by `fill_initiator_bus_prefix`.
        bus_remaps: HashMap::new(),
        transactor_fields: HashMap::new(),
        passive_transactor_fields: std::collections::HashSet::new(),
        transactors: Vec::new(),
        scoreboard_fields: HashMap::new(),
        scoreboards: Vec::new(),
        consts: record_ctx.consts.clone(),
        properties: record_ctx.properties.clone(),
        owner: None,
        const_signed: record_ctx.const_signed.clone(),
        tb_scalar_fields: HashSet::new(),
        tb_queue_fields: HashMap::new(),
        tb_record_fields: Vec::new(),
        regblock_callbacks: HashMap::new(),
        tb_methods: HashMap::new(),
        test_scope_lets: HashSet::new(),
        regblock_bindings: HashMap::new(),
        regblock_init_order: Vec::new(),
        addrmap_bindings: HashMap::new(),
        addrmap_init_order: Vec::new(),
        bare_transactor_fields: HashSet::new(),
        target_state: HashMap::new(),
        components: Vec::new(),
        component_fields: HashMap::new(),
        txn_keeps: HashMap::new(),
        randomize_problem_ids: HashMap::new(),
        tseqs: HashMap::new(),
        // Transactor-context lowering never resolves test-scope probes.
        probes: HashMap::new(),
        extern_fns: record_ctx.extern_fns.clone(),
    };

    let mut funcs = Vec::new();
    let mut sibling_methods = HashMap::new();
    for (h, active_only) in &methods_ast {
        let mname = h.name.name.clone();
        if sibling_methods
            .insert(
                mname.clone(),
                (
                    h.params
                        .iter()
                        .map(|p| p.name.name.clone())
                        .collect::<Vec<_>>(),
                    h.return_ty.is_some(),
                    *active_only,
                ),
            )
            .is_some()
        {
            return Err(LowerError::Invalid(format!(
                "transactor `{tname}` declares method `{mname}` more than once"
            )));
        }
    }
    for (h, active_only) in methods_ast {
        let mname = &h.name.name;
        check_scalar_ty(tname, mname, "return type", h.return_ty.as_ref())?;

        let fid = FunctionId(next_fn.0 + funcs.len() as u32);
        let mut b = FuncBuilder::new(&method_ctx, helper_registry, side_tables);
        b.in_transactor_method = true;
        b.self_transactor = Some(tname.clone());
        b.self_transactor_methods = sibling_methods.clone();
        b.self_transactor_method_active_only = active_only;
        b.current_body_name = Some(mname.clone());
        // Bare-name reads/writes of a state field route to
        // `TransactorState`/`TransactorStateWrite` with an empty instance
        // placeholder, filled at test-binding time (same as the unbound
        // and bound-to target forms). Method params shadow state names
        // (declared below, looked up first), so this is safe up front.
        b.target_state_fields = state_names.clone();
        let mut params = Vec::with_capacity(h.params.len());
        for p in &h.params {
            let ty = method_param_ir_type(tname, mname, p, &method_ctx.record_ids)?;
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
            // The transactor id is fixed up by the caller's push order;
            // a method body never reads its own kind's id. The bound-
            // target path uses the same placeholder convention.
            FunctionKind::TransactorBody {
                transactor: TransactorId(record_ctx.transactors.len() as u32),
            },
            None,
        )?;
        f.params = params;
        schema.methods.push(TransactorMethodSchema {
            name: mname.clone(),
            function: fid,
            param_names: f.params.iter().map(|p| p.name.clone()).collect(),
            has_ret: f.ret.is_some(),
            active_only,
            pre_hooks: Vec::new(),
            post_hooks: Vec::new(),
            cov_hook_subs: Vec::new(),
        });
        funcs.push(f);
    }

    Ok((schema, funcs))
}

/// Lower one persistent state field of a bound-to target transactor.
/// Two kinds are lowered, reusing the same machinery scoreboards and
/// composite components already carry:
///   * a scalar `≤64`-bit counter/latch (`read_count : uint<32> default
///     0`) with a plain-integer/bool default (or no default → 0);
///   * a typed FIFO `queue<scalar ≤ 64 bits>` / `queue<Record>`
///     (`pending : queue<uint<32>>` / `queue<Beat>`), whose element type
///     resolves through the shared `lower_queue_elem` seam.
/// Event/directional fields, module/transaction-typed fields, a `default`
/// on a queue field, and `guard`/`reset` clauses are out of subset.
///
/// `record_ids` resolves `queue<Record>` element names (empty for the
/// unbound DUT-poking form's ctx, which then only admits scalar queues).
fn lower_state_field(
    tname: &str,
    f: &ComponentField,
    record_ids: &std::collections::HashMap<String, crate::ir::RecordId>,
    record_ctx: &super::LowerCtx,
) -> Result<StateFieldSchema, LowerError> {
    let fname = &f.name.name;
    if f.direction.is_some() {
        return Err(unsupported(
            &format!("bound-to transactor `{tname}` event/directional field `{fname}`"),
            "",
        ));
    }
    // A `queue<T>` state field → the shared queue-element machinery
    // (scalar ≤ 64 bits or a value-record), reused verbatim from the
    // scoreboard/component queue seam so all three lower `queue<Record>`
    // through the identical `harc_rt::HarcQueue<Rec>` shape.
    if let TypeExpr::Builtin {
        name: crate::ast::BuiltinTy::Queue,
        args,
        ..
    } = &f.ty
    {
        if f.default.is_some() {
            return Err(unsupported(
                &format!(
                    "bound-to transactor `{tname}` queue state field `{fname}` with a default"
                ),
                "a `queue<T>` state field starts empty; drop the `default`",
            ));
        }
        let elem = super::components::lower_queue_elem(tname, fname, args.first(), record_ids)?;
        return Ok(StateFieldSchema {
            name: fname.clone(),
            kind: StateFieldKind::Queue { elem },
        });
    }
    // A whole value-record state field (`last : Beat`) → the shared
    // record machinery (`IrType::Record` / `RecordId`), reused verbatim
    // from the `queue<Record>` / scoreboard / component record seam so
    // the state struct carries a value-record member. Sub-fields are
    // accessed via the state-record ops; the whole record round-trips
    // through the scalar `TransactorState*` forms.
    if let TypeExpr::Named { name, generics, .. } = &f.ty {
        let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
        if let Some(&rid) = record_ids.get(simple) {
            if !generics.is_empty() {
                return Err(unsupported(
                    &format!(
                        "bound-to transactor `{tname}` record state field `{fname}` of a \
                         generic-applied type"
                    ),
                    "",
                ));
            }
            if f.default.is_some() {
                return Err(unsupported(
                    &format!(
                        "bound-to transactor `{tname}` record state field `{fname}` with a default"
                    ),
                    "a record state field is default-constructed; drop the `default`",
                ));
            }
            return Ok(StateFieldSchema {
                name: fname.clone(),
                kind: StateFieldKind::Record { record: rid },
            });
        }
    }
    let Some(ty) = super::tb_scalar_field_ir_type(&f.ty) else {
        return Err(unsupported(
            &format!("bound-to transactor `{tname}` state field `{fname}` with a non-scalar type"),
            "target-transactor state must be a scalar `uint<N>`/`sint<N>`/`bool` (≤64 bits), \
             a whole value-record, or a `queue<scalar ≤ 64 bits>` / `queue<Record>`",
        ));
    };
    // Same rule as the component/scoreboard field defaults, and the
    // same `check_const_decl_type` range check a `const` declaration
    // gets. v1 emits the default's SOURCE TEXT into the member
    // initializer, so a literal or a `const` name works there but
    // anything else silently degrades to `= 0` — a `default 1 + 1`
    // state field starts at 0, not 2.
    let default = match &f.default {
        None => 0,
        Some(d) => super::components::fold_field_default(
            d,
            Some(&f.ty),
            &record_ctx.const_vals(),
            &format!("transactor `{tname}` state field `{fname}`"),
        )?,
    };
    Ok(StateFieldSchema {
        name: fname.clone(),
        kind: StateFieldKind::Scalar { ty, default },
    })
}

/// Method returns and TLM bus-target args must be scalar (bool / uint /
/// sint) and at most 64 bits wide — the tbir value model is `uint64_t`.
/// v1 lowers wider widths through `_harc_u128` / `VlWide`; this slice
/// only mirrors that for *active-method value params* (see
/// [`check_method_param_ty`]), so every other site stays ≤64 bits. The
/// rejection names the offending site.
fn check_scalar_ty(
    tname: &str,
    mname: &str,
    what: &str,
    ty: Option<&TypeExpr>,
) -> Result<(), LowerError> {
    check_scalar_ty_max(tname, mname, what, ty, 64)
}

/// Active-method value param: the tbir wide-value ABI mirrors v1's value
/// model for any `uint<N>`/`sint<N>` param width — ≤64 bits is u64-backed,
/// 65..128 bits use `_harc_u128` (`__uint128_t`), and `>128` bits use the
/// shared `HarcWide<N>` word-array storage (`local_scalar_cty`). The method
/// body moves the value to a wide DUT port / compares it / hex-formats it,
/// all of which the runtime supports for every width, so no width ceiling
/// applies here (`u32::MAX` = effectively unbounded). Non-scalar param types
/// are still rejected precisely.
fn check_method_param_ty(
    tname: &str,
    mname: &str,
    what: &str,
    ty: Option<&TypeExpr>,
) -> Result<(), LowerError> {
    check_scalar_ty_max(tname, mname, what, ty, u32::MAX)
}

/// Shared scalar-type gate parameterized by the maximum allowed bit width
/// (`max_w`). A width arg above `max_w` is rejected; a non-scalar type is
/// rejected; widthless / classifiable scalars pass.
fn check_scalar_ty_max(
    tname: &str,
    mname: &str,
    what: &str,
    ty: Option<&TypeExpr>,
    max_w: u32,
) -> Result<(), LowerError> {
    let site = || format!("transactor method `{tname}.{mname}` {what}");
    match ty {
        None => Ok(()),
        Some(TypeExpr::Builtin { args, .. }) => {
            // Width arg, when present, must fit the value model for this site.
            if let Some(TypeArg::Expr(e)) = args.first() {
                if let crate::ast::ExprKind::Int(s) = &*e.kind {
                    if let Ok(w) = s.replace('_', "").parse::<u32>() {
                        if w > max_w {
                            let hint = if max_w == 64 {
                                "the tbir value model is 64-bit".to_string()
                            } else {
                                format!(
                                    "the tbir wide-value method ABI mirrors v1's \
                                     `_harc_u128` model up to {max_w} bits"
                                )
                            };
                            return Err(unsupported(
                                &format!("{} wider than {max_w} bits (uint<{w}>)", site()),
                                &hint,
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

/// `Some(rid)` when `t` names a lowered record (`struct`/`transaction`)
/// in the given context; `None` for a scalar/aggregate/unknown type.
fn record_id_of_type(ctx: &super::LowerCtx, t: &TypeExpr) -> Option<ir::RecordId> {
    let TypeExpr::Named { name, .. } = t else {
        return None;
    };
    let simple = name.segments.last().map(|s| s.name.as_str())?;
    ctx.record_ids.get(simple).copied()
}

/// Resolve a method parameter's `IrType`. A `Named` type that names a
/// declared `transaction`/`struct` lowers to `IrType::Record` (passed
/// by value — the method body binds the record param and reads its
/// fields, mirroring v1's by-value struct param). Everything else goes
/// through `check_method_param_ty` and lowers as a scalar (`uint<N>`/
/// `sint<N>`/`bool`); any width flows through the wide-value ABI (u64 /
/// `_harc_u128` / `HarcWide<N>` per `local_scalar_cty`), and a non-scalar
/// type is rejected precisely there. The `Vec`-of-record / nested-record
/// cases are not reachable: a record param is a flat value-record, exactly
/// as v1 emits.
fn method_param_ir_type(
    tname: &str,
    mname: &str,
    p: &Param,
    record_ids: &HashMap<String, ir::RecordId>,
) -> Result<IrType, LowerError> {
    if let Some(TypeExpr::Named { name, .. }) = p.ty.as_ref() {
        let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
        if let Some(&rid) = record_ids.get(simple) {
            return Ok(IrType::Record(rid));
        }
    }
    check_method_param_ty(
        tname,
        mname,
        &format!("parameter `{}`", p.name.name),
        p.ty.as_ref(),
    )?;
    Ok(helpers::ir_type_of(p.ty.as_ref()))
}
