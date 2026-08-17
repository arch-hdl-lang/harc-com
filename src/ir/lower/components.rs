//! Composite-component lowering for the env/agent cluster's
//! flat-struct subset (docs/tbir-mvp.md §env/agent). Three source shapes
//! lower into one `ComponentSchema`, mirroring v1's uniform
//! `emit_component_struct` + `emit_component_method` treatment:
//!
//!   - a `scoreboard` carrying `hookable`/`function` **methods** (a
//!     data-only, method-less board stays on `ScoreboardSchema`);
//!   - a `transactor` used as a pure **analysis source** — `out event<T>`
//!     port(s) + methods that `emit` on them, and NO module-typed DUT
//!     field (the DUT-poking BFM form stays on `TransactorSchema`);
//!   - an `env` that composes the two as by-value sub-component fields
//!     and `connect`s a source event to a sink method.
//!
//! Classification (`is_component_*`) runs BEFORE the scoreboard /
//! transactor schema loops in `lower/mod.rs`, so each decl routes to
//! exactly one schema table.
//!
//! Out of subset, rejected (never mis-lowered): `agent` declarations and
//! `on <ev>` event handlers, watchdog/phase orchestration, generics,
//! `bound to` source transactors, and non-scalar event payloads. Those
//! gate on the agent/sequencer/event slices.

use super::{helpers, not_implemented, unsupported, FuncBuilder, LowerCtx, LowerError, V1Status};
use crate::ast::{
    BuiltinTy, ComponentDecl, ComponentField, ComponentItem, ConnectEdge, Direction, ExprKind,
    HookableMethod, TransactorDecl, TransactorMode, TypeArg, TypeExpr,
};
use crate::ir::{
    Activation, ComponentFieldKind, ComponentFieldSchema, ComponentId, ComponentInstanceMode,
    ComponentKindTag, ComponentMethodSchema, ComponentSchema, ConnectEdgeSchema, EventPayload,
    FunctionId, FunctionKind, IrType, RecordId, ScoreboardId, TbFunction, Terminator, TypedParam,
};
use std::collections::HashMap;

/// True when a `scoreboard` declaration carries a `hookable`/`function`
/// method, so it must lower as a component (per-instance state) rather
/// than the data-only `ScoreboardSchema`.
pub(crate) fn scoreboard_is_component(c: &ComponentDecl) -> bool {
    c.items
        .iter()
        .any(|it| matches!(it, ComponentItem::Hookable(_)))
}

/// True when a `transactor` declaration routes to the composite-component
/// table rather than the DUT-poking `TransactorSchema`. Two shapes:
///   * pure analysis source — at least one `event<T>` field and NO
///     module-typed DUT field (`out event<T>` ports + emit-only methods);
///   * event-driven (consumer-side) transactor — an `in event<T>` field
///     with a matching `on <ev>(t)` handler, optionally plus a single
///     module-typed DUT field the handler pokes. This is the unbound
///     consumer BFM: `emit drv.req(t)` (or a `connect` bridge) fires the
///     handler synchronously, which drives the DUT.
///   * event-driven (consumer-side) transactor **bound to a bus** — an
///     `in event<T>` field with a matching `on <ev>(t)` handler whose body
///     drives the bound bus's handshake channels (`bus.<ch>.send/recv`)
///     instead of a private DUT handle. This is the bound consumer BFM:
///     `emit drv.req(t)` fires the handler synchronously, which drives the
///     bound bus's wires on the test DUT.
///   * passive helper / monitor — hookable methods in the always-on body
///     plus a DUT handle, but no `when active` body, event pipe, or `on`
///     handler. This is the reusable passive monitor shape: methods read
///     DUT pins or update monitor state and exist on both active and passive
///     instances.
/// A `bound to` target responder (`thread bus.<m>(...)` bodies, no event
/// field) is excluded — it stays on the separate TLM responder path.
/// `env_held` is true when this transactor type is referenced as a
/// by-value sub-component field of some `env`/`agent` declaration in the
/// file. It only matters for the purely-structural DUT-poking BFM (the
/// trailing arm): such a BFM defaults to the dedicated `TransactorSchema`
/// path (its standalone testbench-field placement), and routes to the
/// composite-component table ONLY when an env must hold it by value — see
/// the trailing arm's comment.
pub(crate) fn transactor_is_component(t: &TransactorDecl, env_held: bool) -> bool {
    let mut has_event = false;
    let mut has_in_event = false;
    let mut has_on_handler = false;
    let mut has_periodic_handler = false;
    let mut has_module_field = false;
    let mut has_hookable = false;
    for it in t.items.iter().chain(t.when_active.iter().flatten()) {
        match it {
            ComponentItem::Field(f) => {
                if is_event_field(f) {
                    has_event = true;
                    if matches!(f.direction, Some(crate::ast::Direction::In)) {
                        has_in_event = true;
                    }
                } else if matches!(&f.ty, TypeExpr::Named { .. }) {
                    has_module_field = true;
                }
            }
            ComponentItem::OnHandler(h) if !h.periodic => has_on_handler = true,
            ComponentItem::OnHandler(_) => has_periodic_handler = true,
            ComponentItem::Hookable(_) => has_hookable = true,
            _ => {}
        }
    }
    // A `bound to` transactor routes here when it carries a non-periodic
    // `on` handler — either an event-driven driver (`in event` + a
    // subscribing `on ev(arg)`) or a PASSIVE bus monitor (`on
    // bus.<ch>.handshake(arg)` with no event/driver half). A `bound to`
    // transactor with NO `on` handler (a hookable-method initiator BFM or
    // a `thread bus.<m>` target responder — both structurally distinct
    // item kinds, `Hookable` / `TargetTlmThread`, never `OnHandler`) stays
    // on the dedicated transactor path.
    if t.bound_to.is_some() {
        return has_on_handler;
    }
    // Pure analysis source: events, no DUT.
    if has_event && !has_module_field {
        return true;
    }
    // Event-driven consumer transactor: an `in event` + a subscribing
    // `on` handler (the DUT field, if any, is the handler's poke target).
    if has_in_event && has_on_handler {
        return true;
    }
    // Reactive monitor / checker transactor with NO `in event`: a
    // cycle-trigger (`on dut.<sig>`, `on <expr> level`) or periodic
    // (`on <N> cycles`) handler. The component path already lowers these
    // handler shapes (against an optional `dut` handle + scoreboard/sub
    // fields); they reach it here even without an event pipe.
    if has_on_handler || has_periodic_handler {
        return true;
    }
    // Function-library transactor: pure methods, no DUT handle, no event,
    // no `on`. It lowers as a by-value struct with free-function methods
    // (mirroring v1's empty-struct + `<Comp>_<method>` lambdas), held as a
    // sub-field / test field and dispatched on. The DUT-poking
    // `TransactorSchema` path requires a DUT handle, so a handle-less
    // method-only transactor routes here instead.
    if transactor_is_function_library(t) {
        return true;
    }
    // Passive helper / monitor: DUT handle + always-on hookables, with no
    // active-only body to elide. This must lower as a component so a
    // `testbench` can hold `mon : Monitor passive` and call `mon.observe()`.
    if transactor_is_passive_helper(t) {
        return true;
    }
    // Purely structural DUT-poking BFM: `hookable` methods + a
    // module-typed DUT handle, no `on`/event/bound. The dedicated
    // `TransactorSchema` path lowers it as a top-level testbench field
    // (its method lambdas capture one specific instance), and that is its
    // DEFAULT routing — the unit/contract tests and every standalone
    // fixture rely on it staying a `TransactorSchema`. It routes to the
    // composite-component table ONLY when an `env`/`agent` must hold it by
    // value as a sub-component field (`env AxilEnv { drv : AxilXactor }`):
    // then it must be a `ComponentSchema` so the env can hold it by value
    // and `env.drv.<method>(...)` / `env.drv.dut = dut` resolve through the
    // component sub-component machinery. v1 emits both placements
    // identically (a struct with a `dut` handle + `<Type>_<method>(<Type>&
    // self, ...)` free-function lambdas), which is exactly the
    // component-method shape — so routing the env-held form here is
    // v1-faithful.
    //
    // The arms above already excluded every `on`/event/bound shape, so
    // `has_on_handler`/`has_periodic_handler` are both false here. We
    // still require `has_hookable && !has_event` so this stays EXACTLY in
    // lockstep with `transactor_is_dut_poking_bfm` (the classifier that
    // feeds the `active`-mode gate).
    env_held && has_hookable && has_module_field && !has_event
}

/// True for the reusable analysis-source form addressed by issue #534:
/// an unbound transactor with an output event surface and no module/DUT
/// handle. It is stored as a `ComponentSchema`, but its instance mode is
/// still a source-language transactor property.
pub(crate) fn transactor_is_analysis_source(t: &TransactorDecl) -> bool {
    if t.bound_to.is_some() {
        return false;
    }
    let mut has_event = false;
    let mut has_named_field = false;
    for it in t.items.iter().chain(t.when_active.iter().flatten()) {
        if let ComponentItem::Field(f) = it {
            if is_event_field(f) {
                has_event = true;
            } else if matches!(&f.ty, TypeExpr::Named { .. }) {
                has_named_field = true;
            }
        }
    }
    has_event && !has_named_field
}

/// Whether an analysis-source transactor has behavior or storage whose
/// availability depends on the instance mode.
pub(crate) fn transactor_has_mode_sensitive_analysis_surface(t: &TransactorDecl) -> bool {
    transactor_is_analysis_source(t)
        && t.when_active
            .as_ref()
            .is_some_and(|items| !items.is_empty())
}

/// True when a transactor routes to the COMPONENT path purely as a
/// **DUT-poking hookable BFM** — `hookable` methods + exactly the
/// module-typed DUT handle, with NO `on`/event handler and no `bound to`,
/// AND it is `env_held` (referenced as an env/agent sub-component field).
/// Exactly the transactor `transactor_is_component`'s trailing arm admits.
/// Such a transactor is a transactor at every binding site (its methods
/// live under `when active`), so it requires an explicit `active` mode
/// just like an event-driven consumer — even though it lowers to a
/// `ComponentSchema`. A `passive` instance has no methods at all. Feeds
/// the `dut_poking_bfm_names` `active`-mode gate; a NON-env-held BFM stays
/// a `TransactorSchema` whose mode is handled by the `transactor_ids` gate.
pub(crate) fn transactor_is_dut_poking_bfm(t: &TransactorDecl, env_held: bool) -> bool {
    if !env_held || t.bound_to.is_some() {
        return false;
    }
    let mut has_module_field = false;
    let mut has_event = false;
    let mut has_on_handler = false;
    let mut has_hookable = false;
    for it in t.items.iter().chain(t.when_active.iter().flatten()) {
        match it {
            ComponentItem::Field(f) => {
                if is_event_field(f) {
                    has_event = true;
                } else if matches!(&f.ty, TypeExpr::Named { .. }) {
                    has_module_field = true;
                }
            }
            ComponentItem::OnHandler(_) => has_on_handler = true,
            ComponentItem::Hookable(_) => has_hookable = true,
            _ => {}
        }
    }
    // Exactly the shape `transactor_is_component`'s trailing arm admits:
    // hookable BFM + DUT handle, no event/on/periodic, env-held.
    has_hookable && has_module_field && !has_event && !has_on_handler
}

/// True when a transactor is a DUT-attached passive helper/monitor:
/// always-on methods + a DUT handle, but no `when active` items, event
/// pipe, `on` handler, bound bus, or parameters. Since no method is
/// active-only, passive instances keep the same callable surface as active
/// instances.
pub(crate) fn transactor_is_passive_helper(t: &TransactorDecl) -> bool {
    if t.bound_to.is_some() || !t.params.is_empty() || t.when_active.is_some() {
        return false;
    }
    let mut has_module_field = false;
    let mut has_method = false;
    for it in &t.items {
        match it {
            ComponentItem::Hookable(_) => has_method = true,
            ComponentItem::Field(f) => {
                if is_event_field(f) {
                    return false;
                }
                if matches!(&f.ty, TypeExpr::Named { .. }) {
                    has_module_field = true;
                }
            }
            ComponentItem::OnHandler(_)
            | ComponentItem::TargetTlmThread(_)
            | ComponentItem::Watchdog(_)
            | ComponentItem::Connect(_)
            | ComponentItem::Lifecycle(..)
            | ComponentItem::Apply(_) => return false,
        }
    }
    has_module_field && has_method
}

/// True when a transactor is a *function library*: it carries at least one
/// `function`/`hookable` method but NO module-typed DUT handle, NO event
/// field, and NO `on`/periodic handler. Such a transactor holds no
/// per-instance behavior beyond its pure methods, so it lowers to a
/// component (by-value struct + free-function methods) rather than the
/// DUT-poking `TransactorSchema` (which structurally needs a DUT handle).
pub(crate) fn transactor_is_function_library(t: &TransactorDecl) -> bool {
    if t.bound_to.is_some() || !t.params.is_empty() {
        return false;
    }
    let mut has_method = false;
    for it in t.items.iter().chain(t.when_active.iter().flatten()) {
        match it {
            ComponentItem::Hookable(_) => has_method = true,
            ComponentItem::Field(f) => {
                if is_event_field(f) || matches!(&f.ty, TypeExpr::Named { .. }) {
                    // An event pipe or a module/transaction-typed field
                    // means this is not a pure function library.
                    return false;
                }
                // A scalar state field is allowed (persistent component
                // state alongside the pure methods).
            }
            ComponentItem::OnHandler(_)
            | ComponentItem::TargetTlmThread(_)
            | ComponentItem::Watchdog(_)
            | ComponentItem::Connect(_)
            | ComponentItem::Lifecycle(..)
            | ComponentItem::Apply(_) => return false,
        }
    }
    has_method
}

/// True when a transactor is an *event-driven consumer* — it has an
/// `in event<T>` field and a subscribing `on <ev>` handler. These accept
/// an `active`/`passive` instance mode (a transactor concept) even though
/// they route to the composite-component table; a pure analysis source
/// (out-event only, no DUT, no `on`) does not. (A reactive monitor /
/// checker — `on` handlers but no `in event` — is classified separately by
/// `transactor_is_reactive_monitor`; it accepts a `passive` instance too.)
pub(crate) fn transactor_is_event_driven(t: &TransactorDecl) -> bool {
    let mut has_in_event = false;
    let mut has_on_handler = false;
    for it in t.items.iter().chain(t.when_active.iter().flatten()) {
        match it {
            ComponentItem::Field(f)
                if is_event_field(f) && matches!(f.direction, Some(crate::ast::Direction::In)) =>
            {
                has_in_event = true;
            }
            ComponentItem::OnHandler(h) if !h.periodic => has_on_handler = true,
            _ => {}
        }
    }
    has_in_event && has_on_handler
}

/// True when a composite-component transactor is a *reactive monitor /
/// checker* — it has cycle-trigger and/or periodic `on` handlers but NO
/// `in event<T>` consumer pipe. Such an instance is purely observational
/// (its handlers are always-on, registered regardless of instance mode),
/// so unlike an event-driven consumer it accepts a `passive` instance —
/// there is no `when active` half whose `on req` registration a passive
/// instance would suppress.
pub(crate) fn transactor_is_reactive_monitor(t: &TransactorDecl) -> bool {
    let mut has_in_event = false;
    let mut has_on_handler = false;
    let mut has_periodic_handler = false;
    for it in t.items.iter().chain(t.when_active.iter().flatten()) {
        match it {
            ComponentItem::Field(f)
                if is_event_field(f) && matches!(f.direction, Some(crate::ast::Direction::In)) =>
            {
                has_in_event = true;
            }
            ComponentItem::OnHandler(h) if !h.periodic => has_on_handler = true,
            ComponentItem::OnHandler(_) => has_periodic_handler = true,
            _ => {}
        }
    }
    !has_in_event && (has_on_handler || has_periodic_handler)
}

fn is_event_field(f: &ComponentField) -> bool {
    matches!(
        &f.ty,
        TypeExpr::Builtin {
            name: BuiltinTy::Event,
            ..
        }
    )
}

/// One unit of component-lowering work, in file order: the source decl
/// plus its assigned id. Built by the caller's classification pass.
pub(crate) enum CompSource<'a> {
    Env(&'a ComponentDecl),
    Scoreboard(&'a ComponentDecl),
    Transactor(&'a TransactorDecl),
    /// `agent` — composes `on <ev>` self-subscriptions. Same
    /// `ComponentDecl` shape as env/scoreboard.
    Agent(&'a ComponentDecl),
    /// `sequencer` — a stimulus source. Same `ComponentDecl` shape as
    /// env/scoreboard: `out event<T>` ports + hookable methods that
    /// `emit` the generated stream (lowered exactly like an
    /// analysis-source transactor).
    Sequencer(&'a ComponentDecl),
}

/// Lower one component's STRUCTURE (fields + method signatures), without
/// method bodies. `ids` maps already-classified component names to their
/// ids so a `Sub` field / `connect` edge can resolve nested components.
/// Method `FunctionId`s are assigned from `next_fn` upward; bodies are
/// lowered in the second pass (`lower_component_bodies`).
pub(crate) fn lower_component_schema(
    src: &CompSource<'_>,
    ids: &HashMap<String, ComponentId>,
    scoreboard_ids: &HashMap<String, ScoreboardId>,
    record_ids: &HashMap<String, RecordId>,
    next_fn: &mut u32,
    consts: &HashMap<String, super::ConstVal>,
    declared_types: &std::collections::HashSet<String>,
) -> Result<ComponentSchema, LowerError> {
    let (name, kind, items, when_active): (
        &str,
        ComponentKindTag,
        &[ComponentItem],
        Option<&[ComponentItem]>,
    ) = match src {
        CompSource::Env(c) => (&c.name.name, ComponentKindTag::Env, &c.items, None),
        CompSource::Agent(c) => (&c.name.name, ComponentKindTag::Agent, &c.items, None),
        CompSource::Sequencer(c) => (&c.name.name, ComponentKindTag::Sequencer, &c.items, None),
        CompSource::Scoreboard(c) => (&c.name.name, ComponentKindTag::Scoreboard, &c.items, None),
        CompSource::Transactor(t) => (
            &t.name.name,
            ComponentKindTag::Transactor,
            &t.items,
            t.when_active.as_deref(),
        ),
    };
    // A `bound to <Bus>` transactor that routes here is the bound-bus
    // event-driven consumer BFM: its `on <ev>` handler bodies drive the
    // bound bus's handshake channels instead of a private DUT handle.
    // Resolve the bus name (validated against the test's bus binding at
    // test-binding time, mirroring the bound-initiator path); reject the
    // out-of-subset generic-applied / non-named bound type precisely.
    let mut bound_bus: Option<String> = None;
    if let CompSource::Transactor(t) = src {
        if !t.params.is_empty() {
            // Same three outcomes as the env/agent/scoreboard/sequencer
            // arm below, probed separately rather than assumed from it.
            // v1 never reads a component's `#(...)` parameter list:
            //
            //   * declared but unused — output is BYTE-IDENTICAL to the
            //     same declaration without the parameter, and passing
            //     `#(4)` at the instantiation changes nothing. Nothing
            //     is mis-lowered; the parameter simply did nothing.
            //     (Anchored: the component contributes, and a field
            //     `default` is visible in the output, so the identity is
            //     the parameter being dropped rather than the component
            //     being inert.)
            //   * referenced with no name to fall back on — e.g.
            //     `limit : uint<32> default N` emits `uint64_t limit = N;`
            //     with `N` declared nowhere. `EmitsUncompilable`.
            //   * referenced from a HANDLER BODY while a file-scope
            //     `const N = 9` exists — v1 emits
            //     `static constexpr int64_t N = 9;` at namespace scope
            //     and the use lands well after it, so the file COMPILES
            //     and the component silently uses 9 instead of the `#(4)`
            //     the instantiation passed. `SilentlyMisLowers`, and the
            //     only case that earns the arm's label.
            //
            // The POSITION of the reference decides which of the last two
            // applies, and it is not intuition: v1 emits the const AFTER
            // the component struct, so a field default (`limit = N`,
            // inside the struct) still fails to compile even with the
            // const present, while a handler-body use (emitted much
            // later) resolves. Both were checked by splicing the emitted
            // region into g++ with the generated file's own header set,
            // against a control that moves the const and changes nothing
            // else.
            //
            // The first version of this classification named the
            // field-default case as the compiling one, on two `contains`
            // checks that never looked at order. Same label, and the
            // evidence for it was wrong twice running.
            return Err(not_implemented(
                &format!("generic parameters on analysis-source `{name}`"),
                "v1 drops the parameter list entirely: an unused parameter vanishes along \
                 with any `#(...)` argument at the instantiation, and a reference to one \
                 either fails to resolve or silently picks up a same-named file-scope \
                 `const`, depending on where in the emitted file the reference lands",
                V1Status::SilentlyMisLowers,
            ));
        }
        if let Some(bt) = t.bound_to.as_ref() {
            match bt {
                TypeExpr::Named {
                    name: bn, generics, ..
                } => {
                    if !generics.is_empty() {
                        return Err(unsupported(
                            &format!(
                                "event-driven transactor `{name}` bound to a generic-applied \
                                 bus type"
                            ),
                            "",
                        ));
                    }
                    bound_bus = Some(
                        bn.segments
                            .last()
                            .map(|s| s.name.clone())
                            .unwrap_or_default(),
                    );
                }
                _ => {
                    return Err(unsupported(
                        &format!("event-driven transactor `{name}` bound to a non-named bus type"),
                        "",
                    ));
                }
            }
        }
    }
    if let CompSource::Env(c)
    | CompSource::Scoreboard(c)
    | CompSource::Agent(c)
    | CompSource::Sequencer(c) = src
    {
        if !c.params.is_empty() {
            // See the analysis-source arm above for the three outcomes
            // and which one earns the label.
            return Err(not_implemented(
                &format!("parameters on `{name}`"),
                "v1 drops the parameter list entirely: an unused parameter vanishes along \
                 with any `#(...)` argument at the instantiation, and a reference to one \
                 either fails to resolve or silently picks up a same-named file-scope \
                 `const`, depending on where in the emitted file the reference lands",
                V1Status::SilentlyMisLowers,
            ));
        }
        if c.bound_to.is_some() {
            return Err(unsupported(&format!("a `bound to` clause on `{name}`"), ""));
        }
    }

    let mut fields: Vec<ComponentFieldSchema> = Vec::new();
    let mut methods: Vec<ComponentMethodSchema> = Vec::new();
    // On-handler ASTs are collected here and resolved after the field
    // loop, so a handler may textually precede the event field it
    // subscribes to (field lookup needs the full field set). Event-
    // subscription (`on ev(arg)`) and periodic (`on N cycles`) forms are
    // split so each reserves its FunctionId in the right contiguous block.
    let mut on_asts: Vec<(&crate::ast::OnHandler, Activation)> = Vec::new();
    let mut periodic_asts: Vec<(&crate::ast::OnHandler, Activation)> = Vec::new();
    // Cycle-trigger handlers (`on <bool-expr> ... end on`) — the monitor
    // half. Distinguished from event-subscription `on ev(arg)` by the
    // trigger expression shape (a non-`Call`, or a `Call` whose callee is
    // not a self-event field). Resolved after the field loop. The
    // `Option<String>` is the bound-bus channel for an `on
    // bus.<ch>.handshake(arg)` handshake-monitor (`None` for an agent-mode
    // raw-signal `on <bool-expr>`); a monitor handler synthesizes its
    // `valid && ready` trigger + payload capture in pass 2.
    let mut cycle_asts: Vec<(&crate::ast::OnHandler, Option<String>, Activation)> = Vec::new();
    // At most one `watchdog` per component (the second is rejected).
    let mut watchdog_ast: Option<(&crate::ast::WatchdogDecl, Activation)> = None;
    // An event-driven transactor may carry a module-typed DUT handle field
    // (`dut : AxiLiteRegs`); a Named non-component type is then a DUT
    // pointer rather than an unknown sub-component. Only transactors host
    // a DUT field — env/scoreboard/agent/sequencer never do.
    let is_transactor = matches!(src, CompSource::Transactor(_));
    // A transactor with a `bound to <bus>` clause. `transactor_is_component`
    // routes one here when it has a non-periodic `on` handler, so a
    // `thread` item below can be sitting on exactly the construct that
    // serves it — v1 emits the target actor for that shape, and the
    // component arms must not claim otherwise.
    let is_bound_transactor = matches!(src, CompSource::Transactor(t) if t.bound_to.is_some());
    for (activation, body) in std::iter::once((Activation::Always, items)).chain(
        when_active
            .iter()
            .map(|active_items| (Activation::ActiveOnly, *active_items)),
    ) {
        for it in body {
            match it {
                ComponentItem::Field(f) => {
                    let fk = lower_field(
                        name,
                        f,
                        ids,
                        scoreboard_ids,
                        record_ids,
                        is_transactor,
                        consts,
                        declared_types,
                    )?;
                    if fields.iter().any(|x| x.name == f.name.name) {
                        return Err(LowerError::Invalid(format!(
                            "component `{name}` declares field `{}` more than once",
                            f.name.name
                        )));
                    }
                    fields.push(ComponentFieldSchema {
                        name: f.name.name.clone(),
                        kind: fk,
                        activation,
                    });
                }
                ComponentItem::Hookable(h) => {
                    let n_params = h.params.len();
                    let param_tys = h
                        .params
                        .iter()
                        .map(|p| method_schema_ir_type(p.ty.as_ref(), ids, record_ids))
                        .collect();
                    let has_ret = h.return_ty.is_some();
                    let ret_ty = h
                        .return_ty
                        .as_ref()
                        .map(|t| method_schema_ir_type(Some(t), ids, record_ids));
                    let fid = FunctionId(*next_fn);
                    *next_fn += 1;
                    methods.push(ComponentMethodSchema {
                        name: h.name.name.clone(),
                        function: fid,
                        param_tys,
                        n_params,
                        has_ret,
                        ret_ty,
                        hookable: h.is_hookable,
                        cov_hook_subs: Vec::new(),
                        activation,
                    });
                }
                // Connect ownership is modeled only for env/agent and
                // reusable-testbench composition. A transactor-owned block
                // used to be silently discarded here, including inside
                // `when active`; reject it until that ownership has an IR
                // representation with activation provenance.
                ComponentItem::Connect(_) if is_transactor => {
                    let placement = if matches!(activation, Activation::ActiveOnly) {
                        " inside `when active`"
                    } else {
                        ""
                    };
                    return Err(unsupported(
                        &format!("a `connect` declaration{placement} on transactor `{name}`"),
                        "connect declarations are supported on env, agent, and testbench composition, not on analysis-source transactors",
                    ));
                }
                // Testbench lifecycle blocks are parser-restricted already;
                // retain a precise lowerer guard for malformed AST input.
                ComponentItem::Lifecycle(..) if is_transactor => {
                    return Err(unsupported(
                        &format!("a lifecycle declaration on transactor `{name}`"),
                        "lifecycle declarations are supported only on testbench composition",
                    ));
                }
                // Connect blocks are resolved separately (env-binding stage).
                ComponentItem::Connect(_) | ComponentItem::Lifecycle(..) => {}
                ComponentItem::OnHandler(h) if h.periodic => periodic_asts.push((h, activation)),
                // `on bus.<ch>.handshake(arg)` — the passive bus-monitor half
                // of a bound transactor (v1's `emit_bound_monitor_actors`).
                // Collected into `on_asts` like every other non-periodic
                // handler; the classification loop below desugars it into a
                // cycle-trigger handler. Keeping all non-periodic handlers in
                // one source-ordered list is what lets pass-1 FunctionId
                // reservation and pass-2 body lowering re-classify identically.
                ComponentItem::OnHandler(h) => on_asts.push((h, activation)),
                ComponentItem::Watchdog(w) => {
                    if watchdog_ast.is_some() {
                        return Err(unsupported(
                            &format!("a second `watchdog` on `{name}`"),
                            "a component may declare at most one `watchdog`",
                        ));
                    }
                    watchdog_ast = Some((w, activation));
                }

                // Two variants shared one message and one classification.
                // Only the first is probed at THIS landing, so only the first
                // is reclassified — the second keeps what it had rather than
                // inheriting a verdict it did not earn.
                // A `thread` on a BOUND transactor is the construct working
                // as designed — `emit_bound_tlm_target_actors` emits the
                // target actor for it, and this component path is reached
                // only because the transactor also has a non-periodic `on`
                // handler. v1 is a real escape hatch, so it keeps
                // `Unsupported` and the `--codegen v1` pointer with it.
                //
                // The first version of this split reclassified the whole arm
                // from a probe that only ever put a `thread` on an env.
                ComponentItem::TargetTlmThread(_) if is_bound_transactor => {
                    return Err(unsupported(
                        &format!(
                            "a `thread` item on bound transactor `{name}` reached through the \
                             component path"
                        ),
                        "the target actor lowers on the transactor path; this component path is \
                         taken because the transactor also has a non-periodic `on` handler",
                    ));
                }
                ComponentItem::TargetTlmThread(_) => {
                    // v1 accepts a `thread` on an env/agent/scoreboard and
                    // emits the component struct WITHOUT it: no
                    // `harc_rt::ThreadSlot`, no `sched.slots.push_back`, no
                    // serving coroutine. The target never serves, silently.
                    //
                    // Anchored in both directions, because "v1's output did
                    // not change" is not by itself evidence of dropping —
                    // an unbound `thread` emits nothing under v1 wherever it
                    // sits, so the first probe proved nothing. Against
                    // `tlm_target_thread_test`, where the transactor IS
                    // bus-bound, removing the thread DOES change v1's output
                    // (it loses the ThreadSlot and the coroutine); moving
                    // that same thread into an `env` adds only an empty
                    // `struct WrapEnv { ... }`.
                    return Err(not_implemented(
                        &format!("a `thread` item in component `{name}`"),
                        "a target-serving `thread` belongs on a `transactor ... bound to <bus>`; on \
                         an env/agent/scoreboard v1 emits the component without it, so the target \
                         silently never serves",
                        V1Status::SilentlyMisLowers,
                    ));
                }
                // NOT probed at this landing. v1's handling of `apply`
                // differs by position and by whether the named package is
                // declared — in a test body a declared package is rejected
                // while an undeclared name is accepted — so the component
                // landing needs its own anchored probe before any claim
                // about v1 is made here.
                ComponentItem::Apply(_) => {
                    // Detail deliberately says nothing about WHERE to put it:
                    // test scope rejects `apply` too (`TestItem::Apply` in
                    // `mod.rs`), so naming that scope would send the user
                    // somewhere that also fails.
                    return Err(unsupported(
                        &format!("an `apply` item in component `{name}`"),
                        "",
                    ));
                }
            }
        }
    }

    // An event-driven transactor pokes its DUT through the `on`-handler
    // body's `ctx.dut_field` (conventionally `dut`). The handler body
    // ctx hardwires that name, so a DUT field must be named `dut` and
    // there can be at most one — anything else would silently fail to
    // lower a `<field>.<sig>` access as a DUT port write.
    let dut_fields: Vec<&str> = fields
        .iter()
        .filter(|f| matches!(f.kind, ComponentFieldKind::Dut { .. }))
        .map(|f| f.name.as_str())
        .collect();
    // A bound-bus event-driven transactor drives the bound bus's wires, not
    // a private DUT handle — a module-typed field on it is ambiguous.
    if bound_bus.is_some() && !dut_fields.is_empty() {
        return Err(unsupported(
            &format!(
                "bound-to event-driven transactor `{name}` with a module-typed (DUT handle) \
                 field ({})",
                dut_fields.join(", ")
            ),
            "a bound-to transactor drives the bound bus's wires on the test DUT; it has no \
             private DUT handle",
        ));
    }
    if dut_fields.len() > 1 {
        return Err(unsupported(
            &format!(
                "an event-driven transactor `{name}` with more than one DUT handle field \
                 ({})",
                dut_fields.join(", ")
            ),
            "the consumer BFM drives exactly one DUT instance",
        ));
    }
    if let Some(&df) = dut_fields.first() {
        if df != "dut" {
            return Err(unsupported(
                &format!("an event-driven transactor DUT handle field named `{df}`"),
                "name the DUT handle `dut` (the handler body resolves DUT pokes through it)",
            ));
        }
    }

    // Resolve on-handlers. Two non-periodic shapes coexist:
    //   * `on <event>(arg)` — a self-event subscription (the driver half).
    //   * `on <bool-expr>` — a cycle-trigger monitor (the observer half).
    // A handler is event-subscription iff its trigger is a `Call` whose
    // callee names a self `event<...>` field; anything else (a bare bool
    // expr, or a `Call` on a non-event identifier) is a cycle-trigger.
    // The first form reserves a FunctionId here; cycle-triggers reserve
    // theirs AFTER the periodic block (kept contiguous for pass 2).
    let mut on_handlers: Vec<crate::ir::OnHandlerSchema> = Vec::new();
    for (h, activation) in &on_asts {
        if is_bus_handshake_monitor(h) {
            // `on bus.<ch>.handshake(arg)` — desugars to a cycle-trigger
            // handler (valid && ready, rising edge) observing the bound
            // bus channel. Only valid on a `bound to <Bus>` transactor.
            if bound_bus.is_none() {
                return Err(unsupported(
                    &format!(
                        "an `on bus.<ch>.handshake(...)` handshake-monitor handler on \
                         non-bound component `{name}`"
                    ),
                    "handshake-monitor handlers observe a `bound to <Bus>` transactor's \
                     channels; an unbound component has no bus to observe",
                ));
            }
            let channel = bus_handshake_monitor_channel(h).ok_or_else(|| {
                unsupported(
                    &format!("a malformed `on bus.<ch>.handshake(...)` handler on `{name}`"),
                    "the trigger must be `bus.<channel>.handshake(<arg>)`",
                )
            })?;
            cycle_asts.push((h, Some(channel), *activation));
        } else if is_event_subscription(h, &fields) {
            let (event, arg_payload) = resolve_on_handler_event(name, h, &fields)?;
            let fid = FunctionId(*next_fn);
            *next_fn += 1;
            on_handlers.push(crate::ir::OnHandlerSchema {
                event,
                arg_payload,
                function: fid,
                activation: *activation,
            });
        } else {
            cycle_asts.push((h, None, *activation));
        }
    }

    // Periodic handlers (`on <N> cycles`). Reserved AFTER the event
    // handlers so the FunctionId blocks stay contiguous in pass-2
    // declaration order (methods → event on-handlers → periodic → watchdog).
    // The period expression lowers in pass 2 (it may reference component
    // fields); pass 1 records a placeholder.
    let mut periodic_handlers: Vec<crate::ir::PeriodicHandlerSchema> = Vec::new();
    for (h, activation) in &periodic_asts {
        validate_periodic_handler(name, h)?;
        let fid = FunctionId(*next_fn);
        *next_fn += 1;
        periodic_handlers.push(crate::ir::PeriodicHandlerSchema {
            period: crate::ir::Expr::CycleCount, // placeholder; pass 2 fills it
            function: fid,
            phase: crate::ir::HandlerPhase::from_ast(h.phase),
            activation: *activation,
        });
    }

    // Cycle-trigger handlers (`on <bool-expr>`). Reserved AFTER periodic
    // and BEFORE the watchdog so the FunctionId blocks stay contiguous in
    // pass-2 declaration order (methods → event on-handlers → periodic →
    // cycle-trigger → watchdog). The trigger predicate lowers in pass 2
    // (it reads DUT/component fields); pass 1 records a placeholder.
    let mut cycle_handlers: Vec<crate::ir::CycleTriggerHandlerSchema> = Vec::new();
    for (h, monitor_channel, activation) in &cycle_asts {
        validate_cycle_handler(name, h)?;
        let fid = FunctionId(*next_fn);
        *next_fn += 1;
        // A handshake-monitor synthesizes a rising-edge `valid && ready`
        // trigger (the user wrote no explicit edge); an agent-mode
        // `on <bool-expr>` honors the source edge mode.
        let edge = if monitor_channel.is_some() {
            crate::ir::CycleEdge::Rising
        } else {
            edge_to_ir(h.edge)
        };
        cycle_handlers.push(crate::ir::CycleTriggerHandlerSchema {
            trigger: crate::ir::Expr::CycleCount, // placeholder; pass 2 fills it
            edge,
            function: fid,
            monitor_channel: monitor_channel.clone(),
            activation: *activation,
        });
    }

    // Watchdog (at most one). A `disabled` watchdog emits nothing — no
    // FunctionId, no schema entry (mirrors v1's `emit_watchdog` early
    // return). The body FunctionId is reserved LAST; period/max_idle
    // lower in pass 2.
    let watchdog = match watchdog_ast {
        Some((w, activation)) if !w.disabled => {
            let fid = FunctionId(*next_fn);
            *next_fn += 1;
            Some(crate::ir::WatchdogSchema {
                period: None,   // pass 2 fills from `w.period`
                max_idle: None, // pass 2 fills from `w.max_idle`
                function: fid,
                activation,
            })
        }
        _ => None,
    };

    let instance_mode_policy = match src {
        CompSource::Transactor(t)
            if transactor_is_analysis_source(t)
                && !transactor_has_mode_sensitive_analysis_surface(t) =>
        {
            crate::ir::ComponentInstanceModePolicy::AlwaysOnAnalysisMonitor
        }
        _ => crate::ir::ComponentInstanceModePolicy::Standard,
    };

    Ok(ComponentSchema {
        name: name.to_string(),
        kind,
        instance_mode_policy,
        fields,
        methods,
        // Connects resolved in a third pass once all schemas exist.
        connects: Vec::new(),
        on_handlers,
        periodic_handlers,
        cycle_handlers,
        watchdog,
        bound_bus,
    })
}

/// Reject source shapes whose parsed mode annotation would otherwise be
/// representable in TB-IR but meaningless: only transactor children may carry
/// an explicit active/passive override, and only transactors may declare a
/// `when active` member. The verifier repeats these checks for mutated IR.
pub(crate) fn validate_mode_metadata(components: &[ComponentSchema]) -> Result<(), LowerError> {
    for component in components {
        let active_member = component
            .fields
            .iter()
            .any(|field| matches!(field.activation, Activation::ActiveOnly))
            || component
                .methods
                .iter()
                .any(|method| matches!(method.activation, Activation::ActiveOnly))
            || component
                .on_handlers
                .iter()
                .any(|handler| matches!(handler.activation, Activation::ActiveOnly))
            || component
                .periodic_handlers
                .iter()
                .any(|handler| matches!(handler.activation, Activation::ActiveOnly))
            || component
                .cycle_handlers
                .iter()
                .any(|handler| matches!(handler.activation, Activation::ActiveOnly))
            || component
                .watchdog
                .as_ref()
                .is_some_and(|handler| matches!(handler.activation, Activation::ActiveOnly));
        if active_member && !matches!(component.kind, ComponentKindTag::Transactor) {
            return Err(LowerError::Invalid(format!(
                "{} `{}` declares active-only members, but only transactors have an active surface",
                component.kind.keyword(),
                component.name
            )));
        }
        for field in &component.fields {
            let ComponentFieldKind::Sub {
                component: child,
                mode: Some(_),
            } = &field.kind
            else {
                continue;
            };
            let child_schema = components.get(child.index()).ok_or_else(|| {
                LowerError::Invalid(format!(
                    "{} `{}` field `{}` references missing component c{}",
                    component.kind.keyword(),
                    component.name,
                    field.name,
                    child.0
                ))
            })?;
            if !matches!(child_schema.kind, ComponentKindTag::Transactor) {
                return Err(LowerError::Invalid(format!(
                    "a transactor mode on {} field `{}.{}`",
                    child_schema.kind.keyword(),
                    component.name,
                    field.name
                )));
            }
        }
    }
    Ok(())
}

/// True when `h` is an `on <event>(arg)` self-event subscription: its
/// trigger is a `Call` whose callee is a bare identifier naming a self
/// `event<...>` field. Everything else (a bare bool predicate, a `Call`
/// on a non-event name) is a cycle-trigger monitor handler.
fn is_event_subscription(h: &crate::ast::OnHandler, fields: &[ComponentFieldSchema]) -> bool {
    let ExprKind::Call { callee, .. } = &*h.event.kind else {
        return false;
    };
    let ExprKind::Ident(id) = &*callee.kind else {
        return false;
    };
    fields
        .iter()
        .any(|f| f.name == id.name && matches!(f.kind, ComponentFieldKind::Event { .. }))
}

/// True when `h` is an `on bus.<ch>.handshake(arg)` passive bus-monitor
/// handler: its trigger is a `Call` whose callee is a `<bus>.<ch>.handshake`
/// field-path. (The `bus` head is the bound-bus placeholder keyword inside
/// a bound transactor; any dotted `.handshake(...)` call is the monitor
/// shape — distinct from a bare-ident event subscription.)
fn is_bus_handshake_monitor(h: &crate::ast::OnHandler) -> bool {
    let ExprKind::Call { callee, .. } = &*h.event.kind else {
        return false;
    };
    let ExprKind::Field {
        target,
        name: method,
    } = &*callee.kind
    else {
        return false;
    };
    if method.name != "handshake" {
        return false;
    }
    // `<x>.<ch>.handshake` — the target must itself be a `<x>.<ch>` field.
    matches!(&*target.kind, ExprKind::Field { .. })
}

/// The channel name of an `on bus.<ch>.handshake(arg)` monitor handler
/// (`<ch>` from the `<bus>.<ch>.handshake` callee), or `None` if `h` is
/// not a well-formed handshake-monitor.
fn bus_handshake_monitor_channel(h: &crate::ast::OnHandler) -> Option<String> {
    let ExprKind::Call { callee, .. } = &*h.event.kind else {
        return None;
    };
    // callee = `<bus>.<ch>.handshake`; its target is `<bus>.<ch>`.
    let ExprKind::Field {
        target,
        name: method,
    } = &*callee.kind
    else {
        return None;
    };
    if method.name != "handshake" {
        return None;
    }
    let ExprKind::Field { name: ch, .. } = &*target.kind else {
        return None;
    };
    Some(ch.name.clone())
}

/// The argument name of an `on bus.<ch>.handshake(<arg>)` monitor handler
/// (`_beat` when not a plain identifier).
fn bus_handshake_monitor_arg_name(h: &crate::ast::OnHandler) -> String {
    if let ExprKind::Call { args, .. } = &*h.event.kind {
        if let Some(crate::ast::CallArg::Expr(e)) = args.first() {
            if let ExprKind::Ident(id) = &*e.kind {
                return id.name.clone();
            }
        }
    }
    "_beat".to_string()
}

/// Map the AST edge mode onto the IR cycle-edge enum.
fn edge_to_ir(e: crate::ast::EdgeMode) -> crate::ir::CycleEdge {
    match e {
        crate::ast::EdgeMode::Rising => crate::ir::CycleEdge::Rising,
        crate::ast::EdgeMode::Falling => crate::ir::CycleEdge::Falling,
        crate::ast::EdgeMode::Level => crate::ir::CycleEdge::Level,
    }
}

/// Validate an `on <bool-expr> ... end on` cycle-trigger handler: no
/// `pre`/`post` hook side, and (in this subset) the default `Checker`
/// phase. A `post_eval`-phased cycle-trigger is the reactive monitor form
/// handled elsewhere; only the checker phase is lowered here.
fn validate_cycle_handler(comp: &str, h: &crate::ast::OnHandler) -> Result<(), LowerError> {
    if h.hook.is_some() {
        // v1 drops the hook side and lowers the trigger as an ordinary
        // cycle trigger. TWO user-visible shapes reach here and v1 fails
        // each differently, so the label is the worse of the two:
        //
        //   * `on <bool-expr> pre` — a stray modifier on a genuine
        //     cycle trigger. v1's output is BYTE-IDENTICAL to the same
        //     handler written without the hook, so the requested
        //     ordering is silently ignored. (Anchored: removing the
        //     handler entirely does change v1's output, so the identity
        //     is the modifier being dropped, not the handler being
        //     inert.) `SilentlyMisLowers`, and the worst of the two.
        //
        //   * `on <sub>.<method> pre` — a spec §7.3 method hook written
        //     in a component body instead of at test scope. Not an
        //     event subscription and not a handshake monitor, so it
        //     lands here too. v1 drops the hook AND edge-detects on
        //     `(bool)(e.s.send)`, a member that the emitted struct does
        //     not have, so the C++ does not compile. `EmitsUncompilable`
        //     on its own, and subsumed by the arm's label.
        //
        // Both are `NotImplemented`: v1 is not an escape hatch for
        // either. The detail below is worded to stay true of both
        // rather than describing only the byte-identical one.
        //
        // It deliberately does NOT say "write the hook at test scope".
        // That reads as a fix and is one only for a hook on a DIRECT
        // transactor testbench field; the nested path a component body
        // implies (`on e.s.send pre`) is rejected at test scope too, by
        // `resolve_method_hook_target`. A hint naming a destination that
        // also fails is worse than no hint.
        return Err(not_implemented(
            &format!("a `pre`/`post` hook on a cycle-trigger `on` handler on `{comp}`"),
            "cycle-trigger handlers take no hook side; v1 accepts one, drops the hook and \
             lowers the trigger as a plain cycle trigger, so the requested ordering is lost",
            V1Status::SilentlyMisLowers,
        ));
    }
    if !matches!(h.phase, crate::ast::OnPhase::Checker) {
        // NOT the same: v1 implements this one. `phase post_eval` emits
        // `_post_eval_services.push_back` where the default emits
        // `_checkers.push_back` — the phase selects the dispatch vector
        // and it works, so `--codegen v1` is a real escape hatch.
        //
        // The two arms sit four lines apart and looked interchangeable.
        // They were probed with a shared control that changed the
        // trigger AND the modifier at once, which made both read as
        // "differs"; against a one-token control they split.
        return Err(unsupported(
            &format!("a non-default-phase cycle-trigger `on` handler on `{comp}`"),
            "only the default (checker) phase is lowered for cycle-trigger handlers",
        ));
    }
    Ok(())
}

/// Validate an `on <N> cycles ... end on` periodic handler: it must carry
/// no `pre`/`post` hook side. Both the default `Checker` phase and the
/// `phase post_eval` form lower (the phase selects the dispatch vector —
/// `_checkers` vs `_post_eval_services`).
fn validate_periodic_handler(comp: &str, h: &crate::ast::OnHandler) -> Result<(), LowerError> {
    if h.hook.is_some() {
        // Same as the cycle-trigger hook above: v1 emits the periodic
        // handler with the hook side discarded, byte-identically to the
        // same handler written without it.
        return Err(not_implemented(
            &format!("a `pre`/`post` hook on an `on <N> cycles` handler on `{comp}`"),
            "periodic handlers take no hook side; v1 accepts one and emits the handler \
             without it, so the requested ordering is silently ignored",
            V1Status::SilentlyMisLowers,
        ));
    }
    Ok(())
}

/// Validate an `on <event>(arg) ... end on` handler: it must be a bare
/// event-subscription (`on in_ev(t)`) to a self `event<scalar>` field,
/// with no `pre`/`post` hook side, no edge/periodic trigger. Returns the
/// `(event_field_name, arg_payload)`.
fn resolve_on_handler_event(
    comp: &str,
    h: &crate::ast::OnHandler,
    fields: &[ComponentFieldSchema],
) -> Result<(String, EventPayload), LowerError> {
    if h.hook.is_some() {
        // The third member of the hook family, and it probes exactly
        // like the periodic one: v1 emits the subscription handler with
        // the hook side discarded, byte-identically to the same handler
        // written without it. (Anchored: removing the handler changes
        // v1's output.) Nothing here needs the method-hook caveat that
        // `validate_cycle_handler` carries — the trigger is already
        // known to be a self `event<...>` subscription, so there is only
        // one shape.
        return Err(not_implemented(
            &format!("a `pre`/`post` hook `on` handler on `{comp}`"),
            "only bare `on <event>(arg)` self-subscriptions are lowered; v1 accepts a hook \
             side and emits the handler without it, so the requested ordering is silently \
             ignored",
            V1Status::SilentlyMisLowers,
        ));
    }
    if h.periodic {
        return Err(unsupported(
            &format!("an `on <N> cycles` periodic handler on `{comp}`"),
            "periodic/cycle-trigger handlers gate on a later slice",
        ));
    }
    // Event-subscription shape: `on <event>(<arg>)`.
    let ExprKind::Call { callee, args } = &*h.event.kind else {
        return Err(unsupported(
            &format!("a cycle-trigger `on <expr>` handler on `{comp}`"),
            "only `on <event>(arg)` self-subscriptions are lowered",
        ));
    };
    let event = match &*callee.kind {
        ExprKind::Ident(id) => id.name.clone(),
        _ => {
            return Err(unsupported(
                &format!("a dotted-path `on` handler event on `{comp}`"),
                "only a bare self-event name is lowered",
            ));
        }
    };
    if args.len() != 1 {
        return Err(unsupported(
            &format!("an `on {event}(...)` handler with {} arguments", args.len()),
            "event handlers take exactly one payload argument",
        ));
    }
    // The event must name a self `event<...>` field.
    match fields.iter().find(|f| f.name == event).map(|f| &f.kind) {
        Some(ComponentFieldKind::Event { payload }) => Ok((event, *payload)),
        Some(_) => Err(unsupported(
            &format!("`on {event}` — `{comp}.{event}` is not an `event` field"),
            "",
        )),
        None => Err(unsupported(
            &format!("`on {event}` — `{comp}` has no field `{event}`"),
            "",
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn lower_field(
    comp: &str,
    f: &ComponentField,
    ids: &HashMap<String, ComponentId>,
    scoreboard_ids: &HashMap<String, ScoreboardId>,
    record_ids: &HashMap<String, RecordId>,
    is_transactor: bool,
    consts: &HashMap<String, super::ConstVal>,
    // Every type NAME declared anywhere in the file. Used only to tell a
    // typo from a declared-but-unsupported sub-component kind — v1 does
    // very different things to the two, and the site had been treating
    // them alike.
    declared_types: &std::collections::HashSet<String>,
) -> Result<ComponentFieldKind, LowerError> {
    let fname = &f.name.name;
    if f.bound_to.is_some() {
        return Err(unsupported(
            &format!("a `bound to` clause on field `{comp}.{fname}`"),
            "",
        ));
    }
    match &f.ty {
        // `observed : out event<T>` analysis port, a directionless
        // `in_ev : event<T>` agent self-event, or an `in event<T>` input
        // pipe on an event-driven transactor (subscribed via `on`). All
        // lower to the same `std::vector<std::function<void(T)>>` callback
        // list — direction is a source-level role marker (producer vs
        // consumer), not a distinct runtime shape: the consumer's `on`
        // handler registers a subscriber, and an `emit`/`connect` bridge
        // fans out over that same list. An `inout` event remains out of
        // subset. An `in event` is accepted only on a transactor (the
        // consumer-BFM form); on an env/scoreboard/agent/sequencer it is
        // still rejected.
        TypeExpr::Builtin {
            name: BuiltinTy::Event,
            args,
            ..
        } => {
            if matches!(f.direction, Some(Direction::InOut)) {
                return Err(unsupported(
                    &format!("an `inout` event field `{comp}.{fname}`"),
                    "only `out event<T>` analysis ports, directionless agent \
                     self-events, and `in event<T>` transactor input pipes are lowered",
                ));
            }
            if matches!(f.direction, Some(Direction::In)) && !is_transactor {
                return Err(unsupported(
                    &format!("an `in` event field `{comp}.{fname}`"),
                    "an `in event<T>` input pipe is only lowered on an event-driven \
                     transactor (consumer BFM); use a directionless self-event elsewhere",
                ));
            }
            let payload = lower_event_payload(comp, fname, args.first(), record_ids)?;
            Ok(ComponentFieldKind::Event { payload })
        }
        // `expected : queue<T>` FIFO.
        TypeExpr::Builtin {
            name: BuiltinTy::Queue,
            args,
            ..
        } => {
            if f.direction.is_some() {
                return Err(unsupported(
                    &format!("a directional queue field `{comp}.{fname}`"),
                    "",
                ));
            }
            // The element is a scalar ≤ 64 bits or a value-record
            // (`errors : queue<CheckerError>`). A record element lowers to a
            // `harc_rt::HarcQueue<Rec>` and is manipulated through the
            // component-queue ops; anything else (enum / Vec / >64-bit /
            // unknown named type) is rejected precisely.
            let elem = lower_queue_elem(comp, fname, args.first(), record_ids)?;
            Ok(ComponentFieldKind::Queue { elem })
        }
        // Scalar counter, or a nested sub-component (`source :
        // AnalysisSource passive` / `sb : AnalysisSb`).
        TypeExpr::Builtin { .. } => {
            if f.direction.is_some() {
                return Err(unsupported(
                    &format!("a directional scalar field `{comp}.{fname}`"),
                    "",
                ));
            }
            let ty = scalar_ir_type(&f.ty).ok_or_else(|| {
                unsupported(
                    &format!("scalar field `{comp}.{fname}` of an unsupported type"),
                    "only uint/sint/bits/bool fields up to 64 bits are lowered",
                )
            })?;
            let default = scalar_default(&f.default, comp, fname, &f.ty, consts)?;
            Ok(ComponentFieldKind::Scalar { ty, default })
        }
        TypeExpr::Named { name, mode, .. } => {
            if f.direction.is_some() {
                return Err(unsupported(
                    &format!("a directional named-type field `{comp}.{fname}`"),
                    "",
                ));
            }
            let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
            // A data-only `scoreboard` (no methods) lowers to a
            // `ScoreboardSchema`, not a `ComponentSchema`, so it never
            // appears in `ids`. Resolve it as a by-value scoreboard
            // sub-component (a quiesce leaf) — mirrors v1, which holds the
            // board struct inside the env by value.
            if let Some(sid) = scoreboard_ids.get(simple) {
                if mode.is_some() {
                    return Err(LowerError::Invalid(format!(
                        "a transactor mode on data scoreboard field `{comp}.{fname}`"
                    )));
                }
                return Ok(ComponentFieldKind::ScoreboardSub { scoreboard: *sid });
            }
            // A known component type is a nested sub-component (`source :
            // AnalysisSource`).
            if let Some(cid) = ids.get(simple) {
                let mode = match mode {
                    Some(TransactorMode::Active) => Some(ComponentInstanceMode::Active),
                    Some(TransactorMode::Passive) => Some(ComponentInstanceMode::Passive),
                    None => None,
                };
                return Ok(ComponentFieldKind::Sub {
                    component: *cid,
                    mode,
                });
            }
            // A record type held BY VALUE. The standalone-transactor
            // path lowers this to `StateFieldKind::Record`, but a
            // transactor reached through an `env` comes through the
            // component-field machinery, which has no record kind — so
            // it must not fall through to the DUT-handle arm below and
            // report a second DUT handle, which is what it used to do.
            // v1 emits a plain `Beat cur;` member here and it works.
            if record_ids.contains_key(simple) {
                return Err(unsupported(
                    &format!("a record-typed field `{comp}.{fname}` of type `{simple}`"),
                    "record state lowers on a standalone `transactor`; through an \
                     `env` the component-field schema has no record kind yet",
                ));
            }
            // On a transactor, an unknown named type is the module-typed
            // DUT handle (`dut : AxiLiteRegs`) the `on` handler pokes.
            if is_transactor {
                if f.default.is_some() {
                    return Err(unsupported(
                        &format!("a default value on DUT-handle field `{comp}.{fname}`"),
                        "the DUT handle is bound by the test (`<inst>.<dut> = dut`)",
                    ));
                }
                return Ok(ComponentFieldKind::Dut {
                    dut_type: simple.to_string(),
                });
            }
            // Two very different inputs used to share this arm, and v1
            // treats them oppositely:
            //
            //   * a DECLARED type that is not a supported sub-component
            //     kind (a `covergroup`, say) — v1 emits
            //     `AnalysisCovCollector2 weird;` and wires its sampling
            //     (`env.weird.cp.b0++`). That is a working feature, so
            //     `--codegen v1` is a real escape hatch and the
            //     classification stays `Unsupported`.
            //
            //   * a name that is declared NOWHERE — v1 assumes a
            //     Verilated DUT handle and emits
            //     `VNoSuchThing* weird = nullptr;`, naming a type that
            //     does not exist. That is a typo, and a program error
            //     under every backend rather than a subset gap.
            if !declared_types.contains(simple) {
                return Err(LowerError::Invalid(format!(
                    "component `{comp}` field `{fname}` has type `{simple}`, which is not \
                     declared anywhere in the file"
                )));
            }
            Err(unsupported(
                &format!("sub-component field `{comp}.{fname}` of type `{simple}`"),
                "only env/method-scoreboard/data-scoreboard/analysis-source \
                 sub-components are lowered",
            ))
        }
    }
}

/// The output of pass-2 body lowering for one component: the lowered
/// `TbFunction`s (in FunctionId order) plus the period/max_idle clause
/// expressions that could only be lowered once a body context existed.
/// The caller patches the resolved clauses back into the schema.
pub(crate) struct ComponentBodies {
    pub funcs: Vec<TbFunction>,
    /// Resolved `period` expr for each periodic handler, in schema order.
    pub periodic_periods: Vec<crate::ir::Expr>,
    /// Resolved `trigger` expr for each cycle-trigger handler, in schema
    /// order.
    pub cycle_triggers: Vec<crate::ir::Expr>,
    /// Resolved `(period, max_idle)` watchdog clauses (`None` per clause
    /// when the source omitted it). `None` overall when no watchdog.
    pub watchdog_clauses: Option<(Option<crate::ir::Expr>, Option<crate::ir::Expr>)>,
}

/// Lower one component's method BODIES, returning the lowered
/// `TbFunction`s in declaration order (their ids match the schema's
/// `function` slots, assigned in pass 1). The body context resolves bare
/// field names self-relatively and `emit` against the component's event
/// fields.
pub(crate) fn lower_component_bodies(
    src: &CompSource<'_>,
    cid: ComponentId,
    schema: &ComponentSchema,
    ctx: &LowerCtx,
    helpers: &helpers::HelperRegistry<'_>,
    side_tables: &std::cell::RefCell<super::SideTables>,
) -> Result<ComponentBodies, LowerError> {
    let (items, when_active): (&[ComponentItem], Option<&[ComponentItem]>) = match src {
        CompSource::Env(c)
        | CompSource::Scoreboard(c)
        | CompSource::Agent(c)
        | CompSource::Sequencer(c) => (&c.items, None),
        CompSource::Transactor(t) => (&t.items, t.when_active.as_deref()),
    };
    // Pass 1 reserved FunctionIds METHODS-first, then EVENT ON-HANDLERS,
    // then PERIODIC HANDLERS, then CYCLE-TRIGGER HANDLERS, then the
    // WATCHDOG body (see `lower_component_schema`). `prog.functions` is
    // indexed by FunctionId, so the returned bodies MUST come out in that
    // same FunctionId order — NOT source-declaration order. Ordered
    // sub-passes guarantee monotonic ids.
    let mut funcs = Vec::with_capacity(
        schema.methods.len()
            + schema.on_handlers.len()
            + schema.periodic_handlers.len()
            + schema.cycle_handlers.len()
            + 1,
    );
    // The component's full field set, used to re-classify each `on`
    // handler in pass 2 exactly as pass 1 did (event-subscription vs
    // cycle-trigger), so the FunctionId blocks line up.
    let fields = &schema.fields;
    let mut method_idx = 0usize;
    for it in items.iter().chain(when_active.into_iter().flatten()) {
        if let ComponentItem::Hookable(h) = it {
            let m = &schema.methods[method_idx];
            method_idx += 1;
            funcs.push(lower_method_body(
                h,
                m.function,
                m.activation,
                cid,
                ctx,
                helpers,
                side_tables,
            )?);
        }
    }
    let mut on_idx = 0usize;
    for it in items.iter().chain(when_active.into_iter().flatten()) {
        // Only EVENT-subscription handlers live in `schema.on_handlers`;
        // periodic (`on N cycles`) handlers are a separate block.
        if let ComponentItem::OnHandler(h) = it {
            if h.periodic || !is_event_subscription(h, fields) {
                continue;
            }
            let oh = &schema.on_handlers[on_idx];
            on_idx += 1;
            funcs.push(lower_on_handler_body(
                h,
                oh,
                cid,
                ctx,
                helpers,
                side_tables,
            )?);
        }
    }
    // Periodic handlers: lower the zero-arg body AND the period expr (the
    // period reads fields self-relatively, so it lowers in the same
    // component context).
    let mut periodic_periods = Vec::with_capacity(schema.periodic_handlers.len());
    let mut per_idx = 0usize;
    for it in items.iter().chain(when_active.into_iter().flatten()) {
        if let ComponentItem::OnHandler(h) = it {
            if !h.periodic {
                continue;
            }
            let ph = &schema.periodic_handlers[per_idx];
            per_idx += 1;
            let (body, period) = lower_periodic_body(
                h,
                ph.function,
                ph.activation,
                cid,
                ctx,
                helpers,
                side_tables,
            )?;
            funcs.push(body);
            periodic_periods.push(period);
        }
    }
    // Cycle-trigger handlers: lower the zero-arg body AND the trigger
    // predicate (which reads DUT signals + component fields, so it lowers
    // in the same self-component context).
    let mut cycle_triggers = Vec::with_capacity(schema.cycle_handlers.len());
    let mut cyc_idx = 0usize;
    for it in items.iter().chain(when_active.into_iter().flatten()) {
        if let ComponentItem::OnHandler(h) = it {
            if h.periodic || is_event_subscription(h, fields) {
                continue;
            }
            let ch = &schema.cycle_handlers[cyc_idx];
            cyc_idx += 1;
            let (body, trigger) = if let Some(channel) = &ch.monitor_channel {
                lower_monitor_handshake_body(
                    h,
                    channel,
                    ch.function,
                    ch.activation,
                    cid,
                    ctx,
                    helpers,
                    side_tables,
                )?
            } else {
                lower_cycle_body(
                    h,
                    ch.function,
                    ch.activation,
                    cid,
                    ctx,
                    helpers,
                    side_tables,
                )?
            };
            funcs.push(body);
            cycle_triggers.push(trigger);
        }
    }
    // Watchdog body + clauses (at most one). A `disabled` watchdog left no
    // schema entry, so skip it here too.
    let mut watchdog_clauses = None;
    if let Some(ws) = &schema.watchdog {
        for it in items.iter().chain(when_active.into_iter().flatten()) {
            if let ComponentItem::Watchdog(w) = it {
                if w.disabled {
                    continue;
                }
                let (body, period, max_idle) = lower_watchdog_body(
                    w,
                    ws.function,
                    ws.activation,
                    cid,
                    ctx,
                    helpers,
                    side_tables,
                )?;
                funcs.push(body);
                watchdog_clauses = Some((period, max_idle));
                break;
            }
        }
    }
    Ok(ComponentBodies {
        funcs,
        periodic_periods,
        cycle_triggers,
        watchdog_clauses,
    })
}

/// Lower an `on <bool-expr> ... end on` cycle-trigger body as a zero-param
/// `ComponentMethod` (`self` only) and lower its trigger predicate in the
/// same self-component context. Returns `(body, trigger_expr)`. The
/// trigger reads DUT signals (`dut.<sig>`) and/or component fields; it is
/// rendered standalone in the per-instance `_checkers` closure.
fn lower_cycle_body(
    h: &crate::ast::OnHandler,
    fid: FunctionId,
    activation: Activation,
    cid: ComponentId,
    ctx: &LowerCtx,
    helpers: &helpers::HelperRegistry<'_>,
    side_tables: &std::cell::RefCell<super::SideTables>,
) -> Result<(TbFunction, crate::ir::Expr), LowerError> {
    let mut b = FuncBuilder::new(ctx, helpers, side_tables);
    b.self_component = Some(cid);
    b.self_component_active_only = matches!(activation, Activation::ActiveOnly);
    // `lower_expr` (NOT `_no_ports`): the trigger renders standalone in the
    // per-instance `_checkers` closure, never appended to this body, so it
    // must not hoist a port read into a body-only temp local (that local
    // would dangle in the checker). DUT port reads render inline via the
    // checker's test-scope `dut` pointer; field reads resolve self-
    // relatively and re-root at the instance path at emission.
    let trigger = b.lower_expr(&h.event)?;
    b.lower_block_stmts(&h.body)?;
    if !b.is_terminated() {
        b.terminate(Terminator::Return);
    }
    let f = b.finish(
        fid,
        format!("comp_cycle_{}", fid.0),
        FunctionKind::ComponentMethod { component: cid },
        None,
    )?;
    Ok((f, trigger))
}

/// A bound-bus placeholder `PortRef` for `bus.<ch>.<sig>` — the same flat
/// `bus_<ch>_<sig>` shape `bus::bus_port` builds, with the placeholder bus
/// prefix (`transactors::INITIATOR_BUS_PLACEHOLDER`) at `port_path[0]`.
/// `fill_initiator_bus_prefix` rewrites the prefix to the real binding name
/// at test-binding time, exactly as for the driver's `bus.<ch>.send/recv`
/// bodies and trigger.
fn monitor_bus_port(channel: &str, signal: &str) -> crate::ir::PortRef {
    crate::ir::PortRef {
        testbench_field: "dut".to_string(),
        port_path: vec![
            super::transactors::INITIATOR_BUS_PLACEHOLDER.to_string(),
            channel.to_string(),
            signal.to_string(),
        ],
        aggregate_path: false,
        direction: None,
        width: None,
        access: crate::ir::PortAccess::Port,
        lane: None,
    }
}

/// Lower an `on bus.<ch>.handshake(arg) ... end on` passive bus-monitor
/// handler (v1's `emit_bound_monitor_actors`) into a cycle-trigger body +
/// synthesized trigger. The trigger is the channel's `valid && ready`
/// (rising edge, applied by the `_checkers` closure). The body captures
/// the channel payload into the handler's `arg` local — the first payload
/// signal aliases `arg` itself (so a scalar `sb.q.push(arg)` push sees it,
/// matching v1's implicit-conversion-to-first-field), and every payload
/// signal also lands in a per-field alias recorded in `recv_payloads` (so
/// `arg.<field>` reads, e.g. `beat.data`/`beat.resp`, resolve) — then the
/// user body runs. The payload reads use the bound-bus placeholder prefix,
/// rewritten to the real binding at test-binding time.
fn lower_monitor_handshake_body(
    h: &crate::ast::OnHandler,
    channel: &str,
    fid: FunctionId,
    activation: Activation,
    cid: ComponentId,
    ctx: &LowerCtx,
    helpers: &helpers::HelperRegistry<'_>,
    side_tables: &std::cell::RefCell<super::SideTables>,
) -> Result<(TbFunction, crate::ir::Expr), LowerError> {
    // Resolve the channel's payload signals from the bound bus (visible in
    // this context under the placeholder prefix, injected at the per-
    // component body ctx). An empty payload has nothing to observe.
    let bus = ctx
        .bus_bindings
        .get(super::transactors::INITIATOR_BUS_PLACEHOLDER)
        .ok_or_else(|| {
            unsupported(
                "an `on bus.<ch>.handshake(...)` handler with no bound bus in scope",
                "",
            )
        })?;
    let hs = bus
        .handshakes
        .iter()
        .find(|hs| hs.name.name == channel)
        .ok_or_else(|| {
            LowerError::Invalid(format!(
                "bus `{}` has no handshake channel `{channel}` for `on bus.{channel}.handshake`",
                bus.name.name
            ))
        })?;
    if hs.payload.is_empty() {
        return Err(LowerError::Invalid(format!(
            "bus `{}` channel `{channel}` has no payload signals to observe",
            bus.name.name
        )));
    }
    let payload_sigs: Vec<String> = hs.payload.iter().map(|s| s.name.name.clone()).collect();

    let mut b = FuncBuilder::new(ctx, helpers, side_tables);
    b.self_component = Some(cid);
    b.self_component_active_only = matches!(activation, Activation::ActiveOnly);

    // Capture the channel payload BEFORE the user body (the payload is
    // valid in the cycle valid && ready holds). The bound `arg` local is
    // the first payload field; each remaining field gets an alias local,
    // all recorded in `recv_payloads` so `arg.<field>` reads resolve.
    let arg_name = bus_handshake_monitor_arg_name(h);
    let arg_local = b.declare(&arg_name);
    b.push(crate::ir::Stmt::DutRead(
        arg_local,
        monitor_bus_port(channel, &payload_sigs[0]),
    ));
    b.set_local_type(arg_local, IrType::UInt(None));
    let mut fields = Vec::with_capacity(payload_sigs.len());
    fields.push((payload_sigs[0].clone(), arg_local));
    for sig in &payload_sigs[1..] {
        let fl = b.declare(&format!("{arg_name}__{sig}"));
        b.push(crate::ir::Stmt::DutRead(fl, monitor_bus_port(channel, sig)));
        b.set_local_type(fl, IrType::UInt(None));
        fields.push((sig.clone(), fl));
    }
    b.recv_payloads.insert(arg_local, fields);

    b.lower_block_stmts(&h.body)?;
    if !b.is_terminated() {
        b.terminate(Terminator::Return);
    }
    let f = b.finish(
        fid,
        format!("comp_monitor_{}", fid.0),
        FunctionKind::ComponentMethod { component: cid },
        None,
    )?;

    // Trigger: `bus.<ch>.valid && bus.<ch>.ready` (rising edge applied by
    // the checker). Synthesized — the source `h.event` is the
    // `handshake(arg)` call, not a predicate.
    let trigger = crate::ir::Expr::Binary(
        crate::ir::BinOp::And,
        Box::new(crate::ir::Expr::Port(monitor_bus_port(channel, "valid"))),
        Box::new(crate::ir::Expr::Port(monitor_bus_port(channel, "ready"))),
    );
    Ok((f, trigger))
}

/// Lower an `on <N> cycles ... end on` periodic-handler body as a zero-
/// param `ComponentMethod` (`self` only) and lower its period expression
/// in the same self-component context. Returns `(body, period_expr)`.
fn lower_periodic_body(
    h: &crate::ast::OnHandler,
    fid: FunctionId,
    activation: Activation,
    cid: ComponentId,
    ctx: &LowerCtx,
    helpers: &helpers::HelperRegistry<'_>,
    side_tables: &std::cell::RefCell<super::SideTables>,
) -> Result<(TbFunction, crate::ir::Expr), LowerError> {
    let mut b = FuncBuilder::new(ctx, helpers, side_tables);
    b.self_component = Some(cid);
    b.self_component_active_only = matches!(activation, Activation::ActiveOnly);
    // The period is `h.event` in the periodic form (parser stashes the
    // cycle count there). Lower with `lower_expr` (NOT `_no_ports`): the
    // period is rendered standalone in the per-instance `_checkers`
    // closure, not appended to this body, so it must NOT hoist a port
    // read into a body-only temp local (that local would dangle in the
    // checker). A field-backed period resolves self-relatively here and
    // is re-rooted at the instance path at emission. (A DUT port in a
    // period is unusual but renders inline via `port_read` in the
    // checker's test scope.)
    let period = b.lower_expr(&h.event)?;
    b.lower_block_stmts(&h.body)?;
    if !b.is_terminated() {
        b.terminate(Terminator::Return);
    }
    let f = b.finish(
        fid,
        format!("comp_periodic_{}", fid.0),
        FunctionKind::ComponentMethod { component: cid },
        None,
    )?;
    Ok((f, period))
}

/// Lower a `watchdog ... end watchdog` body as a zero-param
/// `ComponentMethod` (`self` only) and lower its `period`/`max_idle`
/// clause expressions in the same self-component context. Returns
/// `(body, period_opt, max_idle_opt)`.
fn lower_watchdog_body(
    w: &crate::ast::WatchdogDecl,
    fid: FunctionId,
    activation: Activation,
    cid: ComponentId,
    ctx: &LowerCtx,
    helpers: &helpers::HelperRegistry<'_>,
    side_tables: &std::cell::RefCell<super::SideTables>,
) -> Result<(TbFunction, Option<crate::ir::Expr>, Option<crate::ir::Expr>), LowerError> {
    let mut b = FuncBuilder::new(ctx, helpers, side_tables);
    b.self_component = Some(cid);
    b.self_component_active_only = matches!(activation, Activation::ActiveOnly);
    // `lower_expr` (NOT `_no_ports`): period/max_idle render standalone in
    // the per-instance `_checkers` closure, never appended to this body —
    // hoisting a port read into a body-only temp would dangle in the
    // checker. Field reads resolve self-relatively and re-root at the
    // instance path at emission.
    let period = match &w.period {
        Some(e) => Some(b.lower_expr(e)?),
        None => None,
    };
    let max_idle = match &w.max_idle {
        Some(e) => Some(b.lower_expr(e)?),
        None => None,
    };
    b.lower_block_stmts(&w.body)?;
    if !b.is_terminated() {
        b.terminate(Terminator::Return);
    }
    let f = b.finish(
        fid,
        format!("comp_watchdog_{}", fid.0),
        FunctionKind::ComponentMethod { component: cid },
        None,
    )?;
    Ok((f, period, max_idle))
}

/// Lower an `on <event>(arg) ... end on` handler body as a one-param
/// `ComponentMethod` function. The single param is the event argument,
/// typed to the event payload (signed selected by the schema). Bare field
/// names resolve self-relatively; `emit`/`idle` work as in any method.
fn lower_on_handler_body(
    h: &crate::ast::OnHandler,
    oh: &crate::ir::OnHandlerSchema,
    cid: ComponentId,
    ctx: &LowerCtx,
    helpers: &helpers::HelperRegistry<'_>,
    side_tables: &std::cell::RefCell<super::SideTables>,
) -> Result<TbFunction, LowerError> {
    let mut b = FuncBuilder::new(ctx, helpers, side_tables);
    b.self_component = Some(cid);
    b.self_component_active_only = matches!(oh.activation, Activation::ActiveOnly);
    // The handler's single argument (from `on <event>(<arg>)`). The
    // param type mirrors the subscribed event's payload: a scalar
    // (signed per the schema) or a value-record (so `t.field` reads in
    // the body resolve against the record schema).
    let arg_name = on_handler_arg_name(h);
    let ty = match oh.arg_payload {
        EventPayload::Scalar { signed: true } => IrType::SInt(None),
        EventPayload::Scalar { signed: false } => IrType::UInt(None),
        EventPayload::Record(rid) => IrType::Record(rid),
    };
    let local = b.declare(&arg_name);
    b.set_local_type(local, ty.clone());
    let params = vec![TypedParam { name: arg_name, ty }];
    b.lower_block_stmts(&h.body)?;
    if !b.is_terminated() {
        b.terminate(Terminator::Return);
    }
    let mut f = b.finish(
        oh.function,
        format!("comp_on_{}", oh.function.0),
        FunctionKind::ComponentMethod { component: cid },
        None,
    )?;
    f.params = params;
    Ok(f)
}

/// The bare argument name of an `on <event>(<arg>)` handler (`_v` when
/// not a plain identifier).
fn on_handler_arg_name(h: &crate::ast::OnHandler) -> String {
    if let ExprKind::Call { args, .. } = &*h.event.kind {
        if let Some(CallArg::Expr(e)) = args.first() {
            if let ExprKind::Ident(id) = &*e.kind {
                return id.name.clone();
            }
        }
    }
    "_v".to_string()
}

/// IR type of a hookable-method parameter. Extends `helpers::ir_type_of`
/// (scalars only) with the `TSeq<Record>` form: a sequencer's
/// `hookable dispatch(txns: TSeq<RegOp>)` binds `txns` as a `RecordSeq`
/// local so `for t in txns` lowers to the counted-loop-over-sequence form
/// (same typing a `let txns = SomeTseq(...)` local would get). A
/// `TSeq<scalar>` or unresolved element falls back to `Unknown`.
fn method_param_ir_type(ty: Option<&TypeExpr>, ctx: &LowerCtx) -> IrType {
    if let Some(TypeExpr::Builtin {
        name: BuiltinTy::TSeq,
        args,
        ..
    }) = ty
    {
        // The element type-arg of `TSeq<RegOp>`. A bare `RegOp` identifier
        // parses as `TypeArg::Expr(Ident)` (the parser only treats builtin
        // keywords as `TypeArg::Type`); a `TypeArg::Type(Named)` also
        // occurs via the fragment parser. Resolve both shapes.
        let elem_name: Option<&str> = match args.first() {
            Some(TypeArg::Type(TypeExpr::Named { name, .. })) => {
                name.segments.last().map(|s| s.name.as_str())
            }
            Some(TypeArg::Expr(e)) => match &*e.kind {
                ExprKind::Ident(id) => Some(id.name.as_str()),
                _ => None,
            },
            _ => None,
        };
        if let Some(simple) = elem_name {
            if let Some(rid) = ctx.record_ids.get(simple) {
                return IrType::RecordSeq(*rid);
            }
        }
        // A scalar element (`TSeq<uint<N>>`/`<sint<N>>`/`<bool>`) parses as
        // `TypeArg::Type(Builtin)` — not a record name — so type the param as
        // `Seq(scalar)`, mirroring how `collect_tseq_records` types a
        // scalar-element tseq result (#453). v1 renders both `RecordSeq` and
        // `Seq` as `std::vector<T>`; only the element C++ type differs.
        if let Some(TypeArg::Type(inner)) = args.first() {
            match helpers::ir_type_of(Some(inner)) {
                ty @ (IrType::UInt(_) | IrType::SInt(_) | IrType::Bool) => {
                    return IrType::Seq(Box::new(ty));
                }
                _ => {}
            }
        }
        return IrType::Unknown;
    }
    // A record-typed component/scoreboard method parameter
    // (`observe(cmd: Cmd)`) is a by-value transaction/struct, not a
    // component/module handle. Resolve it before the component-typed
    // parameter path so the method body can read `cmd.field`.
    if let Some(TypeExpr::Named { name, .. }) = ty {
        let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
        if let Some(rid) = ctx.record_ids.get(simple) {
            return IrType::Record(*rid);
        }
    }
    // A component-typed parameter (`observe(addr, model: ProtocolModel)`):
    // resolve the component name against the program's component table so
    // method calls on it dispatch through `ComponentBase::Local`.
    if let Some(TypeExpr::Named { name, .. }) = ty {
        let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
        if let Some(rid) = ctx.record_ids.get(simple) {
            return IrType::Record(*rid);
        }
        if let Some(cid) = ctx
            .components
            .iter()
            .position(|c| c.name == simple)
            .map(|i| ComponentId(i as u32))
        {
            return IrType::Component(cid);
        }
    }
    helpers::ir_type_of(ty)
}

fn method_schema_ir_type(
    ty: Option<&TypeExpr>,
    ids: &HashMap<String, ComponentId>,
    record_ids: &HashMap<String, RecordId>,
) -> IrType {
    if let Some(TypeExpr::Builtin {
        name: BuiltinTy::TSeq,
        args,
        ..
    }) = ty
    {
        let elem_name: Option<&str> = match args.first() {
            Some(TypeArg::Type(TypeExpr::Named { name, .. })) => {
                name.segments.last().map(|s| s.name.as_str())
            }
            Some(TypeArg::Expr(e)) => match &*e.kind {
                ExprKind::Ident(id) => Some(id.name.as_str()),
                _ => None,
            },
            _ => None,
        };
        if let Some(simple) = elem_name {
            if let Some(rid) = record_ids.get(simple) {
                return IrType::RecordSeq(*rid);
            }
        }
        if let Some(TypeArg::Type(inner)) = args.first() {
            match helpers::ir_type_of(Some(inner)) {
                ty @ (IrType::UInt(_) | IrType::SInt(_) | IrType::Bool) => {
                    return IrType::Seq(Box::new(ty));
                }
                _ => {}
            }
        }
        return IrType::Unknown;
    }
    if let Some(TypeExpr::Named { name, .. }) = ty {
        let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
        if let Some(rid) = record_ids.get(simple) {
            return IrType::Record(*rid);
        }
        if let Some(cid) = ids.get(simple) {
            return IrType::Component(*cid);
        }
    }
    helpers::ir_type_of(ty)
}

fn lower_method_body(
    h: &HookableMethod,
    fid: FunctionId,
    activation: Activation,
    cid: ComponentId,
    ctx: &LowerCtx,
    helpers: &helpers::HelperRegistry<'_>,
    side_tables: &std::cell::RefCell<super::SideTables>,
) -> Result<TbFunction, LowerError> {
    let mut b = FuncBuilder::new(ctx, helpers, side_tables);
    b.self_component = Some(cid);
    b.self_component_active_only = matches!(activation, Activation::ActiveOnly);
    // Bind parameters as the first locals (the run/check convention: a
    // LocalId < params.len() *is* the i-th param). Same shape as a
    // transactor method body.
    let mut params = Vec::with_capacity(h.params.len());
    for p in &h.params {
        let ty = method_param_ir_type(p.ty.as_ref(), ctx);
        let local = b.declare(&p.name.name);
        b.set_local_type(local, ty.clone());
        if let Some(t) = &p.ty {
            if let Some(w) = scalar_width(t) {
                b.let_widths.insert(local, w);
            }
        }
        params.push(TypedParam {
            name: p.name.name.clone(),
            ty,
        });
    }
    if let Some(rt) = &h.return_ty {
        let ret = b.declare("__ret");
        // A record-returning method (`-> ReadResponse`) types its `__ret`
        // slot as the record so codegen declares it as the struct (and the
        // lambda's return type resolves to the record name).
        if let TypeExpr::Named { name, .. } = rt {
            let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
            if let Some(&rid) = ctx.record_ids.get(simple) {
                b.set_local_type(ret, IrType::Record(rid));
            }
        }
        b.helper_ret = Some(ret);
    }
    b.lower_block_stmts(&h.body)?;
    if !b.is_terminated() {
        b.terminate(Terminator::Return);
    }
    let mut f = b.finish(
        fid,
        format!("comp_method_{}", fid.0),
        FunctionKind::ComponentMethod { component: cid },
        None,
    )?;
    f.params = params;
    Ok(f)
}

/// Resolve an env's `connect` block into `ConnectEdgeSchema`s. The env
/// is `env_schema` (id `env_cid`); the test reaches it through the
/// test-field local. `components` is the program snapshot for resolving
/// sink sub-component types. Paths are relative to the env field (the
/// first segment is the env local; here we record the sub-component path
/// from the env down, e.g. `["source"]`).
pub(crate) fn resolve_connects(
    env: &ComponentDecl,
    env_schema: &ComponentSchema,
    components: &[ComponentSchema],
) -> Result<Vec<ConnectEdgeSchema>, LowerError> {
    let mut edges = Vec::new();
    for it in &env.items {
        let ComponentItem::Connect(block) = it else {
            continue;
        };
        for e in &block.edges {
            edges.push(resolve_one_connect(env, components, e, |path| {
                resolve_sub_path(env_schema, components, path)
            })?);
        }
    }
    Ok(edges)
}

/// Resolve `connect` blocks owned directly by a reusable testbench. Their
/// endpoint paths remain rooted at testbench component fields for emission.
pub(crate) fn resolve_testbench_connects(
    tb: &ComponentDecl,
    roots: &HashMap<String, ComponentId>,
    components: &[ComponentSchema],
) -> Result<Vec<ConnectEdgeSchema>, LowerError> {
    let mut edges = Vec::new();
    for it in &tb.items {
        let ComponentItem::Connect(block) = it else {
            continue;
        };
        for e in &block.edges {
            edges.push(resolve_one_connect(tb, components, e, |path| {
                resolve_testbench_path(roots, components, path)
            })?);
        }
    }
    Ok(edges)
}

fn resolve_one_connect<F>(
    owner: &ComponentDecl,
    components: &[ComponentSchema],
    edge: &ConnectEdge,
    resolve_path: F,
) -> Result<ConnectEdgeSchema, LowerError>
where
    F: Fn(&[String]) -> Result<ComponentId, LowerError>,
{
    // `source.observed -> sb.write_obs`: both sides are dotted paths
    // rooted at an env sub-component field.
    // NOT reclassified, and the reason is `connect`'s standing one: what
    // v1 does with a bad edge depends on WHERE THE EDGE SITS. In an
    // instantiated env v1 reaches its own endpoint check and refuses;
    // in an UNINSTANTIATED one it emits no wiring at all and succeeds,
    // so the same malformed edge is invisible there. tbir resolves
    // `connect` for every env in the merged file, so it sees edges v1
    // never reaches. No single `V1Status` is honest, so the suggestion
    // stays, being true somewhere. The regression test is
    // `a_malformed_connect_endpoint_keeps_its_v1_suggestion`.
    let from = dotted_path(&edge.from).ok_or_else(|| {
        unsupported(
            &format!("a non-path `connect` source in `{}`", owner.name.name),
            "",
        )
    })?;
    let to = dotted_path(&edge.to).ok_or_else(|| {
        unsupported(
            &format!("a non-path `connect` sink in `{}`", owner.name.name),
            "",
        )
    })?;
    // The source path is `<subcomp>.<event>` (final segment is the event
    // port on the source sub-component).
    if from.len() < 2 {
        // NOT reclassified, and the reason is worth keeping: what v1
        // does with a malformed `connect` edge depends on where the edge
        // SITS, not on how it is malformed. In an INSTANTIATED env v1
        // emits the path verbatim and the result usually does not
        // compile — but a single-segment endpoint resolves against the
        // owner's own hookable / `out event` and works
        // (`E_take(_tb.top, _t)`), and an UNINSTANTIATED env emits no
        // wiring at all, so every malformed edge in one is invisible and
        // v1 simply succeeds. tbir resolves `connect` for every env in
        // the merged file, so it sees edges v1 never reaches. One site,
        // three outcomes — the `--codegen v1` suggestion stays, because
        // it is true somewhere.
        return Err(unsupported(
            &format!(
                "a `connect` source `{}` without an event field",
                from.join(".")
            ),
            "",
        ));
    }
    let (src_path, src_event) = from.split_at(from.len() - 1);
    let src_event = src_event[0].clone();
    // The sink path is `<subcomp>.<method>` (final segment is the
    // hookable method on the sink sub-component).
    if to.len() < 2 {
        return Err(unsupported(
            &format!("a `connect` sink `{}` without a method", to.join(".")),
            "",
        ));
    }
    let (sink_path, sink_name) = to.split_at(to.len() - 1);
    let sink_name = sink_name[0].clone();

    // Resolve the source sub-component and verify it exposes `src_event`.
    let src_cid = resolve_path(src_path)?;
    let src_comp = &components[src_cid.index()];
    let (src_payload, src_activation) = match src_comp.field(&src_event) {
        Some(ComponentFieldSchema {
            kind: ComponentFieldKind::Event { payload }, activation,
            ..
        }) => (*payload, *activation),
        _ => {
            return Err(unsupported(
                &format!(
                    "a `connect` source `{}.{src_event}` that is not an `out event` port",
                    src_path.join(".")
                ),
                "",
            ));
        }
    };

    // Resolve the sink sub-component. The final segment is either a
    // hookable sink method (`sb.write_obs`) or an `in event<T>` field on
    // an event-driven transactor (`drv.req`); pick the matching sink shape.
    let sink_cid = resolve_path(sink_path)?;
    let sink_comp = &components[sink_cid.index()];
    let (sink, sink_activation) = if let Some(sm) = sink_comp.method(&sink_name) {
        if !sm.hookable {
            return Err(unsupported(
                &format!(
                    "a `connect` sink method `{}.{sink_name}` that is not hookable",
                    sink_path.join(".")
                ),
                "analysis sinks must be declared `hookable`",
            ));
        }
        if sm.n_params != 1 {
            return Err(unsupported(
                &format!(
                    "a `connect` sink method `{sink_name}` with {} parameters",
                    sm.n_params
                ),
                "analysis sinks take exactly one payload parameter",
            ));
        }
        if sm.has_ret {
            return Err(unsupported(
                &format!(
                    "a `connect` sink method `{}.{sink_name}` that returns a value",
                    sink_path.join(".")
                ),
                "analysis sinks must not return a value",
            ));
        }
        if !event_payload_matches_ir_type(src_payload, &sm.param_tys[0]) {
            return Err(unsupported(
                &format!(
                    "a `connect` payload mismatch from `{}.{src_event}` to `{}.{sink_name}`",
                    src_path.join("."),
                    sink_path.join("."),
                ),
                "source and sink payloads must have the same signed scalar shape or record type",
            ));
        }
        (
            crate::ir::ConnectSink::Method { method: sink_name },
            sm.activation,
        )
    } else if let Some(ComponentFieldSchema {
        kind: ComponentFieldKind::Event { payload }, activation,
        ..
    }) = sink_comp.field(&sink_name)
    {
        if *payload != src_payload {
            return Err(unsupported(
                &format!(
                    "a `connect` payload mismatch from `{}.{src_event}` to `{}.{sink_name}`",
                    src_path.join("."),
                    sink_path.join("."),
                ),
                "source and sink event payloads must have the same signed scalar shape or record type",
            ));
        }
        (
            crate::ir::ConnectSink::Event { event: sink_name },
            *activation,
        )
    } else {
        return Err(unsupported(
            &format!(
                "a `connect` sink `{}.{sink_name}` that is neither a sink method nor an \
                 `event` field",
                sink_path.join(".")
            ),
            "",
        ));
    };

    Ok(ConnectEdgeSchema {
        src_path: src_path.to_vec(),
        src_event,
        src_activation,
        sink_path: sink_path.to_vec(),
        sink_component: sink_cid,
        sink,
        sink_activation,
    })
}

fn resolve_testbench_path(
    roots: &HashMap<String, ComponentId>,
    components: &[ComponentSchema],
    path: &[String],
) -> Result<ComponentId, LowerError> {
    let Some((root, tail)) = path.split_first() else {
        return Err(unsupported(
            "an empty testbench `connect` component path",
            "",
        ));
    };
    let cid = roots.get(root).copied().ok_or_else(|| {
        unsupported(
            &format!("a testbench `connect` root `{root}` that is not a component field"),
            "connect endpoints must be rooted at a testbench-owned component field",
        )
    })?;
    if tail.is_empty() {
        Ok(cid)
    } else {
        resolve_sub_path(&components[cid.index()], components, tail)
    }
}

/// Whether a hookable method's declared parameter has the same runtime
/// callback shape as an analysis event payload. Narrow unsigned values,
/// `bits`, and `bool` all widen to the unsigned callback representation;
/// signed values and value records retain distinct shapes.
fn event_payload_matches_ir_type(payload: EventPayload, ty: &IrType) -> bool {
    match (payload, ty) {
        (_, IrType::Unknown) => true,
        (EventPayload::Scalar { signed: true }, IrType::SInt(_)) => true,
        (EventPayload::Scalar { signed: false }, IrType::UInt(_) | IrType::Bool) => true,
        (EventPayload::Record(source), IrType::Record(sink)) => source == *sink,
        _ => false,
    }
}

/// Walk a sub-component path (relative to the env) to the ComponentId it
/// names. `["source"]` → the env field `source`'s component.
fn resolve_sub_path(
    env_schema: &ComponentSchema,
    components: &[ComponentSchema],
    path: &[String],
) -> Result<ComponentId, LowerError> {
    let mut cur = env_schema;
    let mut cid = None;
    for seg in path {
        let f = cur.field(seg).ok_or_else(|| {
            // Same position-dependence as the endpoint arms above: v1
            // prints the path verbatim in an instantiated env (a member
            // access that does not compile) and emits nothing at all in
            // an uninstantiated one.
            unsupported(
                &format!("a `connect` path segment `{seg}` that is not a sub-component field"),
                "",
            )
        })?;
        match &f.kind {
            ComponentFieldKind::Sub { component, .. } => {
                cid = Some(*component);
                cur = &components[component.index()];
            }
            _ => {
                return Err(unsupported(
                    &format!("a `connect` path segment `{seg}` that is not a sub-component"),
                    "",
                ));
            }
        }
    }
    cid.ok_or_else(|| unsupported("an empty `connect` sub-component path", ""))
}

/// Flatten `a.b.c` (Field nodes over an Ident root) into a dotted path,
/// or `None` for anything else.
pub(crate) fn dotted_path(e: &crate::ast::Expr) -> Option<Vec<String>> {
    match &*e.kind {
        ExprKind::Ident(id) => Some(vec![id.name.clone()]),
        ExprKind::Field { target, name } => {
            let mut p = dotted_path(target)?;
            p.push(name.name.clone());
            Some(p)
        }
        ExprKind::Paren(inner) => dotted_path(inner),
        _ => None,
    }
}

/// Resolve the `<T>` inside a `queue<T>` component-field element to its
/// `QueueElem`. Mirrors `lower_event_payload`:
///   * a scalar (`uint<W>`/`sint<W>`/`bool` ≤ 64 bits) → `Scalar`;
///   * a user-named `transaction`/`struct` → `Record` (carried by value).
///
/// A scalar element parses as `TypeArg::Type`; a user-named record element
/// parses as a bare identifier (`TypeArg::Expr(Ident)`) or, in some
/// positions, `TypeArg::Type(Named)`. A named type that is neither a
/// scalar nor a known record (enum / Vec / nested / unknown) is rejected
/// precisely.
pub(crate) fn lower_queue_elem(
    comp: &str,
    fname: &str,
    arg: Option<&TypeArg>,
    record_ids: &HashMap<String, RecordId>,
) -> Result<crate::ir::QueueElem, LowerError> {
    use crate::ir::QueueElem;
    let reject_named = |named: &str| -> LowerError {
        unsupported(
            &format!("a non-scalar queue element `{named}` on `{comp}.{fname}`"),
            "only `queue<scalar ≤ 64 bits>` and `queue<transaction|struct>` elements \
             are lowered; enum/Vec/nested elements gate on a later slice",
        )
    };
    match arg {
        // Scalar element (`queue<uint<32>>`), or a single-segment named
        // record the type-arg layer parsed as a Type.
        Some(TypeArg::Type(ty)) => {
            if let Some(name) = type_arg_simple_name(ty) {
                if let Some(rid) = record_ids.get(name) {
                    return Ok(QueueElem::Record(*rid));
                }
            }
            match scalar_ir_type(ty) {
                Some(IrType::SInt(_)) => Ok(QueueElem::Scalar { signed: true }),
                Some(IrType::UInt(_)) | Some(IrType::Bool) => {
                    Ok(QueueElem::Scalar { signed: false })
                }
                _ => Err(reject_named(type_arg_simple_name(ty).unwrap_or("<expr>"))),
            }
        }
        // `queue<CheckerError>` parses the element as a bare identifier.
        Some(TypeArg::Expr(e)) => {
            if let ExprKind::Ident(id) = &*e.kind {
                if let Some(rid) = record_ids.get(&id.name) {
                    return Ok(QueueElem::Record(*rid));
                }
                return Err(reject_named(&id.name));
            }
            Err(unsupported(
                &format!("a non-identifier queue element on `{comp}.{fname}`"),
                "only `queue<scalar ≤ 64 bits>` and `queue<transaction|struct>` elements \
                 are lowered",
            ))
        }
        Some(TypeArg::Named { name, .. }) => Err(reject_named(&name.name)),
        None => Err(unsupported(
            &format!("a `queue` with no element type on `{comp}.{fname}`"),
            "declare the element type: `queue<uint<W>>` / `queue<Record>`",
        )),
    }
}

/// Resolve the `<T>` inside an `event<T>` analysis-port field to its
/// `EventPayload`. Mirrors v1's `payload_type_for_arg` for the lowered
/// subset:
///   * a scalar (`uint<W>`/`sint<W>`/`bool` ≤ 64 bits) → `Scalar`;
///   * a user-named `transaction`/`struct` → `Record` (carried by value
///     as the record struct, matching v1's `std::function<void(Txn)>`).
///
/// A scalar payload parses as `TypeArg::Type`; a user-named record
/// payload parses as a bare identifier — `TypeArg::Expr(Ident)` (the
/// common case) or `TypeArg::Named`. A named type that is neither a
/// scalar nor a known record (enum / Vec / nested / unknown) is rejected
/// precisely — those genuinely unsupported payload shapes gate on later
/// slices.
pub(crate) fn lower_event_payload(
    comp: &str,
    fname: &str,
    arg: Option<&TypeArg>,
    record_ids: &HashMap<String, RecordId>,
) -> Result<EventPayload, LowerError> {
    let reject_named = |named: &str| -> LowerError {
        unsupported(
            &format!("a non-record event payload `{named}` on `{comp}.{fname}`"),
            "only event<scalar ≤ 64 bits> and event<transaction|struct> payloads \
             are lowered; enum/Vec/nested payloads gate on a later slice",
        )
    };
    match arg {
        // Scalar payload (`event<uint<8>>`) or a single-segment named
        // type that the type-arg layer happened to parse as a Type.
        Some(TypeArg::Type(ty)) => {
            if let Some(name) = type_arg_simple_name(ty) {
                if let Some(rid) = record_ids.get(name) {
                    return Ok(EventPayload::Record(*rid));
                }
            }
            match scalar_ir_type(ty) {
                Some(IrType::SInt(_)) => Ok(EventPayload::Scalar { signed: true }),
                Some(IrType::UInt(_)) | Some(IrType::Bool) => {
                    Ok(EventPayload::Scalar { signed: false })
                }
                _ => Err(reject_named(type_arg_simple_name(ty).unwrap_or("<expr>"))),
            }
        }
        // `event<TinyTxn>` parses the payload as a bare identifier.
        Some(TypeArg::Expr(e)) => {
            if let ExprKind::Ident(id) = &*e.kind {
                if let Some(rid) = record_ids.get(&id.name) {
                    return Ok(EventPayload::Record(*rid));
                }
                return Err(reject_named(&id.name));
            }
            Err(unsupported(
                &format!("a non-identifier event payload on `{comp}.{fname}`"),
                "only event<scalar ≤ 64 bits> and event<transaction|struct> payloads are lowered",
            ))
        }
        // `TypeArg::Named` is a keyword-style arg (`depth=16`), never a
        // payload type reference — reject it precisely.
        Some(TypeArg::Named { name, .. }) => Err(reject_named(&name.name)),
        // A bare `event` with no payload defaults to an unsigned scalar
        // (matches the prior behavior).
        None => Ok(EventPayload::Scalar { signed: false }),
    }
}

/// The simple (last-segment) name of a bare named `TypeExpr`
/// (`TinyTxn`), or `None` for a builtin — used to spot a record payload
/// that parsed as `TypeArg::Type(Named)`.
fn type_arg_simple_name(t: &TypeExpr) -> Option<&str> {
    match t {
        TypeExpr::Named { name, .. } => name.segments.last().map(|s| s.name.as_str()),
        _ => None,
    }
}

// --- shared scalar-field helpers (mirroring scoreboards.rs) ---

fn scalar_ir_type(t: &TypeExpr) -> Option<IrType> {
    let TypeExpr::Builtin { name, args, .. } = t else {
        return None;
    };
    let width = match args.first() {
        Some(TypeArg::Expr(e)) => match &*e.kind {
            ExprKind::Int(s) => Some(s.replace('_', "").parse::<u32>().ok()?),
            _ => return None,
        },
        Some(_) => return None,
        None => None,
    };
    if width.is_some_and(|w| w == 0 || w > 64) {
        return None;
    }
    match name {
        BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits | BuiltinTy::Int => {
            Some(IrType::UInt(width))
        }
        BuiltinTy::SInt | BuiltinTy::SIntCap => Some(IrType::SInt(width)),
        BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => Some(IrType::Bool),
        _ => None,
    }
}

fn scalar_width(t: &TypeExpr) -> Option<u32> {
    match scalar_ir_type(t) {
        Some(IrType::UInt(Some(w))) | Some(IrType::SInt(Some(w))) => Some(w),
        Some(IrType::Bool) => Some(1),
        _ => None,
    }
}

/// A component field's `default`, folded to the value the struct member
/// is initialized with.
///
/// v1 emits the default's SOURCE TEXT into the C++ member initializer,
/// which works for a literal (`= 7`) and for a `const` name (`= K`,
/// since the const is emitted as a C++ constant) but silently degrades
/// to `= 0` for anything else — a `default 1 + 1` field starts at 0
/// there, not 2. Folding through the file's constant table covers both
/// working shapes and every other constant expression besides, so the
/// only remaining rejection is a default that is not constant at all.
fn scalar_default(
    default: &Option<crate::ast::Expr>,
    comp: &str,
    fname: &str,
    ty: &TypeExpr,
    consts: &HashMap<String, super::ConstVal>,
) -> Result<u64, LowerError> {
    let Some(d) = default else { return Ok(0) };
    fold_field_default(d, Some(ty), consts, &format!("component field `{comp}.{fname}`"))
}

/// Shared by the component, scoreboard and transactor-state field
/// paths: fold a `default` to the value its member is initialized with.
///
/// `ty` is the field's declared type, run through the SAME
/// `check_const_decl_type` a `const` declaration gets — so a negative
/// value in an unsigned field, or one wider than the field, is rejected
/// here rather than emitted as a 64-bit bit pattern. `what` names the
/// field for the diagnostic.
///
/// The three error classes stay distinct: a non-constant expression is
/// a `NotImplemented` (v1 accepts it and silently emits `= 0`), while an
/// illegal evaluation — division by zero, an out-of-range shift, a value
/// that does not fit the field — is a `LowerError::Invalid`, matching
/// what a `const` declaration reports for the same expression.
pub(crate) fn fold_field_default(
    d: &crate::ast::Expr,
    ty: Option<&crate::ast::TypeExpr>,
    consts: &HashMap<String, super::ConstVal>,
    what: &str,
) -> Result<u64, LowerError> {
    // Fast path: a plain literal or bool needs no constant table, and is
    // what almost every `default` actually is.
    let folded = match &*d.kind {
        ExprKind::Int(lit) => super::exprs::parse_int_literal(lit).map(|bits| super::ConstVal {
            bits,
            signed: bits <= i64::MAX as u64,
        }),
        ExprKind::Bool(b) => Some(super::ConstVal {
            bits: *b as u64,
            signed: true,
        }),
        _ => None,
    };
    // `""` as the self-name: a field default has no enclosing `const` to
    // form a cycle with, so no identifier can be a self-reference.
    let v = match folded {
        Some(v) => v,
        None => super::fold_const(d, consts, "").map_err(|e| match e {
            super::ConstFoldErr::Unsupported(detail) => not_implemented(
                &format!("a non-constant default on {what}"),
                detail,
                V1Status::SilentlyMisLowers,
            ),
            super::ConstFoldErr::Invalid(detail) => {
                LowerError::Invalid(format!("the default on {what}: {detail}"))
            }
        })?,
    };
    super::check_const_decl_type(ty, v)
        .map(|v| v.bits)
        .map_err(|detail| LowerError::Invalid(format!("the default on {what}: {detail}")))
}

// --- access resolution on FuncBuilder (test-body + method-body) ---

use crate::ast::{CallArg, Expr as AstExpr};
use crate::ir::{ComponentBase, Expr as IrExpr, Stmt as IrStmt};

impl super::FuncBuilder<'_> {
    /// Normalize a component-access dotted path to the form the component
    /// machinery resolves: rooted at the BARE component-field name.
    ///
    /// Two binding shapes reach component access, and the impl-for
    /// desugaring rewrites them differently:
    ///   * test-scope `let env : <Env>` — left as the bare name `env`,
    ///     so `env.source.publish` arrives unchanged.
    ///   * `testbench` FIELD `prod : Producer` — the desugaring prepends
    ///     the `_tb` prefix, so `prod.in_ev` arrives as `_tb.prod.in_ev`.
    /// Both must resolve to the same `component_fields` entry (keyed by
    /// the bare field name) and to a `ComponentBase::Path` rooted at the
    /// bare name — tbir emits every component as a run-scope instance
    /// regardless of its binding shape. Strip a leading `_tb` segment
    /// (only when it matches `ctx.tb_field` AND a real component field
    /// follows) so a user component literally named `_tb` is untouched.
    pub(crate) fn strip_tb_prefix<'p>(&self, path: &'p [String]) -> &'p [String] {
        if path.len() >= 2
            && Some(path[0].as_str()) == self.ctx.tb_field.as_deref()
            && self.ctx.component_fields.contains_key(&path[1])
        {
            &path[1..]
        } else {
            path
        }
    }

    /// Resolve a component-method call callee: `env.source.publish` (a
    /// dotted path rooted at a test-scope component local) or a bare
    /// `publish` self-call inside a method body. Returns
    /// `(base, component, method)` when the receiver resolves to a
    /// component and `method` is one of its methods; `None` otherwise.
    /// An error is reserved for a malformed path that clearly INTENDED a
    /// component (head segment is a component local) but does not resolve.
    pub(crate) fn as_component_method_call(
        &self,
        callee: &AstExpr,
    ) -> Result<Option<(ComponentBase, ComponentId, String)>, LowerError> {
        // Path form: `<path...>.<method>`.
        if let Some(path) = dotted_path(callee) {
            let path = self.strip_tb_prefix(&path);
            if path.len() >= 2 {
                if let Some(&head_cid) = self.ctx.component_fields.get(&path[0]) {
                    let (recv, method) = path.split_at(path.len() - 1);
                    let method = method[0].clone();
                    // A path through a data-only scoreboard sub-component
                    // (`top.sb.expected.push(...)`) is a scoreboard op, not
                    // a component method call — let the scoreboard handlers
                    // claim it instead of erroring in resolve_component_recv.
                    if self.recv_is_scoreboard_sub(head_cid, &recv[1..]) {
                        return Ok(None);
                    }
                    let cid = self.resolve_component_recv(head_cid, &recv[1..])?;
                    let comp = &self.ctx.components[cid.index()];
                    let Some(method_schema) = comp.method(&method) else {
                        return Err(unsupported(
                            &format!(
                                "component `{}` has no method `{method}` (in `{}`)",
                                comp.name,
                                path.join(".")
                            ),
                            "",
                        ));
                    };
                    self.require_component_activation(
                        &path[0],
                        head_cid,
                        &recv[1..],
                        method_schema.activation,
                        "method",
                        &method,
                    )?;
                    return Ok(Some((ComponentBase::Path(recv.to_vec()), cid, method)));
                }
            }
        }
        // Self-call form: bare `publish` inside a method body.
        if let ExprKind::Ident(id) = &*callee.kind {
            if let Some(cid) = self.self_component {
                let comp = &self.ctx.components[cid.index()];
                if let Some(method) = comp.method(&id.name) {
                    self.require_self_activation(method.activation, "method", &id.name)?;
                    return Ok(Some((ComponentBase::SelfField, cid, id.name.clone())));
                }
            }
        }
        // Component-typed parameter call: `model.predict_read(addr)` inside
        // a method body, where `model` is a method PARAM local of type
        // `IrType::Component(cid)`. Dispatch through `ComponentBase::Local`
        // (the receiver is the local itself, passed by value). Checked
        // before the self sub-component arm — a param is a real local, so
        // that arm's `lookup(...).is_none()` guard would skip it.
        if let ExprKind::Field {
            target,
            name: method,
        } = &*callee.kind
        {
            if let ExprKind::Ident(recv) = &*target.kind {
                if let Some(local) = self.lookup(&recv.name) {
                    if let Some(cid) = self.component_of_local(local) {
                        let comp = &self.ctx.components[cid.index()];
                        if comp.method(&method.name).is_none() {
                            return Err(unsupported(
                                &format!(
                                    "component `{}` has no method `{}` (on parameter `{}`)",
                                    comp.name, method.name, recv.name
                                ),
                                "",
                            ));
                        }
                        return Ok(Some((
                            ComponentBase::Local(local),
                            cid,
                            method.name.clone(),
                        )));
                    }
                }
            }
        }
        // Self-relative sub-component call: `sb.record_error(...)` inside a
        // transactor/component body, where `sb` is a self sub-component
        // field (a `Sub` to a method-bearing component). The receiver is a
        // `self`-rooted path the emitter re-roots at the running instance.
        if let ExprKind::Field {
            target,
            name: method,
        } = &*callee.kind
        {
            if let ExprKind::Ident(sub) = &*target.kind {
                if self.lookup(&sub.name).is_none() {
                    if let Some(self_cid) = self.self_component {
                        let comp = &self.ctx.components[self_cid.index()];
                        if let Some(ComponentFieldKind::Sub { component, .. }) =
                            comp.field(&sub.name).map(|f| &f.kind)
                        {
                            let sub_comp = &self.ctx.components[component.index()];
                            if sub_comp.method(&method.name).is_some() {
                                return Ok(Some((
                                    ComponentBase::Path(vec!["self".to_string(), sub.name.clone()]),
                                    *component,
                                    method.name.clone(),
                                )));
                            }
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    /// Resolve a `<recv>.<queue>.<method>(...)` access on a composite-
    /// component `queue<T>` field. Two receiver shapes (mirroring the
    /// scalar-field resolvers):
    ///   * self-relative `errors.push(e)` inside a method body — `callee`
    ///     is `Field { Ident(queue), method }` and `queue` names a queue
    ///     field of `self_component`;
    ///   * test-scope path `checker.sb.errors.pop()` — `callee` is
    ///     `Field { <path>.<queue>, method }`, the head names a test-scope
    ///     component local, and the resolved sub-component has a queue
    ///     field `queue`.
    /// Returns `(base, queue_field, method)` when it resolves; `None` when
    /// the access is not a component-queue call (a different resolver may
    /// claim it). A path that reaches a component but whose terminal field
    /// is not a queue returns `None` too (so a method/scalar resolver can
    /// claim it). A data-only scoreboard sub receiver is left for the
    /// scoreboard handlers (`recv_is_scoreboard_sub`).
    pub(crate) fn as_component_queue_call(
        &self,
        callee: &AstExpr,
    ) -> Result<Option<(ComponentBase, String, String)>, LowerError> {
        // callee = Field { target = <recv>.<queue>, name = method }. For
        // `errors.push(e)` the target is `Ident(errors)`; for
        // `checker.sb.errors.push(e)` it is the dotted `checker.sb.errors`.
        let ExprKind::Field {
            target,
            name: method,
        } = &*callee.kind
        else {
            return Ok(None);
        };
        // Self-relative bare queue: `errors.push(e)` — target is an `Ident`
        // naming a self queue field (not a shadowing local).
        if let ExprKind::Ident(qid) = &*target.kind {
            if self.lookup(&qid.name).is_none() {
                if let Some(cid) = self.self_component {
                    let comp = &self.ctx.components[cid.index()];
                    if let Some(field) = comp.field(&qid.name) {
                        if matches!(field.kind, ComponentFieldKind::Queue { .. }) {
                            self.require_self_activation(field.activation, "queue", &qid.name)?;
                            return Ok(Some((
                                ComponentBase::SelfField,
                                qid.name.clone(),
                                method.name.clone(),
                            )));
                        }
                    }
                }
            }
            return Ok(None);
        }
        // Path form: `<head>.<subs...>.<queue>.<method>`.
        let Some(path) = dotted_path(target) else {
            return Ok(None);
        };
        let path = self.strip_tb_prefix(&path);
        if path.len() < 2 {
            return Ok(None);
        }
        let Some(&head_cid) = self.ctx.component_fields.get(&path[0]) else {
            return Ok(None);
        };
        let (recv, queue) = path.split_at(path.len() - 1);
        let queue = queue[0].clone();
        // A receiver ending in a data-only scoreboard sub is a scoreboard
        // queue op, not a component-queue op — let the scoreboard handlers
        // claim it.
        if self.recv_is_scoreboard_sub(head_cid, &recv[1..]) {
            return Ok(None);
        }
        let cid = self.resolve_component_recv(head_cid, &recv[1..])?;
        let comp = &self.ctx.components[cid.index()];
        let Some(field) = comp.field(&queue) else {
            return Ok(None);
        };
        if !matches!(field.kind, ComponentFieldKind::Queue { .. }) {
            return Ok(None);
        }
        self.require_component_activation(
            &recv[0],
            head_cid,
            &recv[1..],
            field.activation,
            "queue",
            &queue,
        )?;
        Ok(Some((
            ComponentBase::Path(recv.to_vec()),
            queue,
            method.name.clone(),
        )))
    }

    /// Lower a whole-component value copy of a test-scope sub-component:
    /// `checker.sb = sb` / `responder.model = model`. Returns `true` when
    /// consumed. The LHS is `<dst-path>.<sub-field>` where the terminal
    /// field is a `Sub` component field; the RHS is a single-segment
    /// test-scope component local (or `_tb`-prefixed) of the SAME component
    /// type. Anything else returns `false` (a later resolver / the
    /// scalar-field rejection claims it).
    pub(crate) fn lower_component_sub_assign(
        &mut self,
        target: &AstExpr,
        value: &AstExpr,
    ) -> Result<bool, LowerError> {
        let Some(path) = dotted_path(target) else {
            return Ok(false);
        };
        let path = self.strip_tb_prefix(&path);
        if path.len() < 2 {
            return Ok(false);
        }
        let Some(&head_cid) = self.ctx.component_fields.get(&path[0]) else {
            return Ok(false);
        };
        let (recv, field) = path.split_at(path.len() - 1);
        let field = field[0].clone();
        // A receiver ending in a data-only scoreboard sub is not a
        // component sub-copy — let the scoreboard handlers claim it.
        if self.recv_is_scoreboard_sub(head_cid, &recv[1..]) {
            return Ok(false);
        }
        let cid = self.resolve_component_recv(head_cid, &recv[1..])?;
        let comp = &self.ctx.components[cid.index()];
        let Some(ComponentFieldKind::Sub { component, .. }) = comp.field(&field).map(|f| &f.kind)
        else {
            return Ok(false);
        };
        let dst_sub = *component;
        // RHS must be a test-scope component local of the same type.
        let Some(src_path) = dotted_path(value) else {
            return Ok(false);
        };
        let src_path = self.strip_tb_prefix(&src_path);
        let Some(&src_cid) = src_path
            .first()
            .and_then(|h| self.ctx.component_fields.get(h))
        else {
            return Ok(false);
        };
        // Resolve through any further sub-path on the RHS (usually none).
        let src_resolved = self.resolve_component_recv(src_cid, &src_path[1..])?;
        if src_resolved != dst_sub {
            return Err(unsupported(
                &format!(
                    "copying component `{}` into sub-component field `{}.{field}` of a \
                     different type `{}`",
                    self.ctx.components[src_resolved.index()].name,
                    recv.join("."),
                    self.ctx.components[dst_sub.index()].name
                ),
                "",
            ));
        }
        self.push(IrStmt::ComponentSubAssign {
            dst: ComponentBase::Path(recv.to_vec()),
            field,
            src: ComponentBase::Path(src_path.to_vec()),
        });
        Ok(true)
    }

    /// The `QueueElem` of the component `queue<T>` field reached by
    /// `(base, queue)`. Resolves the owning component (the `self_component`
    /// for `SelfField`, or the path receiver for `Path`) and reads the
    /// field's element kind. Errors when the field is missing / not a queue
    /// (the resolvers that produced `base` already validated it, so this is
    /// defensive).
    pub(crate) fn component_queue_elem(
        &self,
        base: &ComponentBase,
        queue: &str,
    ) -> Result<crate::ir::QueueElem, LowerError> {
        let cid = match base {
            ComponentBase::SelfField => self.self_component.ok_or_else(|| {
                unsupported(
                    &format!("a self-relative queue `{queue}` outside a component body"),
                    "",
                )
            })?,
            ComponentBase::Path(path) => {
                let head_cid = *self.ctx.component_fields.get(&path[0]).ok_or_else(|| {
                    unsupported(
                        &format!("`{}` is not a component-typed test field", path[0]),
                        "",
                    )
                })?;
                self.resolve_component_recv(head_cid, &path[1..])?
            }
            // A component-typed method-param local never owns a queue
            // field access (only method dispatch reaches a `Local` base).
            ComponentBase::Local(_) => {
                return Err(unsupported(
                    &format!("a queue `{queue}` on a component-typed parameter"),
                    "",
                ));
            }
        };
        let comp = &self.ctx.components[cid.index()];
        match comp.field(queue).map(|f| &f.kind) {
            Some(ComponentFieldKind::Queue { elem }) => Ok(*elem),
            _ => Err(unsupported(
                &format!("`{queue}` is not a queue field of `{}`", comp.name),
                "",
            )),
        }
    }

    /// Resolve a component scalar-field assignment target: a bare
    /// `count` (self-relative, inside a method body) or a dotted path
    /// `env.sb.errors` (test scope). Returns `(base, field)` when it
    /// resolves to a scalar field of a component.
    /// `<inst>.dut = dut` / `env.<drv>.dut = dut` — bind an event-driven
    /// transactor component's `Dut` handle field to the test DUT. Erased
    /// (returns `true` when consumed): the on-handler body's `DutWrite`s
    /// resolve directly to the test's `dut`, so no IR is emitted. Rejects
    /// a bind of a `Dut` field to anything other than the test DUT.
    pub(crate) fn lower_component_dut_bind(
        &self,
        target: &AstExpr,
        value: &AstExpr,
    ) -> Result<bool, LowerError> {
        let Some(path) = dotted_path(target) else {
            return Ok(false);
        };
        let path = self.strip_tb_prefix(&path);
        if path.len() < 2 {
            return Ok(false);
        }
        let (head_cid, recv_prefix, tail): (ComponentId, Vec<String>, &[String]) =
            if let Some(&head_cid) = self.ctx.component_fields.get(&path[0]) {
                (head_cid, vec![path[0].clone()], &path[1..])
            } else if let Some(self_cid) = self.self_component {
                let comp = &self.ctx.components[self_cid.index()];
                match comp.field(&path[0]).map(|f| &f.kind) {
                    Some(ComponentFieldKind::Sub { component, .. }) => (
                        *component,
                        vec!["self".to_string(), path[0].clone()],
                        &path[1..],
                    ),
                    _ => return Ok(false),
                }
            } else {
                return Ok(false);
            };
        let (_recv, field) = path.split_at(path.len() - 1);
        let field = &field[0];
        // A path through a data-only scoreboard sub-component is not a
        // `Dut`-handle bind — let the scoreboard handlers claim it.
        let recv_tail_len = tail.len().saturating_sub(1);
        if self.recv_is_scoreboard_sub(head_cid, &tail[..recv_tail_len]) {
            return Ok(false);
        }
        let cid = self.resolve_component_recv(head_cid, &tail[..recv_tail_len])?;
        let comp = &self.ctx.components[cid.index()];
        if !matches!(
            comp.field(field).map(|f| &f.kind),
            Some(ComponentFieldKind::Dut { .. })
        ) {
            return Ok(false);
        }
        // RHS must be the test DUT.
        let is_dut = match &*value.kind {
            ExprKind::Ident(id) => self.is_dut_name(&id.name),
            _ => false,
        };
        if !is_dut {
            return Err(unsupported(
                &format!(
                    "binding event-driven transactor DUT handle `{}` to something other \
                     than the test DUT",
                    recv_prefix
                        .iter()
                        .chain(tail.iter())
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(".")
                ),
                "",
            ));
        }
        Ok(true)
    }

    pub(crate) fn as_component_field_target(
        &self,
        target: &AstExpr,
    ) -> Result<Option<(ComponentBase, String)>, LowerError> {
        // Self-relative bare field (only inside a method body, and only
        // when the name is NOT a shadowing local).
        if let ExprKind::Ident(id) = &*target.kind {
            if self.lookup(&id.name).is_none() {
                if let Some(cid) = self.self_component {
                    let comp = &self.ctx.components[cid.index()];
                    if let Some(field) = comp.field(&id.name) {
                        if matches!(field.kind, ComponentFieldKind::Scalar { .. }) {
                            self.require_self_activation(field.activation, "field", &id.name)?;
                            return Ok(Some((ComponentBase::SelfField, id.name.clone())));
                        }
                    }
                }
            }
        }
        // Dotted path `env.sb.errors`.
        if let Some(path) = dotted_path(target) {
            let path = self.strip_tb_prefix(&path);
            if path.len() >= 2 {
                if let Some((head_cid, base_head, tail)) = self.component_path_head(&path) {
                    let (recv_tail, field) = tail.split_at(tail.len() - 1);
                    let field = field[0].clone();
                    // A receiver ending in a data-only scoreboard sub
                    // (`top.sb.<scalar>`) is NOT a component-field write —
                    // fall through so the scoreboard scalar-write path
                    // (`scoreboard_root`) handles it.
                    if self.recv_is_scoreboard_sub(head_cid, recv_tail) {
                        return Ok(None);
                    }
                    let cid = self.resolve_component_recv(head_cid, recv_tail)?;
                    let comp = &self.ctx.components[cid.index()];
                    match comp.field(&field) {
                        Some(schema)
                            if matches!(schema.kind, ComponentFieldKind::Scalar { .. }) =>
                        {
                            self.require_component_activation(
                                &base_head[0],
                                head_cid,
                                recv_tail,
                                schema.activation,
                                "field",
                                &field,
                            )?;
                            let mut base = base_head;
                            base.extend_from_slice(recv_tail);
                            return Ok(Some((ComponentBase::Path(base), field)));
                        }
                        _ => {
                            return Err(unsupported(
                                &format!(
                                    "write to `{}` — not a scalar component field",
                                    path.join(".")
                                ),
                                "",
                            ));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    /// Resolve a component scalar-field READ as an `Expr::ComponentField`:
    /// bare `count` (self) or path `env.sb.count`.
    pub(crate) fn as_component_field_read(
        &self,
        e: &AstExpr,
    ) -> Result<Option<IrExpr>, LowerError> {
        if let ExprKind::Ident(id) = &*e.kind {
            if self.lookup(&id.name).is_none() {
                if let Some(cid) = self.self_component {
                    let comp = &self.ctx.components[cid.index()];
                    if let Some(field) = comp.field(&id.name) {
                        if matches!(field.kind, ComponentFieldKind::Scalar { .. }) {
                            self.require_self_activation(field.activation, "field", &id.name)?;
                            return Ok(Some(IrExpr::ComponentField {
                                base: ComponentBase::SelfField,
                                field: id.name.clone(),
                            }));
                        }
                    }
                }
            }
        }
        if let Some(path) = dotted_path(e) {
            let path = self.strip_tb_prefix(&path);
            if path.len() >= 2 {
                if let Some((head_cid, base_head, tail)) = self.component_path_head(&path) {
                    let (recv_tail, field) = tail.split_at(tail.len() - 1);
                    let field = field[0].clone();
                    // A receiver ending in a data-only scoreboard sub
                    // (`top.sb.<scalar>`) is a scoreboard read, not a
                    // component-field read — fall through to `scoreboard_root`.
                    if self.recv_is_scoreboard_sub(head_cid, recv_tail) {
                        return Ok(None);
                    }
                    let cid = self.resolve_component_recv(head_cid, recv_tail)?;
                    let comp = &self.ctx.components[cid.index()];
                    if let Some(schema) = comp.field(&field) {
                        if matches!(schema.kind, ComponentFieldKind::Scalar { .. }) {
                            self.require_component_activation(
                                &base_head[0],
                                head_cid,
                                recv_tail,
                                schema.activation,
                                "field",
                                &field,
                            )?;
                            let mut base = base_head;
                            base.extend_from_slice(recv_tail);
                            return Ok(Some(IrExpr::ComponentField {
                                base: ComponentBase::Path(base),
                                field,
                            }));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    /// Resolve a whole composite-component value READ as an
    /// `Expr::ComponentValue` — the receiver passed by value as a method
    /// argument (`sb.observe(addr, model)` reads `model`). Two shapes:
    ///   * bare `model` inside a method/handler body — a self `Sub`
    ///     sub-component field → `base = Path(["self","model"])`;
    ///   * a test-scope path `env.model` whose terminal is a `Sub` field →
    ///     `base = Path(["env","model"])`.
    /// A component-typed PARAM local (`IrType::Component`) is handled
    /// separately at ident resolution (it is a `Local` base, not a field).
    pub(crate) fn as_component_value_read(
        &self,
        e: &AstExpr,
    ) -> Result<Option<IrExpr>, LowerError> {
        if let ExprKind::Ident(id) = &*e.kind {
            if self.lookup(&id.name).is_none() {
                if let Some(cid) = self.self_component {
                    let comp = &self.ctx.components[cid.index()];
                    if matches!(
                        comp.field(&id.name).map(|f| &f.kind),
                        Some(ComponentFieldKind::Sub { .. })
                    ) {
                        return Ok(Some(IrExpr::ComponentValue {
                            base: ComponentBase::Path(vec!["self".to_string(), id.name.clone()]),
                        }));
                    }
                }
            }
        }
        if let Some(path) = dotted_path(e) {
            let path = self.strip_tb_prefix(&path);
            if path.len() >= 2 {
                if let Some(&head_cid) = self.ctx.component_fields.get(&path[0]) {
                    let (recv, field) = path.split_at(path.len() - 1);
                    let field = field[0].clone();
                    if self.recv_is_scoreboard_sub(head_cid, &recv[1..]) {
                        return Ok(None);
                    }
                    let cid = self.resolve_component_recv(head_cid, &recv[1..])?;
                    let comp = &self.ctx.components[cid.index()];
                    if matches!(
                        comp.field(&field).map(|f| &f.kind),
                        Some(ComponentFieldKind::Sub { .. })
                    ) {
                        return Ok(Some(IrExpr::ComponentValue {
                            base: ComponentBase::Path(path.to_vec()),
                        }));
                    }
                }
            }
        }
        Ok(None)
    }

    /// True when the receiver path `segs` (relative to `head`) terminates
    /// at a data-only scoreboard sub-component field (`top.sb`). Such a
    /// path is a scoreboard access, not a component-field path — the
    /// component-field resolvers fall through so `scoreboard_root` handles
    /// it. A non-terminal `ScoreboardSub` (a board can hold no subs) or an
    /// unresolvable segment returns false (the normal resolver then
    /// produces the precise error).
    fn recv_is_scoreboard_sub(&self, head: ComponentId, segs: &[String]) -> bool {
        let mut cid = head;
        for (i, seg) in segs.iter().enumerate() {
            let comp = &self.ctx.components[cid.index()];
            match comp.field(seg).map(|f| &f.kind) {
                Some(ComponentFieldKind::Sub { component, .. }) => cid = *component,
                Some(ComponentFieldKind::ScoreboardSub { .. }) => {
                    return i == segs.len() - 1;
                }
                _ => return false,
            }
        }
        false
    }

    /// Walk a sub-component receiver path from a head component down
    /// `segs` (each must name a `Sub` field), returning the final
    /// ComponentId. `segs` is the path AFTER the head local segment.
    fn resolve_component_recv(
        &self,
        head: ComponentId,
        segs: &[String],
    ) -> Result<ComponentId, LowerError> {
        let mut cid = head;
        for seg in segs {
            let comp = &self.ctx.components[cid.index()];
            match comp.field(seg).map(|f| &f.kind) {
                Some(ComponentFieldKind::Sub { component, .. }) => cid = *component,
                _ => {
                    return Err(unsupported(
                        &format!("`{seg}` is not a sub-component of `{}`", comp.name),
                        "",
                    ));
                }
            }
        }
        Ok(cid)
    }

    /// Resolve the inherited/overridden transactor mode at a component
    /// receiver. Structural components preserve inherited context while a
    /// transactor field consumes an explicit override or requires one from
    /// its parent/root.
    fn resolve_component_mode(
        &self,
        head_name: &str,
        head: ComponentId,
        segs: &[String],
    ) -> Result<Option<ComponentInstanceMode>, LowerError> {
        let inherited = self.ctx.component_modes.get(head_name).copied().flatten();
        if matches!(
            self.ctx.components[head.index()].kind,
            crate::ir::ComponentKindTag::Transactor
        ) && inherited.is_none()
        {
            return Err(LowerError::Invalid(format!(
                "transactor `{head_name}` has no effective active/passive mode"
            )));
        }
        let resolved = crate::ir::resolve_component_path_mode(
            &self.ctx.components,
            head,
            inherited,
            segs,
        )
        .map_err(|err| match err {
            crate::ir::ComponentPathResolutionError::NotSubcomponent { .. } => {
                unsupported(&err.to_string(), "")
            }
            _ => LowerError::Invalid(err.to_string()),
        })?;
        let target = &self.ctx.components[resolved.component.index()];
        if matches!(target.kind, crate::ir::ComponentKindTag::Transactor)
            && target.requires_instance_mode()
            && resolved.effective_mode.is_none()
        {
            let path = std::iter::once(head_name)
                .chain(segs.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(".");
            return Err(LowerError::Invalid(format!(
                "transactor path `{path}` has no effective active/passive mode"
            )));
        }
        Ok(matches!(
            target.kind,
            crate::ir::ComponentKindTag::Transactor
        )
        .then_some(resolved.effective_mode)
        .flatten())
    }

    fn require_component_activation(
        &self,
        head_name: &str,
        head: ComponentId,
        segs: &[String],
        activation: Activation,
        member_kind: &str,
        member: &str,
    ) -> Result<(), LowerError> {
        if matches!(activation, Activation::Always) {
            return Ok(());
        }
        let mode = self.resolve_component_mode(head_name, head, segs)?;
        if !matches!(mode, Some(ComponentInstanceMode::Active)) {
            let path = std::iter::once(head_name)
                .chain(segs.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(".");
            return Err(LowerError::Invalid(format!(
                "active-only {member_kind} `{member}` is used through passive transactor `{path}`"
            )));
        }
        Ok(())
    }

    fn require_self_activation(
        &self,
        activation: Activation,
        member_kind: &str,
        member: &str,
    ) -> Result<(), LowerError> {
        if matches!(activation, Activation::ActiveOnly) && !self.self_component_active_only {
            return Err(LowerError::Invalid(format!(
                "always-on component body cannot access active-only {member_kind} `{member}`"
            )));
        }
        Ok(())
    }

    /// Resolve the first segment of a component path. Test-scope paths
    /// (`env.mon.x`) are rooted at `env`; self-relative method paths
    /// (`mon.x` inside `testbench Env`) are rooted at `self.mon`.
    fn component_path_head<'a>(
        &self,
        path: &'a [String],
    ) -> Option<(ComponentId, Vec<String>, &'a [String])> {
        if let Some(&head_cid) = self.ctx.component_fields.get(&path[0]) {
            return Some((head_cid, vec![path[0].clone()], &path[1..]));
        }
        let self_cid = self.self_component?;
        let comp = &self.ctx.components[self_cid.index()];
        match comp.field(&path[0]).map(|f| &f.kind) {
            Some(ComponentFieldKind::Sub { component, .. }) => Some((
                *component,
                vec!["self".to_string(), path[0].clone()],
                &path[1..],
            )),
            _ => None,
        }
    }

    /// Lower the args of a component method call (port-hoisted, like any
    /// host-side call).
    pub(crate) fn lower_component_call_args(
        &mut self,
        args: &[CallArg],
    ) -> Result<Vec<IrExpr>, LowerError> {
        let mut out = Vec::with_capacity(args.len());
        for a in args {
            let CallArg::Expr(e) = a else {
                // No CODEGEN site in v1 reads an argument name: of the
                // 30 `CallArg::Named` matches in `cpp_tb.rs`, 25 are
                // `{ value, .. }` and one is `{ value: e, .. }` — all 26
                // drop the name — and the 4 that bind `name` are
                // AST-rewrite passes that reconstruct the node and pass
                // it along. So binding is by position everywhere.
                // Measured, not just read:
                // `axil_write(data = t.value, addr = t.addr)` emits
                // `AxilXactor_axil_write(_tb.env.drv, t.value, t.addr)` —
                // the two arguments SWAPPED, silently, in code that
                // compiles and runs. A misspelled name is accepted with no
                // diagnostic at all.
                //
                // Reordering is the entire point of naming arguments, so
                // this is the worst outcome the sweep classifies and
                // `--codegen v1` is the last place to send the user.
                //
                // Split on the ARGUMENT count, because a single argument
                // cannot be reordered: there is no other position for it
                // to land in, so v1 emits exactly the call the positional
                // form emits (verified against `axil_read(addr = ..)` and
                // `axil_write(data = ..)` in `axilite_env_test`). The
                // named-ness contributes nothing there, which is what
                // makes `--codegen v1` honest.
                //
                // Deliberately NOT a claim about the callee's parameter
                // count, which this seam does not know. A one-argument
                // call to a two-parameter method emits an uncompilable
                // call under v1 — but so does the positional
                // `axil_write(t.value)`, which tbir lowers and verifies
                // clean. That arity gap is real and pre-existing; it is
                // not something naming the argument caused, and the
                // wording below does not pretend the call is well-formed.
                //
                // Arity ≥ 2 is NOT split further — same-order names emit
                // correctly too, but telling the two apart needs the
                // callee's parameter list, and an arm's status is the
                // worst thing under it.
                if args.len() == 1 {
                    // Not "method call": this helper also lowers the
                    // payload of `emit <ev>(...)`, so the construct name
                    // covers both callers rather than naming one of them.
                    return Err(unsupported(
                        "a named argument in a single-argument component call",
                        "v1 ignores the name and binds by position; with one argument there \
                         is no other position, so it emits exactly the positional form",
                    ));
                }
                return Err(not_implemented(
                    "named arguments in a component method call",
                    "v1 ignores argument names and binds strictly by position, so names \
                     written out of declaration order silently SWAP the values",
                    V1Status::SilentlyMisLowers,
                ));
            };
            out.push(self.lower_expr_no_ports(e)?);
        }
        Ok(out)
    }

    /// Lower `emit <event>(args)` to a `Stmt::ComponentEmit`. Two forms:
    ///   * self-relative `emit observed(v)` inside a method/on-handler body
    ///     (`base = SelfField`, single segment);
    ///   * test-scope path `emit env.agent.in_ev(v)` (`base = Path`, head
    ///     segment is a test-scope component local, trailing segment is the
    ///     event field of the resolved sub-component).
    pub(crate) fn lower_emit(
        &mut self,
        name: &crate::ast::Path,
        args: &[CallArg],
    ) -> Result<(), LowerError> {
        // Path form: `emit <comp-local>.<subs...>.<event>(args)`. The
        // impl-for desugaring prefixes a testbench-field component with
        // `_tb` (`emit _tb.prod.in_ev(..)`); strip it so both binding
        // shapes resolve through `component_fields` by the bare name.
        let segs: Vec<String> = name.segments.iter().map(|s| s.name.clone()).collect();
        let segs = self.strip_tb_prefix(&segs);
        if segs.len() >= 2 {
            let head = segs[0].clone();
            if let Some(&head_cid) = self.ctx.component_fields.get(&head) {
                let recv: Vec<String> = segs[..segs.len() - 1].to_vec();
                let event = segs.last().unwrap().clone();
                let cid = self.resolve_component_recv(head_cid, &recv[1..])?;
                let comp = &self.ctx.components[cid.index()];
                match comp.field(&event) {
                    Some(field) if matches!(field.kind, ComponentFieldKind::Event { .. }) => {
                        self.require_component_activation(
                            &head,
                            head_cid,
                            &recv[1..],
                            field.activation,
                            "event",
                            &event,
                        )?;
                    }
                    _ => {
                        return Err(unsupported(
                            &format!(
                                "`emit {}` — `{}.{event}` is not an `event` field",
                                path_str(name),
                                comp.name
                            ),
                            "",
                        ));
                    }
                }
                let lowered = self.lower_component_call_args(args)?;
                self.push(IrStmt::ComponentEmit {
                    base: ComponentBase::Path(recv),
                    event,
                    args: lowered,
                });
                return Ok(());
            }
        }
        // Test-scope event channel: `emit e(x)` on a `let e : event<T>`
        // local. v1 emits the same synchronous fan-out
        // (`for (auto& _s : e) _s(x);`).
        if segs.len() == 1 {
            if let Some(local) = self.lookup(&segs[0]) {
                if let IrType::Event(payload) = *self.local_type(local) {
                    if args.len() != 1 {
                        return Err(LowerError::Invalid(format!(
                            "`emit {}` carries {} argument(s); an event payload is exactly one",
                            segs[0],
                            args.len()
                        )));
                    }
                    let lowered = self.lower_component_call_args(args)?;
                    // Shape must agree: the channel renders as
                    // `std::function<void(uint64_t)>` or
                    // `std::function<void(<Record>)>`, and passing one
                    // where the other is expected is a hard C++ error.
                    // Signedness is left alone — both backends widen a
                    // scalar payload to a 64-bit slot, so `sint` into an
                    // `event<uint<8>>` is the same benign conversion v1
                    // performs.
                    let arg_ty = self.expr_type(&lowered[0]).unwrap_or(IrType::Unknown);
                    let shape_ok = match (payload, &arg_ty) {
                        (_, IrType::Unknown) => true,
                        (EventPayload::Record(want), IrType::Record(got)) => want == *got,
                        (EventPayload::Record(_), _) => false,
                        (EventPayload::Scalar { .. }, IrType::Record(_)) => false,
                        (EventPayload::Scalar { .. }, _) => true,
                    };
                    if !shape_ok {
                        let want = match payload {
                            EventPayload::Scalar { signed: true } => "sint".to_string(),
                            EventPayload::Scalar { signed: false } => "uint".to_string(),
                            EventPayload::Record(r) => self.ctx.records[r.index()].name.clone(),
                        };
                        return Err(LowerError::Invalid(format!(
                            "`emit {}`: the channel carries `event<{want}>`, but the payload \
                             does not match that shape",
                            segs[0]
                        )));
                    }
                    self.push(IrStmt::EventEmit {
                        event: local,
                        args: lowered,
                    });
                    return Ok(());
                }
            }
        }
        // Self-relative form, inside a method/on-handler body.
        let cid = self.self_component.ok_or_else(|| {
            // Reaching here means the channel did not resolve at all. v1
            // emits the fan-out anyway (`for (auto& _s : a.b) _s(x);`),
            // naming a symbol that does not exist, so the generated TB
            // does not compile.
            not_implemented(
                "`emit` outside a component method body or a test-scope event channel",
                "`emit <e>(x)` needs either a component `event` field or a test-scope \
                 `let <e> : event<T>` in scope",
                V1Status::EmitsUncompilable,
            )
        })?;
        if name.segments.len() != 1 {
            return Err(unsupported(
                &format!("`emit {}` to a dotted event path", path_str(name)),
                "only self-relative `emit <event>(...)` or a test-scope \
                 `emit <comp>.<event>(...)` is lowered",
            ));
        }
        let event = name.segments[0].name.clone();
        let comp = &self.ctx.components[cid.index()];
        match comp.field(&event) {
            Some(field) if matches!(field.kind, ComponentFieldKind::Event { .. }) => {
                self.require_self_activation(field.activation, "event", &event)?;
            }
            _ => {
                return Err(unsupported(
                    &format!("`emit {event}` — not an `event` field of `{}`", comp.name),
                    "",
                ));
            }
        }
        let lowered = self.lower_component_call_args(args)?;
        self.push(IrStmt::ComponentEmit {
            base: ComponentBase::SelfField,
            event,
            args: lowered,
        });
        Ok(())
    }

    /// Lower `<comp>.idle(N)` / `.idle_in(N)` / `.idle_out(N)` to an
    /// `Expr::ComponentIdle` when the callee resolves to a component
    /// instance path. Returns `None` when the callee is not an idle
    /// predicate on a known component (caller falls through to other
    /// call-lowering paths).
    pub(crate) fn as_component_idle(
        &mut self,
        callee: &AstExpr,
        args: &[CallArg],
    ) -> Result<Option<IrExpr>, LowerError> {
        let ExprKind::Field { target, name } = &*callee.kind else {
            return Ok(None);
        };
        let kind = match name.name.as_str() {
            "idle" => crate::ir::IdleKind::Both,
            "idle_in" => crate::ir::IdleKind::In,
            "idle_out" => crate::ir::IdleKind::Out,
            _ => return Ok(None),
        };
        // The receiver must be a path rooted at a component local — a
        // bare `agent` / `env.agent` (test-scope let) or a `_tb`-prefixed
        // `_tb.prod` (testbench field, after impl-for desugaring). Strip
        // the `_tb` prefix so both resolve through `component_fields`.
        let Some(raw) = dotted_path(target) else {
            return Ok(None);
        };
        let path = self.strip_tb_prefix(&raw).to_vec();
        let Some(&head_cid) = path.first().and_then(|h| self.ctx.component_fields.get(h)) else {
            return Ok(None);
        };
        // Walk sub-component segments to confirm the path resolves to a
        // component (the idle stamps live on every component struct).
        self.resolve_component_recv(head_cid, &path[1..])?;
        if args.len() != 1 {
            return Err(unsupported(
                &format!("`{}(...)` with {} arguments", name.name, args.len()),
                "idle predicates take exactly one cycle-count argument",
            ));
        }
        let CallArg::Expr(n_expr) = &args[0] else {
            // Stays `Unsupported`, unlike the general component-method
            // arm above. The arity check three lines up has already
            // established exactly one argument, so v1's name-dropping
            // positional binding lands the value in the only slot there
            // is and emits code identical to the positional form. No
            // reordering hazard exists here.
            return Err(unsupported(
                &format!("a named argument to `{}`", name.name),
                "v1 ignores the name and binds by position; with one parameter that is \
                 the same thing, so it emits the predicate correctly",
            ));
        };
        let n = self.lower_expr_no_ports(n_expr)?;
        Ok(Some(IrExpr::ComponentIdle {
            base: ComponentBase::Path(path),
            kind,
            n: Box::new(n),
        }))
    }

    /// Lower `<env>.quiesced(N)` to a conjunction of `idle(N)` predicates
    /// over every LEAF sub-component reachable from the receiver — the
    /// env-level aggregation form (spec §8.x). Mirrors v1's
    /// `resolve_component_quiesced_predicate` + `collect_quiesced_paths`:
    /// the receiver's sub-component tree is walked, and each leaf (a
    /// component with no further sub-components, or a data-only scoreboard
    /// sub) contributes one `idle(N)` term. A receiver with no
    /// sub-components is itself the single leaf. Returns `None` when the
    /// callee is not a `quiesced` predicate on a known component instance.
    pub(crate) fn as_component_quiesced(
        &mut self,
        callee: &AstExpr,
        args: &[CallArg],
    ) -> Result<Option<IrExpr>, LowerError> {
        let ExprKind::Field { target, name } = &*callee.kind else {
            return Ok(None);
        };
        if name.name != "quiesced" {
            return Ok(None);
        }
        let Some(raw) = dotted_path(target) else {
            return Ok(None);
        };
        let path = self.strip_tb_prefix(&raw).to_vec();
        let Some(&head_cid) = path.first().and_then(|h| self.ctx.component_fields.get(h)) else {
            return Ok(None);
        };
        // Resolve the receiver to its component (errors if a mid-path
        // segment is not a sub-component).
        let recv_cid = self.resolve_component_recv(head_cid, &path[1..])?;
        if args.len() != 1 {
            return Err(unsupported(
                &format!("`quiesced(...)` with {} arguments", args.len()),
                "the quiesce predicate takes exactly one cycle-count argument",
            ));
        }
        let CallArg::Expr(n_expr) = &args[0] else {
            // Same as the idle predicates: one argument, so v1's
            // positional binding is correct and the name is decoration.
            return Err(unsupported(
                "a named argument to `quiesced`",
                "v1 ignores the name and binds by position; with one parameter that is \
                 the same thing, so it emits the predicate correctly",
            ));
        };
        let n = self.lower_expr_no_ports(n_expr)?;

        // Collect every leaf sub-component instance path under the receiver.
        let mut leaves: Vec<Vec<String>> = Vec::new();
        let mut stack: std::collections::HashSet<ComponentId> = std::collections::HashSet::new();
        self.collect_quiesce_leaves(recv_cid, path.clone(), &mut stack, &mut leaves);

        // Each leaf → `idle(N)` (both stamps). AND them together. A single
        // leaf yields a bare `ComponentIdle` (no redundant `&& true`),
        // matching v1's per-condition expansion.
        let mut terms = leaves.into_iter().map(|leaf| IrExpr::ComponentIdle {
            base: ComponentBase::Path(leaf),
            kind: crate::ir::IdleKind::Both,
            n: Box::new(n.clone()),
        });
        let first = terms
            .next()
            .expect("collect_quiesce_leaves always yields at least the receiver as a leaf");
        let conj = terms.fold(first, |acc, t| {
            IrExpr::Binary(crate::ir::BinOp::And, Box::new(acc), Box::new(t))
        });
        Ok(Some(conj))
    }

    /// Walk the sub-component tree rooted at `cid`/`inst_path`, pushing one
    /// instance path per LEAF. A component with at least one sub-component
    /// (`Sub` or `ScoreboardSub`) is NOT a leaf — only its descendants are.
    /// A component with no sub-components, or a data-only scoreboard sub, is
    /// a leaf. Mirrors v1's `collect_quiesced_paths`, including its
    /// cycle guard: a by-value component cycle is not C++-constructible but
    /// IS expressible in the schema (all component ids are registered before
    /// field lowering, so two envs can name each other), so a revisited
    /// component terminates as a leaf instead of recursing forever.
    fn collect_quiesce_leaves(
        &self,
        cid: ComponentId,
        inst_path: Vec<String>,
        stack: &mut std::collections::HashSet<ComponentId>,
        out: &mut Vec<Vec<String>>,
    ) {
        if !stack.insert(cid) {
            out.push(inst_path);
            return;
        }
        let comp = &self.ctx.components[cid.index()];
        let mut found_sub = false;
        for f in &comp.fields {
            match &f.kind {
                ComponentFieldKind::Sub { component, .. } => {
                    found_sub = true;
                    let mut sub_path = inst_path.clone();
                    sub_path.push(f.name.clone());
                    self.collect_quiesce_leaves(*component, sub_path, stack, out);
                }
                ComponentFieldKind::ScoreboardSub { .. } => {
                    // A data-only scoreboard is always a leaf (it has no
                    // sub-components of its own).
                    found_sub = true;
                    let mut sub_path = inst_path.clone();
                    sub_path.push(f.name.clone());
                    out.push(sub_path);
                }
                _ => {}
            }
        }
        if !found_sub {
            out.push(inst_path);
        }
        stack.remove(&cid);
    }
}

fn path_str(p: &crate::ast::Path) -> String {
    p.segments
        .iter()
        .map(|s| s.name.clone())
        .collect::<Vec<_>>()
        .join(".")
}
