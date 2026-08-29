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
//! Event-bearing unbound transactors and `on` handlers route through the
//! component lowering path before this module. Everything else outside the
//! subset — `bound to <BusType>` (initiator side), generics, and TLM target
//! threads — is rejected explicitly. A `watchdog` is the exception: v1 emits its
//! body and never schedules it, so that one is a `NotImplemented`.

use super::{
    helpers, not_implemented, unsupported, FuncBuilder, LowerCtx, LowerError, SideTables, V1Status,
};
use crate::ast::{
    BusDecl, ComponentField, ComponentItem, HookableMethod, Param, TargetTlmThread, TransactorDecl,
    TypeArg, TypeExpr,
};
use crate::ir::{
    self, Activation, FunctionId, FunctionKind, IrType, StateFieldKind, StateFieldSchema,
    TargetTlmMethodSchema, TbFunction, Terminator, TransactorId, TransactorMethodSchema,
    TransactorSchema, TypedParam,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

/// Lower one `transactor` declaration into a schema plus one
/// `TbFunction` per method. `next_fn` is the id the FIRST method
/// function will get (the caller pushes the returned functions in
/// order).
/// One item of an UNBOUND transactor, from either declaration position.
///
/// The two positions used to be two copies of this match, and five of
/// the rejections were the same rejection written twice. They are also
/// the arms that had never been measured — every one said "re-run with
/// `--codegen v1`", and only two of the six shapes they cover are
/// things v1 actually gets right:
///
/// | item | v1 emits | verdict |
/// |---|---|---|
/// | `req : in event<uint<8>>` | routed to component lowering, which emits `std::vector<std::function<void(uint64_t)>> req;` plus the real fan-out at the emit site | supported |
/// | `p : in uint<8>` / `out uint<8>` | `uint64_t p;` — the direction is dropped; uninitialized unless the field also carries a `default` | `SilentlyMisLowers` |
/// | `dut : Top default <lit>` | `VTop* dut = <lit>;` — only `0` converts; `default 1` is "invalid conversion from 'int' to 'VTop*'" | `EmitsUncompilable` |
/// | a second module-typed field | a `V<Name>*` member each, but only the testbench DUT's header — and this function cannot see which module that is | `EmitsUncompilable` |
/// | `apply Some.Policy` | nothing — the output is byte-identical to the same transactor without it | `SilentlyMisLowers` |
/// | `on <anything>` | — | unreachable |
///
/// The `on` arm is dead in both positions: `transactor_is_component`
/// routes an unbound transactor carrying ANY `OnHandler` — subscription,
/// cycle-trigger, or periodic — to the component path before this
/// function is called. So is the `Lifecycle` half of the lifecycle/apply
/// arm, which the PARSER refuses inside a transactor ("lifecycle blocks
/// are currently supported only inside `test`/`impl` and `testbench`");
/// only `apply` reaches it. Both were confirmed with `unreachable!()`
/// against the whole suite.
///
/// The `default` row is the branch's "worst thing v1 does anywhere under
/// the arm" rule paying out: `default 0` compiles, because `0` is a null
/// pointer constant, and every other literal does not.
///
/// The second-handle row took three passes. The first called it a null
/// dereference by comparing against a control that was equally broken —
/// neither backend auto-binds ANY handle, so `VTop* dut = nullptr;` is
/// what the supported single-handle shape emits too, and
/// `<inst>.dut = dut` is the idiom in both. The second called it a real
/// escape hatch on the strength of the one row that compiles. The third
/// is above: the row that compiles does so only when the field names
/// the TESTBENCH's DUT module, which this function cannot see.
#[allow(clippy::too_many_arguments)]
fn lower_unbound_item<'a>(
    ci: &'a ComponentItem,
    from_when_active: bool,
    tname: &str,
    record_ctx: &LowerCtx,
    dut: &mut Option<(String, String)>,
    methods_ast: &mut Vec<(&'a HookableMethod, bool)>,
    state_fields: &mut Vec<StateFieldSchema>,
    state_names: &mut HashMap<String, StateFieldKind>,
) -> Result<(), LowerError> {
    let mut push_state = |f: &crate::ast::ComponentField| -> Result<(), LowerError> {
        let sf = lower_state_field(
            tname,
            f,
            &record_ctx.record_ids,
            record_ctx,
            StateFieldOwner::Unbound,
        )?;
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
        Ok(())
    };
    match ci {
        ComponentItem::Hookable(h) => methods_ast.push((h, from_when_active)),
        ComponentItem::Field(f) => {
            let fname = &f.name.name;
            // The directional rule lives in `lower_state_field`. This
            // was the THIRD pre-check shadowing it — the previous commit
            // deleted the two on the initiator paths, said "all three
            // owners" and left this one, so the unbound form kept the
            // pre-split blanket `Unsupported` for `event<Color>` and
            // `event<T> default 0`, both of which v1 fails to compile.
            // A DIRECTIONAL field goes to `lower_state_field`, which
            // owns that rule, before any of the named-type branches
            // below get a look. Deleting the old pre-check without this
            // was a regression: `dut : in TlmReadInitiator` fell into
            // the module-handle branch and LOWERED, tbir dropping the
            // `in` marker itself — byte-identical output to the
            // undirected spelling — where it had been refused.
            if f.direction.is_some() {
                return push_state(f);
            }
            if let TypeExpr::Named { name, .. } = &f.ty {
                let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                // KNOWN GAP, measured and deliberately not fixed here:
                // `record_ids` also holds regblock MIRRORS, so
                // `b : DmaRegs` is accepted as a value-record — tbir
                // emits `DmaRegs b{};` and compiles, while v1 emits
                // `VDmaRegs* b = nullptr;` and g++ refuses it (1 error).
                // `queue<DmaRegs>` is the same hole: tbir gives
                // `HarcQueue<DmaRegs>`, v1 `HarcQueue<uint64_t>`, both
                // building. Gating it needs a regblock-name set that
                // does not reach this function; the event allow-list
                // above sidesteps the same map rather than trusting it.
                //
                // A whole value-record held as transactor state
                // (`cur : Beat`), legal in BOTH declaration positions —
                // v1 compiles one written inside `when active` exactly
                // as it does one written above. Same schema the bound-to
                // path produces, and the same duplicate check the scalar
                // branch makes: without it `cur : uint<32>` plus
                // `cur : Beat` emitted two `cur` members.
                if record_ctx.record_ids.contains_key(simple) {
                    return push_state(f);
                }
                if f.default.is_some() {
                    return Err(not_implemented(
                        &format!("a default value on the DUT-handle field `{tname}.{fname}`"),
                        "the DUT handle is bound by the test; v1 pastes the literal into \
                         `VTop* <f> = <lit>;`, which only compiles when it is `0`",
                        V1Status::EmitsUncompilable,
                    ));
                }
                if let Some((first, _)) = dut.as_ref() {
                    // v1 emits `V<Name>* <field> = nullptr;` for every
                    // module-typed field and includes exactly ONE
                    // Verilated header — the TESTBENCH's DUT type. So
                    // whether its output compiles turns on whether each
                    // field names that module, which this function
                    // cannot see: transactors lower before testbenches,
                    // and a transactor with two module fields never
                    // reaches the testbench-side check that does know it
                    // (`lower_test`'s DUT-type comparison), because it
                    // errors out here first.
                    //
                    //   dut : Top, other : Top   both bound, 0 errors
                    //     — when the testbench DUT is also `Top`
                    //   d1 : Foo, d2 : Foo       'VFoo' does not name a
                    //     type, twice — and this arm cannot tell the two
                    //     apart
                    //   other : AxiLiteRegs      'VAxiLiteRegs' likewise
                    //   mode  : Color (an enum)  `Color mode;` — no `V`
                    //     prefix and no pointer; v1 never emits a C++
                    //     enum at all, so the name is simply undeclared
                    //
                    // An arm's status is the worst thing v1 does
                    // anywhere under it. A first pass called this a real
                    // escape hatch on the `other : Top` row alone; a
                    // second tried to split on `simple != dut_ty`, which
                    // compares the second field against the FIRST
                    // HANDLE rather than against the testbench's DUT —
                    // right answer for `other : AxiLiteRegs`, wrong one
                    // for `d1 : Foo, d2 : Foo`.
                    return Err(not_implemented(
                        &format!(
                            "transactor `{tname}` with more than one module-typed field \
                             (`{first}`, `{fname}`)"
                        ),
                        "the TB-IR transactor schema carries exactly one DUT handle; v1 \
                         emits a `V<Name>*` member for each while including only the \
                         testbench DUT's Verilated header, so unless every one of them \
                         names that same module the emitted C++ does not compile",
                        V1Status::EmitsUncompilable,
                    ));
                }
                *dut = Some((fname.clone(), simple.to_string()));
            } else {
                return push_state(f);
            }
        }
        ComponentItem::OnHandler(_) => unreachable!(
            "an unbound transactor carrying any `on` handler is routed to the component \
             path by `transactor_is_component`"
        ),
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
        ComponentItem::Lifecycle(..) => unreachable!(
            "the parser refuses a lifecycle block inside a transactor: \
             \"lifecycle blocks are currently supported only inside `test`/`impl` and \
             `testbench`\""
        ),
        ComponentItem::Apply(_) => {
            return Err(not_implemented(
                &format!("an `apply` item on transactor `{tname}`"),
                "v1 emits NOTHING for it — its output is byte-identical to the same \
                 transactor without the item, so the policy silently does not apply",
                V1Status::SilentlyMisLowers,
            ));
        }
    }
    Ok(())
}

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
    // ONE walk over both declaration positions. These were two
    // copies of the same 120-line match, differing in exactly one
    // expression — `methods_ast.push((h, false))` vs `(h, true)` — and
    // five of the rejection arms in them were the same rejection
    // written twice. The flattening itself is v1's
    // `synth_component_from_transactor` with include_active = true; the
    // flag is preserved only so an always-on sibling cannot backdoor-
    // call an active-only method.
    let items = t
        .items
        .iter()
        .map(|ci| (ci, false))
        .chain(t.when_active.iter().flatten().map(|ci| (ci, true)));
    for (ci, from_when_active) in items {
        lower_unbound_item(
            ci,
            from_when_active,
            tname,
            record_ctx,
            &mut dut,
            &mut methods_ast,
            &mut state_fields,
            &mut state_names,
        )?;
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
        target_transactor_fields: HashMap::new(),
        passive_transactor_fields: std::collections::HashSet::new(),
        transactors: Vec::new(),
        heartbeat_transactor_fields: Default::default(),
        heartbeat_transactor_storage: HashMap::new(),
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
        ambiguous_variants: record_ctx.ambiguous_variants.clone(),
        enum_names: HashSet::new(),
        tb_scalar_fields: HashMap::new(),
        tb_queue_fields: HashMap::new(),
        tb_record_fields: Vec::new(),
        regblock_callbacks: HashMap::new(),
        tb_methods: HashMap::new(),
        test_scope_lets: HashSet::new(),
        regblock_instance_types: record_ctx.regblock_instance_types.clone(),
        regblock_bindings: HashMap::new(),
        regblock_init_order: Vec::new(),
        addrmap_bindings: HashMap::new(),
        addrmap_init_order: Vec::new(),
        bare_transactor_fields: HashSet::new(),
        target_state: HashMap::new(),
        components: Vec::new(),
        component_fields: HashMap::new(),
        component_modes: HashMap::new(),
        // A method body could host `randomize`. The constraint-IR problem
        // table only catalogs test/tseq sites, so it has no problem-id,
        // but declared record keeps still belong to the fallback site.
        record_keeps: record_ctx.record_keeps.clone(),
        randomize_problem_ids: HashMap::new(),
        tseqs: HashMap::new(),
        // Transactor-context lowering never resolves test-scope probes.
        probes: HashMap::new(),
        extern_fns: record_ctx.extern_fns.clone(),
        // Transactor bodies never host a testbench-lifecycle marker call
        // (#619 M4a); the map stays empty here.
        tb_lifecycle_fns: std::collections::HashMap::new(),
    };

    let mut funcs = Vec::new();
    let mut sibling_methods = HashMap::new();
    for (h, active_only) in &methods_ast {
        let mname = h.name.name.clone();
        let param_tys = h
            .params
            .iter()
            .map(|p| method_param_ir_type(tname, &mname, p, &method_ctx.record_ids))
            .collect::<Result<Vec<_>, _>>()?;
        let ret_ty = method_return_ir_type(
            tname,
            &mname,
            "return type",
            h.return_ty.as_ref(),
            &method_ctx.record_ids,
        )?;
        if sibling_methods
            .insert(
                mname.clone(),
                (
                    h.params
                        .iter()
                        .map(|p| p.name.name.clone())
                        .collect::<Vec<_>>(),
                    param_tys,
                    ret_ty,
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
        let ret_ty = method_return_ir_type(
            tname,
            mname,
            "return type",
            h.return_ty.as_ref(),
            &method_ctx.record_ids,
        )?;

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
        if let Some(ty) = ret_ty.clone() {
            let ret = b.declare("__ret");
            b.set_local_type(ret, ty);
            b.helper_ret = Some(ret);
        }
        b.lower_block_stmts(&h.body)?;
        // Leave natural completion unterminated so `finish` records the
        // synthesized return in `implicit_returns`. Post hooks fire there,
        // but must bypass an explicit source `return`.
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
            param_tys: f.params.iter().map(|p| p.ty.clone()).collect(),
            ret_ty,
            has_ret: f.ret.is_some(),
            hookable: h.is_hookable,
            active_only,
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
    // The SAME question `transactor_is_component` answers for this
    // transactor, asked through the same function rather than re-derived
    // here. When it says yes, the component view owns every field this
    // pass is about to skip.
    let component_hosted = super::components::bound_transactor_is_component(t);
    // Resolve the bound bus.
    let bus_name = match t.bound_to.as_ref() {
        Some(bt) => super::bound_bus_name(bt, &format!("transactor `{tname}`"))?,
        // Both bound paths are reached only from `lower_transactor`'s
        // `if t.bound_to.is_some()` branch.
        None => unreachable!("a bound-transactor path entered with no `bound to` clause"),
    };
    let Some(bus) = buses.get(&bus_name) else {
        // The same shape as `bound to <builtin>` one type-variant over,
        // and it gets the same verdict for the same reason: a
        // NEVER-INSTANTIATED `transactor T bound to RegOp` emits an
        // inert `struct T { … };` under v1 and the file compiles, so
        // some configuration of the program runs and `Invalid` — which
        // this arm used to answer — is too strong.
        return Err(not_implemented(
            &format!(
                "transactor `{tname}` bound to `{bus_name}`, which is not a `bus` \
                 declaration"
            ),
            "v1 rejects it at every instantiation; only a never-instantiated \
             declaration gets through, and there it emits an inert struct",
            V1Status::Rejects,
        ));
    };

    // Walk items (and the optional `when active` body, though target
    // responders live as always-on items): collect state fields and
    // target threads; reject the out-of-subset shapes precisely.
    let mut state_fields: Vec<StateFieldSchema> = Vec::new();
    let mut state_names: HashMap<String, StateFieldKind> = HashMap::new();
    let mut state_activations: HashMap<String, Activation> = HashMap::new();
    let mut threads_ast: Vec<(&TargetTlmThread, Activation)> = Vec::new();
    let all_items = t.items.iter().map(|item| (item, Activation::Always)).chain(
        t.when_active
            .iter()
            .flatten()
            .map(|item| (item, Activation::ActiveOnly)),
    );
    for (ci, activation) in all_items {
        match ci {
            ComponentItem::Field(f) => {
                // A mixed responder/handler declaration has two IR views.
                // Component IR owns event ports, DUT handles, fixed vectors,
                // and nested components; responder IR needs only the state
                // kinds its target-thread expression lowering can address.
                // The component pass still validates every skipped field.
                let target_state_field = f.direction.is_none()
                    && (matches!(
                        f.ty,
                        TypeExpr::Builtin {
                            name: crate::ast::BuiltinTy::Queue,
                            ..
                        }
                    ) || super::tb_scalar_field_ir_type(&f.ty).is_some()
                        || matches!(
                            &f.ty,
                            TypeExpr::Named { name, .. }
                                if name
                                    .segments
                                    .last()
                                    .is_some_and(|name| record_ctx.record_ids.contains_key(&name.name))
                        ));
                if component_hosted && !target_state_field {
                    continue;
                }
                let sf = lower_state_field(
                    tname,
                    f,
                    &record_ctx.record_ids,
                    record_ctx,
                    StateFieldOwner::BoundTarget,
                )?;
                if state_names
                    .insert(sf.name.clone(), sf.kind.clone())
                    .is_some()
                {
                    return Err(LowerError::Invalid(format!(
                        "transactor `{tname}` declares state field `{}` more than once",
                        sf.name
                    )));
                }
                state_activations.insert(sf.name.clone(), activation);
                state_fields.push(sf);
            }
            ComponentItem::TargetTlmThread(th) => threads_ast.push((th, activation)),
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
            ComponentItem::OnHandler(h) if !h.periodic => {
                // The component IR view lowers and schedules non-periodic
                // handlers. This target view keeps only responder metadata so
                // its actor can share the component-hosted instance.
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
            // `apply <Aspect>` and a lifecycle block are two
            // different situations wearing one arm.
            //
            // A lifecycle block never reaches here at all: the parser
            // (`parser.rs`, "lifecycle blocks are currently supported
            // only inside `test`/`impl` and `testbench`") rejects
            // `setup`/`check`/`teardown` in every component that is not
            // a `testbench`, so neither backend ever sees one on a
            // transactor. Measured: both backends print that same
            // parser error, byte for byte.
            //
            // `apply` DOES reach here, and v1 does not implement it:
            // `ComponentItem::Apply(_) => {}` in `cpp_tb`, so v1 emits
            // the file with no trace of the aspect and without even
            // checking that the name resolves — `apply Whatever`,
            // naming nothing, emits clean. That is the `connect` arm's
            // situation one step above, and it takes the same verdict.
            // `unreachable!`, matching the sibling arm this file
            // already had for the same impossible state — not a
            // user-facing `Invalid`, which would be a diagnostic
            // nothing can ever print.
            ComponentItem::Lifecycle(..) => unreachable!(
                "the parser refuses a lifecycle block inside a transactor: \
                 \"lifecycle blocks are currently supported only inside `test`/`impl` and \
                 `testbench`\""
            ),
            ComponentItem::Apply(_) => {
                return Err(not_implemented(
                    &format!("bound-to transactor `{tname}` `apply` items"),
                    "v1 parses the item and emits NOTHING for it — the aspect is silently \
                     dropped, and its name is never resolved",
                    V1Status::SilentlyMisLowers,
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
        target_transactor_fields: HashMap::new(),
        passive_transactor_fields: std::collections::HashSet::new(),
        transactors: Vec::new(),
        heartbeat_transactor_fields: Default::default(),
        heartbeat_transactor_storage: HashMap::new(),
        scoreboard_fields: HashMap::new(),
        scoreboards: Vec::new(),
        consts: record_ctx.consts.clone(),
        properties: record_ctx.properties.clone(),
        owner: None,
        const_signed: record_ctx.const_signed.clone(),
        ambiguous_variants: record_ctx.ambiguous_variants.clone(),
        enum_names: HashSet::new(),
        tb_scalar_fields: HashMap::new(),
        tb_queue_fields: HashMap::new(),
        tb_record_fields: Vec::new(),
        regblock_callbacks: HashMap::new(),
        tb_methods: HashMap::new(),
        test_scope_lets: HashSet::new(),
        regblock_instance_types: record_ctx.regblock_instance_types.clone(),
        regblock_bindings: HashMap::new(),
        regblock_init_order: Vec::new(),
        addrmap_bindings: HashMap::new(),
        addrmap_init_order: Vec::new(),
        bare_transactor_fields: HashSet::new(),
        target_state: HashMap::new(),
        components: Vec::new(),
        component_fields: HashMap::new(),
        component_modes: HashMap::new(),
        // Responder bodies are not cataloged in the constraint-IR problem
        // table; a `randomize` here has no problem-id but still merges the
        // record's declared keeps.
        record_keeps: record_ctx.record_keeps.clone(),
        randomize_problem_ids: HashMap::new(),
        tseqs: HashMap::new(),
        // Transactor-context lowering never resolves test-scope probes.
        probes: HashMap::new(),
        extern_fns: record_ctx.extern_fns.clone(),
        // Transactor bodies never host a testbench-lifecycle marker call
        // (#619 M4a); the map stays empty here.
        tb_lifecycle_fns: std::collections::HashMap::new(),
    };

    let mut funcs = Vec::new();
    for (th, activation) in threads_ast {
        // `thread bus.<method>(...)`: the method path is `bus.<method>`.
        let segs: Vec<&str> = th.method.segments.iter().map(|s| s.name.as_str()).collect();
        if segs.len() != 2 || segs[0] != "bus" {
            // A program error, not a subset gap, so it does NOT point at
            // `--codegen v1`: v1 refuses it too ("target TLM thread ...
            // must target `bus.<method>`", measured), matching the sibling
            // `Invalid`s in this same loop (unknown method, arity, tags).
            return Err(LowerError::Invalid(format!(
                "transactor `{tname}` target thread `{}` must target `bus.<method>(...)`",
                segs.join(".")
            )));
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
        if let Some(record) = ret_record {
            body_ctx.reject_dynamic_list_record_wire(
                record,
                &format!(
                    "record return from target responder `bus.{mname}` crossing a TLM response wire"
                ),
            )?;
        }
        if let Some(ret) = method.ret.as_ref() {
            if ret_record.is_none() {
                check_scalar_ty(tname, mname, "return type", Some(ret))?;
            }
        }

        let fid = FunctionId(next_fn.0 + funcs.len() as u32);
        let mut b = FuncBuilder::new(&body_ctx, helper_registry, side_tables);
        b.concurrent_target_ooo_lanes = ooo_tags.is_some();
        b.current_body_name = Some(format!(
            "transactor `{tname}` {} target thread `bus.{mname}`",
            if matches!(activation, Activation::Always) {
                "always-present"
            } else {
                "active-only"
            }
        ));
        b.target_state_fields = state_names
            .iter()
            .filter(|(name, _)| {
                matches!(activation, Activation::ActiveOnly)
                    || matches!(state_activations[*name], Activation::Always)
            })
            .map(|(name, kind)| (name.clone(), kind.clone()))
            .collect();
        if matches!(activation, Activation::Always) {
            b.inactive_target_state_fields = state_activations
                .iter()
                .filter(|(_, activation)| matches!(activation, Activation::ActiveOnly))
                .map(|(name, _)| name.clone())
                .collect();
        }
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
            activation,
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
        Some(bt) => super::bound_bus_name(bt, &format!("transactor `{tname}`"))?,
        // Both bound paths are reached only from `lower_transactor`'s
        // `if t.bound_to.is_some()` branch.
        None => unreachable!("a bound-transactor path entered with no `bound to` clause"),
    };
    let Some(bus) = buses.get(&bus_name) else {
        // The same shape as `bound to <builtin>` one type-variant over,
        // and it gets the same verdict for the same reason: a
        // NEVER-INSTANTIATED `transactor T bound to RegOp` emits an
        // inert `struct T { … };` under v1 and the file compiles, so
        // some configuration of the program runs and `Invalid` — which
        // this arm used to answer — is too strong.
        return Err(not_implemented(
            &format!(
                "transactor `{tname}` bound to `{bus_name}`, which is not a `bus` \
                 declaration"
            ),
            "v1 rejects it at every instantiation; only a never-instantiated \
             declaration gets through, and there it emits an inert struct",
            V1Status::Rejects,
        ));
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
                // carry the bound-bus handshake context.
                // The directional rule lives in `lower_state_field`,
                // which every state field already goes through. This
                // site used to answer first, which made the split there
                // dead code on this path — a directional SCALAR kept a
                // blanket `Unsupported` while v1 dropped the direction
                // and compiled. One rule, one place, three owners.
                // A scalar persistent state field (`uint<N>`/`sint<N>`/
                // `bool` ≤64 bits with a plain-literal default). Reuse the
                // bound-to target state-field lowering; reject module/
                // transaction-typed and non-scalar fields inside it.
                let sf = lower_state_field(
                    tname,
                    f,
                    &record_ctx.record_ids,
                    record_ctx,
                    StateFieldOwner::BoundInitiator,
                )?;
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
            // Same split as the target-side arm: the lifecycle
            // half is unreachable (the parser refuses a lifecycle block
            // outside `test`/`impl`/`testbench`), and v1 drops `apply`
            // silently.
            // `unreachable!`, matching the sibling arm this file
            // already had for the same impossible state — not a
            // user-facing `Invalid`, which would be a diagnostic
            // nothing can ever print.
            ComponentItem::Lifecycle(..) => unreachable!(
                "the parser refuses a lifecycle block inside a transactor: \
                 \"lifecycle blocks are currently supported only inside `test`/`impl` and \
                 `testbench`\""
            ),
            ComponentItem::Apply(_) => {
                return Err(not_implemented(
                    &format!("initiator-side bound-to transactor `{tname}` `apply` items"),
                    "v1 parses the item and emits NOTHING for it — the aspect is silently \
                     dropped, and its name is never resolved",
                    V1Status::SilentlyMisLowers,
                ));
            }
        }
    }
    for ci in t.when_active.iter().flatten() {
        match ci {
            ComponentItem::Hookable(h) => methods_ast.push((h, true)),
            ComponentItem::Field(f) => {
                // The directional rule lives in `lower_state_field`,
                // which every state field already goes through. This
                // site used to answer first, which made the split there
                // dead code on this path — a directional SCALAR kept a
                // blanket `Unsupported` while v1 dropped the direction
                // and compiled. One rule, one place, three owners.
                let sf = lower_state_field(
                    tname,
                    f,
                    &record_ctx.record_ids,
                    record_ctx,
                    StateFieldOwner::BoundInitiator,
                )?;
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
            // Same split as the target-side arm: the lifecycle
            // half is unreachable (the parser refuses a lifecycle block
            // outside `test`/`impl`/`testbench`), and v1 drops `apply`
            // silently.
            // `unreachable!`, matching the sibling arm this file
            // already had for the same impossible state — not a
            // user-facing `Invalid`, which would be a diagnostic
            // nothing can ever print.
            ComponentItem::Lifecycle(..) => unreachable!(
                "the parser refuses a lifecycle block inside a transactor: \
                 \"lifecycle blocks are currently supported only inside `test`/`impl` and \
                 `testbench`\""
            ),
            ComponentItem::Apply(_) => {
                return Err(not_implemented(
                    &format!("initiator-side bound-to transactor `{tname}` `apply` items"),
                    "v1 parses the item and emits NOTHING for it — the aspect is silently \
                     dropped, and its name is never resolved",
                    V1Status::SilentlyMisLowers,
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
        target_transactor_fields: HashMap::new(),
        passive_transactor_fields: std::collections::HashSet::new(),
        transactors: Vec::new(),
        heartbeat_transactor_fields: Default::default(),
        heartbeat_transactor_storage: HashMap::new(),
        scoreboard_fields: HashMap::new(),
        scoreboards: Vec::new(),
        consts: record_ctx.consts.clone(),
        properties: record_ctx.properties.clone(),
        owner: None,
        const_signed: record_ctx.const_signed.clone(),
        ambiguous_variants: record_ctx.ambiguous_variants.clone(),
        enum_names: HashSet::new(),
        tb_scalar_fields: HashMap::new(),
        tb_queue_fields: HashMap::new(),
        tb_record_fields: Vec::new(),
        regblock_callbacks: HashMap::new(),
        tb_methods: HashMap::new(),
        test_scope_lets: HashSet::new(),
        regblock_instance_types: record_ctx.regblock_instance_types.clone(),
        regblock_bindings: HashMap::new(),
        regblock_init_order: Vec::new(),
        addrmap_bindings: HashMap::new(),
        addrmap_init_order: Vec::new(),
        bare_transactor_fields: HashSet::new(),
        target_state: HashMap::new(),
        components: Vec::new(),
        component_fields: HashMap::new(),
        component_modes: HashMap::new(),
        record_keeps: record_ctx.record_keeps.clone(),
        randomize_problem_ids: HashMap::new(),
        tseqs: HashMap::new(),
        // Transactor-context lowering never resolves test-scope probes.
        probes: HashMap::new(),
        extern_fns: record_ctx.extern_fns.clone(),
        // Transactor bodies never host a testbench-lifecycle marker call
        // (#619 M4a); the map stays empty here.
        tb_lifecycle_fns: std::collections::HashMap::new(),
    };

    let mut funcs = Vec::new();
    let mut sibling_methods = HashMap::new();
    for (h, active_only) in &methods_ast {
        let mname = h.name.name.clone();
        let param_tys = h
            .params
            .iter()
            .map(|p| method_param_ir_type(tname, &mname, p, &method_ctx.record_ids))
            .collect::<Result<Vec<_>, _>>()?;
        let ret_ty = method_return_ir_type(
            tname,
            &mname,
            "return type",
            h.return_ty.as_ref(),
            &method_ctx.record_ids,
        )?;
        if sibling_methods
            .insert(
                mname.clone(),
                (
                    h.params
                        .iter()
                        .map(|p| p.name.name.clone())
                        .collect::<Vec<_>>(),
                    param_tys,
                    ret_ty,
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
        let ret_ty = method_return_ir_type(
            tname,
            mname,
            "return type",
            h.return_ty.as_ref(),
            &method_ctx.record_ids,
        )?;

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
        if let Some(ty) = ret_ty.clone() {
            let ret = b.declare("__ret");
            b.set_local_type(ret, ty);
            b.helper_ret = Some(ret);
        }
        b.lower_block_stmts(&h.body)?;
        // Preserve natural-vs-explicit return provenance for post-hook
        // fan-out (same contract as unbound/component hookable methods).
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
            param_tys: f.params.iter().map(|p| p.ty.clone()).collect(),
            ret_ty,
            has_ret: f.ret.is_some(),
            hookable: h.is_hookable,
            active_only,
            cov_hook_subs: Vec::new(),
        });
        funcs.push(f);
    }

    Ok((schema, funcs))
}

/// How the transactor whose state field this is was declared. Every
/// message in `lower_state_field` used to say "bound-to transactor", on all
/// FOUR call sites — including the unbound DUT-poking form, which is
/// bound to nothing, and the two initiator-side ones, whose sibling
/// arms in the same functions say "initiator-side bound-to transactor".
/// A diagnostic that names the wrong construct sends the reader to the
/// wrong part of their file.
#[derive(Clone, Copy)]
pub(crate) enum StateFieldOwner {
    /// `transactor X` with no `bound to` — drives the DUT directly.
    Unbound,
    /// `transactor X bound to <Bus>` serving `thread bus.<m>(...)`.
    BoundTarget,
    /// `transactor X bound to <Bus>` with `hookable` methods.
    BoundInitiator,
}

impl StateFieldOwner {
    fn label(self) -> &'static str {
        match self {
            StateFieldOwner::Unbound => "transactor",
            StateFieldOwner::BoundTarget => "bound-to transactor",
            StateFieldOwner::BoundInitiator => "initiator-side bound-to transactor",
        }
    }
}

/// Lower one persistent state field of a transactor, in any of the
/// three forms `StateFieldOwner` names. The kinds reuse
/// the machinery scoreboards and composite components already carry:
///   * a scalar `<=64`-bit counter/latch (`read_count : uint<32>
///     default 0`) with a plain-integer/bool default (or none -> 0);
///   * a typed FIFO `queue<scalar>` / `queue<Record>`, whose
///     element type resolves through the shared `lower_queue_elem` seam.
///   * a fixed vector, whose complete recursive `IrType` resolves through
///     the component fixed-vector decoder.
///
/// `record_ids` resolves `queue<Record>` element names. It is NOT a
/// "does v1 declare this type" oracle — it also holds regblock MIRRORS,
/// which v1 declares under a different name and flattens in an event
/// signature, which is why the event allow-list does not consult it. It
/// is the
/// same non-empty map at all four call sites — the previous version of
/// this comment claimed it was empty for the unbound form, which it
/// never was.
///
/// A transaction-typed field is NOT out of subset (it lowers through
/// `record_ids` like any other record), and there is no `guard`/`reset`
/// clause to reject — `ComponentField` carries no such thing. Both were
/// claimed here too.
fn lower_state_field(
    tname: &str,
    f: &ComponentField,
    record_ids: &std::collections::HashMap<String, crate::ir::RecordId>,
    record_ctx: &super::LowerCtx,
    owner: StateFieldOwner,
) -> Result<StateFieldSchema, LowerError> {
    let fname = &f.name.name;
    let who = owner.label();
    if f.direction.is_some() {
        // `f.direction.is_some()` admits an EVENT field and everything
        // else, and they get different verdicts. The event half is
        // itself mixed — see the allow-list below.
        //
        //   * an EVENT field — v1 emits the subscriber vector
        //     (`std::vector<std::function<void(uint64_t)>>`) and it
        //     works, so `--codegen v1` is a real way out;
        //   * a directional SCALAR (`p : in uint<8>`) — v1 emits a
        //     plain `uint64_t p;` and DROPS the direction, compiling a
        //     program that means something else. Measured: g++ exit 0.
        //
        // Labelling the whole arm from the event probe alone pointed
        // the scalar case at a backend that silently changes what the
        // field means.
        if super::components::is_event_field(f) {
            // An ALLOW-LIST, after a blacklist grew three times.
            //
            // `Unsupported` promises v1 handles the program, so it may
            // only be given to a payload shape whose v1 behaviour is
            // certified HERE. What can be certified at this site is a
            // single positional BUILTIN scalar payload with no
            // `default`: v1 emits `std::vector<std::function<void(
            // uint64_t)>> ev;` and it compiles and works.
            //
            // Everything else v1 does one of two things to, measured:
            //   `event<Color>`            5 g++ errors — v1 emits no
            //                             C++ enum, so the payload name
            //                             in the signature is undeclared
            //   `event<string>`,
            //   `event<BusName>`,
            //   `event<TransactorName>`,
            //   `event<queue<T>>`,
            //   `event<stream<T>>`,
            //   `event<pkg.Beat>`,
            //   `event<T, U>`,
            //   `event<depth=16>`,
            //   `event<RegblockMirror>`   0 errors — the payload is
            //                             silently FLATTENED to
            //                             `void(uint64_t)`
            //
            // The flattening rows are `SilentlyMisLowers`, which
            // outranks the enum row's `EmitsUncompilable`, so one label
            // covers the lot.
            //
            // A record payload (`event<Beat>`) does work under v1, and
            // this refuses it too — because `record_ids` cannot tell a
            // struct from a REGBLOCK MIRROR, and the mirror is one of
            // the flattening rows. Certifying it needs a regblock set
            // that does not reach this function. Over-cautious beats
            // actively false: the alternative is an `Unsupported` that
            // promises v1 works for a shape where it silently does not.
            let payload_certified = match &f.ty {
                TypeExpr::Builtin { args, .. } => match args.as_slice() {
                    // A bare `event` with no payload. Certified, and the
                    // rule is not new: `lower_event_payload` already
                    // says "a bare `event` with no payload defaults to
                    // an unsigned scalar". v1 agrees — it emits the same
                    // `void(uint64_t)` member the certified
                    // `event<uint<8>>` produces, byte for byte. Answering
                    // `false` here gave two spellings of one member
                    // opposite verdicts.
                    [] => true,
                    [crate::ast::TypeArg::Type(TypeExpr::Builtin { name, .. })] => matches!(
                        name,
                        crate::ast::BuiltinTy::UInt
                            | crate::ast::BuiltinTy::UIntCap
                            | crate::ast::BuiltinTy::SInt
                            | crate::ast::BuiltinTy::SIntCap
                            | crate::ast::BuiltinTy::Bits
                            | crate::ast::BuiltinTy::Bool
                            | crate::ast::BuiltinTy::BoolLower
                            | crate::ast::BuiltinTy::Bit
                    ),
                    _ => false,
                },
                _ => false,
            };
            // Payload FIRST. A field that is both defaulted and
            // uncertified is under both arms, and this one is graded a
            // notch lower — checking the default first handed
            // `event<string> default ev` the weaker
            // `EmitsUncompilable` while v1 compiled it and flattened
            // the payload.
            if !payload_certified {
                return Err(not_implemented(
                    &format!("{who} `{tname}` event field `{fname}` with an uncertified payload"),
                    "TB-IR lowers an event payload that is a single builtin scalar, or none \
                     at all; for anything else v1 either flattens the payload to a 64-bit \
                     integer without a word, or names a type it never declares"
                        .to_string(),
                    V1Status::SilentlyMisLowers,
                ));
            }
            if f.default.is_some() {
                return Err(not_implemented(
                    &format!("{who} `{tname}` event field `{fname}` with a default"),
                    // NOT "which does not convert": `format_simple_expr`
                    // pastes a bare `Ident` verbatim, so `default ev2`
                    // naming another event field emits
                    // `... ev = ev2;` and compiles. `EmitsUncompilable`
                    // is the worst under the arm, which is what sets it.
                    "an event field is a subscriber list, not a value; v1 pastes the \
                     default into its initialiser, which does not convert for a literal"
                        .to_string(),
                    V1Status::EmitsUncompilable,
                ));
            }
            return Err(unsupported(
                &format!("{who} `{tname}` directional event field `{fname}`"),
                "event-driven transactors await the event slice; an `in event<T>` needs an \
                 `on <ev>` handler and an `out event<T>` needs an `emit` site and a \
                 subscriber, and neither is lowered yet",
            ));
        }
        // NOT "scalar": this is everything directional that is not an
        // event — `in Vec<uint<8>, 4>`, `in Beat`, `in Color`, and a
        // module handle (`dut : in Top`), which is routed here
        // deliberately by the dispatch at the top of
        // `lower_unbound_item`.
        //
        // What most of them share is that v1 emits the member for the
        // underlying type and DROPS the direction. The module handle is
        // the exception and the reason text below does NOT describe it:
        // v1's output for `dut : in Top` is byte-identical to
        // `dut : Top` and correct, since the handle is bound by the
        // test. It sits under this arm because `SilentlyMisLowers` is
        // the arm's WORST landing (`p : in uint<8>` genuinely does mean
        // something else), not because v1 mis-lowers this one.
        return Err(not_implemented(
            &format!("{who} `{tname}` directional non-event field `{fname}`"),
            "event-driven transactors await the event slice; v1 emits the member for the \
             field's own type and DROPS the direction, so the field means something other \
             than what was written (and reads indeterminate unless it also carries a \
             `default`)"
                .to_string(),
            V1Status::SilentlyMisLowers,
        ));
    }
    // A `queue<T>` state field → the shared queue-element machinery
    // (an exact scalar type or a value-record), reused verbatim from the
    // direct-testbench/scoreboard/component queue seam. This call opts
    // persistent transactor state into that already-shipped scalar policy;
    // non-queue state and event/field policies remain unchanged.
    if let TypeExpr::Builtin {
        name: crate::ast::BuiltinTy::Queue,
        args,
        ..
    } = &f.ty
    {
        if f.default.is_some() {
            // v1 pastes the default's SOURCE TEXT into the member
            // initialiser. Enumerated over what that text can be:
            //   `default 0`  -> `HarcQueue<uint64_t> q = 0;`
            //                   g++: could not convert `0` from `int`
            //   `default q0` -> `HarcQueue<uint64_t> q = q0;` (a bare
            //                   `Ident` is pasted verbatim) — compiles
            // So the arm is NOT uniformly uncompilable, and a comment
            // saying "there is no `--codegen v1` to send anyone to"
            // would be false. `EmitsUncompilable` is the WORST outcome
            // under it, which is what sets the label.
            return Err(not_implemented(
                &format!("{who} `{tname}` queue state field `{fname}` with a default"),
                "a `queue<T>` state field starts empty; drop the `default`".to_string(),
                V1Status::EmitsUncompilable,
            ));
        }
        let elem = super::components::lower_queue_elem(
            tname,
            fname,
            args.first(),
            record_ids,
            &record_ctx.enum_names,
        )?;
        return Ok(StateFieldSchema {
            name: fname.clone(),
            kind: StateFieldKind::Queue { elem },
        });
    }
    // A fixed-vector state field uses the same recursive resolver as
    // component fields and method parameters.  Keep the complete type so
    // state element accesses retain nested-vector/record leaf metadata.
    if let Some(ty @ IrType::FixedVec { .. }) =
        super::components::fixed_vec_ir_type_with_records(&f.ty, record_ids)
    {
        if f.default.is_some() {
            return Err(not_implemented(
                &format!("{who} `{tname}` fixed-vector state field `{fname}` with a default"),
                "a fixed-vector state field is value-initialised; drop the `default`"
                    .to_string(),
                V1Status::EmitsUncompilable,
            ));
        }
        return Ok(StateFieldSchema {
            name: fname.clone(),
            kind: StateFieldKind::FixedVec { ty },
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
                // NOT "a record cannot take generic parameters":
                // `parse_transaction` calls
                // `parse_optional_generic_params`, so
                // `transaction Beat#(W: int = 8)` parses and has its own
                // measured arm in `records.rs`. Only `struct` has no
                // slot. `record_ids` is transactions UNION structs UNION
                // regblock mirrors, so this arm spans all three:
                //   struct / transaction -> v1 emits `Beat b;`, dropping
                //     the argument list without a word; compiles, and
                //     runs the program `b : Beat` would have given
                //   regblock mirror      -> v1 emits
                //     `VDmaRegs* b = nullptr;` (its
                //     `is_dut_pointer_field_type` does not consult
                //     regblocks) and g++ refuses it
                // `SilentlyMisLowers` outranks `EmitsUncompilable`, so
                // that is the arm's label.
                return Err(not_implemented(
                    &format!(
                        "{who} `{tname}` record state field `{fname}` of a generic-applied type"
                    ),
                    "this type takes no generic arguments here (only a `transaction` \
                     declares them, and not at this site); v1 drops the argument list \
                     without a word and lowers the field as if it were not written"
                        .to_string(),
                    V1Status::SilentlyMisLowers,
                ));
            }
            if f.default.is_some() {
                // Same shape and same enumeration as the `queue`
                // default above: `default 0` gives `Beat b = 0;` and
                // g++ answers "could not convert `0` from `int`", while
                // `default b0` naming another record field pastes
                // verbatim and compiles. Worst wins.
                return Err(not_implemented(
                    &format!("{who} `{tname}` record state field `{fname}` with a default"),
                    "a record state field is default-constructed; drop the `default`".to_string(),
                    V1Status::EmitsUncompilable,
                ));
            }
            return Ok(StateFieldSchema {
                name: fname.clone(),
                kind: StateFieldKind::Record { record: rid },
            });
        }
    }
    let Some(ty) = super::tb_scalar_field_ir_type(&f.ty) else {
        // The catch-all of `tb_scalar_field_ir_type`, which answers
        // `None` for every non-`Builtin` type and every builtin that is
        // not `uint`/`sint`/`bits`/`bool`/`bit` within
        // `scalar_field_ir_type`. Enumerated rather than probed once,
        // because an arm's verdict is the WORST thing v1 does anywhere
        // under it:
        //
        //   `uint<128>`         -> lowered. It reached this arm while
        //                          the shared rule capped at 64, and
        //                          was the one landing here where v1
        //                          was a genuine way out; the cap is
        //                          the declared-field width now, so
        //                          this no longer answers `None`
        //   `uint<2048>`        -> `harc_rt::HarcWide<64> w;`, compiles
        //                          — past the declared-field width, and
        //                          v1 does handle the declaration
        //   `Vec<uint<8>, 4>`   -> handled by the fixed-vector arm above
        //   `stream<uint<8>>`   -> `uint64_t s;`                 , compiles
        //   `buffer<uint<8>,N>` -> `uint64_t bf;`                , compiles
        //   an enum type        -> `Color m;`  — g++: does not name a type
        //   an unknown named ty -> `VWidget* d = nullptr;`  — likewise
        //   `Vec<...> default 0`-> `std::array<...> v = 0;`  — no conversion
        //
        // The `stream`/`buffer` rows set the label. They fall through
        // `component_field_c_type`'s `_ =>` into `scalar_leaf_c_type`,
        // which answers `None`, and come out as a bare `uint64_t` — a
        // member that compiles and means something else entirely. That
        // is `SilentlyMisLowers`, two grades worse than the
        // `Unsupported` a `Vec`-only probe suggested.
        return Err(not_implemented(
            &format!("{who} `{tname}` state field `{fname}` with a non-scalar type"),
            "transactor state must be a scalar `uint<N>`/`sint<N>`/`bool` (up to 1024 \
             bits), a fixed vector, a whole value-record, or a `queue<scalar>` / \
             `queue<Record>`; v1 \
             emits a bare `uint64_t` member for a `stream`/`buffer` field, which compiles \
             and means something else (for a `uint<N>` past 1024 bits v1 declares the \
             member correctly, so `--codegen v1` does work for that one)"
                .to_string(),
            V1Status::SilentlyMisLowers,
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

/// TLM bus-target args and returns must be scalar (bool / uint / sint) and at
/// most 64 bits wide because their wire protocol is 64-bit. Active transactor
/// method params and returns use their separate exact-width gates below.
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

/// Active-method scalar return values use the same exact-width C++ ABI as
/// active-method parameters. The return slot, method schema, call destination,
/// and emitted `std::function` all carry this `IrType`, so widths above 64 bits
/// must not be rejected at the declaration gate.
fn check_method_return_ty(
    tname: &str,
    mname: &str,
    what: &str,
    ty: Option<&TypeExpr>,
) -> Result<(), LowerError> {
    check_scalar_ty_max(tname, mname, what, ty, u32::MAX)
}

/// Resolve a transactor method return through the aggregate-aware parameter
/// path. `TSeq<T>` and fixed vectors are by-value aggregate results, matching
/// the parameter ABI and the component/testbench method resolvers. All other
/// spellings retain the scalar gate.
fn method_return_ir_type(
    tname: &str,
    mname: &str,
    what: &str,
    ty: Option<&TypeExpr>,
    record_ids: &HashMap<String, ir::RecordId>,
) -> Result<Option<IrType>, LowerError> {
    let Some(ty) = ty else {
        return Ok(None);
    };
    if let Some(seq) = super::helpers::tseq_ir_type(Some(ty), record_ids) {
        return Ok(Some(seq));
    }
    if let Some(fixed @ IrType::FixedVec { .. }) =
        super::components::fixed_vec_ir_type_with_records(ty, record_ids)
    {
        return Ok(Some(fixed));
    }
    check_method_return_ty(tname, mname, what, Some(ty))?;
    Ok(Some(super::helpers::ir_type_of_with_records(
        Some(ty),
        record_ids,
    )))
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
/// declared `transaction`/`struct` lowers to `IrType::Record`; a fixed
/// `Vec<T, N>` lowers recursively to `IrType::FixedVec`, including record
/// leaves. Both are passed by value, matching v1. Everything else goes
/// through `check_method_param_ty` and lowers as a scalar (`uint<N>`/
/// `sint<N>`/`bool`); any width flows through the wide-value ABI.
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
    // A `TSeq<T>` parameter, through the resolver the component-method
    // schema uses. Without it the type came back `Unknown` and the slot
    // check described a `TSeq<Beat>` parameter as taking a non-record
    // value — then rejected `drv.dispatch(xs)`, which v1 compiles
    // (`[&](Drv& self, const std::vector<Beat>& txns)`).
    if let Some(seq) = helpers::tseq_ir_type(p.ty.as_ref(), record_ids) {
        return Ok(seq);
    }
    if let Some(fixed @ IrType::FixedVec { .. }) = p.ty.as_ref().and_then(|ty| {
        super::components::fixed_vec_ir_type_with_records(ty, record_ids)
    }) {
        return Ok(fixed);
    }
    check_method_param_ty(
        tname,
        mname,
        &format!("parameter `{}`", p.name.name),
        p.ty.as_ref(),
    )?;
    Ok(helpers::ir_type_of(p.ty.as_ref()))
}
