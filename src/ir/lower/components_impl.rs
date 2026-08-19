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
    FixedVecSchema, FunctionId, FunctionKind, IrType, RecordId, ScoreboardId, TbFunction,
    Terminator, TypedParam,
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
pub(crate) fn transactor_is_component(
    t: &TransactorDecl,
    env_held: bool,
    record_ids: &HashMap<String, RecordId>,
) -> bool {
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
                } else if let TypeExpr::Named { name, .. } = &f.ty {
                    let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                    // Declared records are persistent host-side state, not
                    // DUT handles. Only an otherwise-unclassified named type
                    // is evidence that this transactor owns a module field.
                    if !record_ids.contains_key(simple) {
                        has_module_field = true;
                    }
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
pub(crate) fn transactor_is_analysis_source(
    t: &TransactorDecl,
    record_ids: &HashMap<String, RecordId>,
) -> bool {
    if t.bound_to.is_some() {
        return false;
    }
    let mut has_event = false;
    let mut has_named_field = false;
    for it in t.items.iter().chain(t.when_active.iter().flatten()) {
        if let ComponentItem::Field(f) = it {
            if is_event_field(f) {
                has_event = true;
            } else if let TypeExpr::Named { name, .. } = &f.ty {
                let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                if !record_ids.contains_key(simple) {
                    has_named_field = true;
                }
            }
        }
    }
    has_event && !has_named_field
}

/// Whether an analysis-source transactor has behavior or storage whose
/// availability depends on the instance mode.
pub(crate) fn transactor_has_mode_sensitive_analysis_surface(
    t: &TransactorDecl,
    record_ids: &HashMap<String, RecordId>,
) -> bool {
    transactor_is_analysis_source(t, record_ids)
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

/// True when a consumer's subscribing `on <ev>` handler lives ONLY under
/// `when active`, so it registers for an `active` instance and for no
/// other. Such an instance bound `passive` (or mode-less, where the mode
/// is not inherited as `active`) is inert in the one way that matters:
/// nothing subscribes to its `in event`, so an `emit <inst>.<ev>(..)`
/// runs its fan-out loop over an empty vector and the transaction is
/// dropped on the floor.
///
/// That is what the emitter produced for `t : T passive` here — the
/// `for (auto& _s : t.req) _s(1);` loop with no `push_back` anywhere —
/// and no gate objected, because the analysis-source mode gates accept
/// `passive` and run ahead of the event-driven one.
///
/// A consumer whose `on` handler sits in the ordinary body is NOT this:
/// it registers regardless of mode, the emitter wires it on a passive
/// instance, and it keeps accepting every mode the analysis-source
/// policy allows.
pub(crate) fn transactor_is_active_only_consumer(t: &TransactorDecl) -> bool {
    if !transactor_is_event_driven(t) {
        return false;
    }
    let subscribes = |items: &[ComponentItem]| {
        items
            .iter()
            .any(|it| matches!(it, ComponentItem::OnHandler(h) if !h.periodic))
    };
    // An always-on handler anywhere means something registers for every
    // mode, so the instance is not inert even if a second handler is
    // active-only.
    !subscribes(&t.items) && t.when_active.as_deref().is_some_and(subscribes)
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

/// An `event<T>` field. `pub(crate)` because the unbound-transactor
/// item walk splits its directional-field rejection on exactly this
/// question — v1 models a directional EVENT (a real subscriber vector
/// plus a fan-out at the emit site) and flattens a directional SCALAR to
/// an uninitialized `uint64_t` — and asking the routing predicate's own
/// helper is what keeps the two answers from drifting apart.
pub(crate) fn is_event_field(f: &ComponentField) -> bool {
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn lower_component_schema(
    src: &CompSource<'_>,
    ids: &HashMap<String, ComponentId>,
    scoreboard_ids: &HashMap<String, ScoreboardId>,
    record_ids: &HashMap<String, RecordId>,
    next_fn: &mut u32,
    consts: &HashMap<String, super::ConstVal>,
    declared_types: &std::collections::HashSet<String>,
    // `record_ids` restricted to transactions and structs — v1's
    // `Emitter::is_record_type`. `record_ids` itself also holds every
    // regblock's mirror record by the time components lower, and the
    // record-field arm below asks a v1-PARITY question, so it must not
    // use the contaminated map.
    declared_records: &std::collections::HashSet<String>,
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
            bound_bus = Some(super::bound_bus_name(
                bt,
                &format!("event-driven transactor `{name}`"),
            )?);
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
            // ONE label, after three wrong verdicts. v1 does three
            // different things here, and the arm carries the worst.
            //
            // Measured, `agent Watcher bound to BusAxiLite` in eight
            // cells — four handler shapes across the two instantiation
            // positions, counting bare `bus` identifiers outside
            // comments in v1's output:
            //
            //                                = bind    tb field
            //   on <ev> driver                  0          4
            //   on <bool-expr> cycle trigger    1          1
            //   on N cycles + a bus write       1          1
            //   on bus.<ch>.handshake(...)      1          1
            //
            // A `hookable` body behaves exactly like the `on <ev>`
            // driver — 0 bare `bus` at a bind, 4 as a field — so the
            // working column has two entries, not one. (v1 REFUSES the
            // transactor spelling of that same program, "drives a DUT
            // signal from the always-on body … spec §8.1"; the
            // env/agent spelling bypasses that check.)
            //
            // `bus` is declared nowhere in the non-working cells, so
            // g++ answers "'bus' was not declared in this scope".
            //
            // And a NINTH cell is worse than either: a
            // `thread bus.<method>(...)` responder. v1 COMPILES that
            // one — zero bare `bus` references — because it drops the
            // responder coroutine entirely. Against
            // `tlm_target_thread_if_test`, changing only the keyword
            // `transactor` to `agent` deletes the whole
            // `_target_read_target_slot` block (300 emitted lines to
            // 242); the DUT's blocking read is then never answered and
            // the test's own assertions fail at run time. All four
            // declaration kinds emit byte-identically. That is
            // `SilentlyMisLowers`, and it outranks the seven
            // uncompilable cells.
            //
            // Two earlier versions tried to name the working cell with
            // a predicate here. The first keyed on "is there a
            // non-periodic `on bus.<ch>.handshake` handler", which
            // misses a cycle trigger that reads the bus, a periodic
            // handler that writes it, and — because `parse_on_handler`
            // reads the trigger before the `cycles` decoration —
            // `on bus.w.handshake(d) cycles`, the canonical broken
            // shape wearing one extra word. The instantiation position
            // IS knowable here (`lower_program` pre-scans every
            // `TestItem::Let { bind: true }` before components lower,
            // and already threads five whole-file pre-scans into this
            // function), so an earlier note claiming otherwise was
            // wrong — but a position split does not help either, since
            // the `thread` cell is silent in BOTH columns.
            //
            // So: one label, the worst one, and a detail that names
            // both the working cell and the silent one.
            return Err(not_implemented(
                &format!("a `bound to` clause on {} `{name}`", kind.keyword()),
                "v1 does three different things with this clause. A \
                 `thread bus.<method>(...)` responder COMPILES and is silently dropped — \
                 the target never answers. An `on <ev>` handler body OR a `hookable` \
                 body, on an instance bound at a `let x : C = bind <bus>` site, emits a \
                 working driver. A cycle trigger, a periodic handler, an \
                 `on bus.<ch>.handshake(...)` monitor, or either working shape \
                 instantiated as a plain testbench field, emit `bus` verbatim into a \
                 scope that declares no such name",
                V1Status::SilentlyMisLowers,
            ));
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
                        declared_records,
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
                    let param_names: Vec<String> =
                        h.params.iter().map(|p| p.name.name.clone()).collect();
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
                        param_names,
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
        // v1's poke lowering hardwires the name `dut` and resolves it
        // to the TEST's DUT pointer, so the transactor's own module
        // field is an inert `VTop* dut = nullptr;` member. A SECOND
        // handle is inert too while unread — but a poke through it
        // emits `_tb.drv.dut2.en`, a `.` on a `VTop*`: "request for
        // member 'en' ... which is of pointer type".
        return Err(not_implemented(
            &format!(
                "an event-driven transactor `{name}` with more than one DUT handle field \
                 ({})",
                dut_fields.join(", ")
            ),
            "the consumer BFM drives exactly one DUT instance; v1 emits the extra handle \
             as an unbound `VTop* <name> = nullptr;` member and a poke through it as \
             `_tb.<inst>.<name>.<sig>` — a `.` on a pointer, which g++ refuses",
            V1Status::EmitsUncompilable,
        ));
    }
    if let Some(&df) = dut_fields.first() {
        if df != "dut" {
            // Same mechanism, one handle: v1 only rewrites a poke into
            // `dut->...` when the field is literally named `dut`.
            // Under any other name it emits `_tb.<inst>.<name>.<sig>`
            // against a `VTop*`.
            return Err(not_implemented(
                &format!("an event-driven transactor DUT handle field named `{df}`"),
                "name the DUT handle `dut` (the handler body resolves DUT pokes through \
                 it); v1 rewrites a poke to the test DUT only for that name, and emits \
                 `_tb.<inst>.<name>.<sig>` on a `VTop*` for any other",
                V1Status::EmitsUncompilable,
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
        if let Some(channel) = bus_handshake_monitor_channel(h) {
            // `on bus.<ch>.handshake(arg)` — desugars to a cycle-trigger
            // handler (valid && ready, rising edge) observing the bound
            // bus channel. Only valid on a `bound to <Bus>` transactor.
            if bound_bus.is_none() {
                // v1 emits the handler anyway, against a `bus` that
                // does not exist in the emitted scope: "'bus' was not
                // declared in this scope", and the same for the
                // payload argument. Nothing to re-run with.
                return Err(not_implemented(
                    &format!(
                        "an `on bus.<ch>.handshake(...)` handshake-monitor handler on \
                         non-bound component `{name}`"
                    ),
                    "handshake-monitor handlers observe a `bound to <Bus>` transactor's \
                     channels; an unbound component has no bus to observe, and v1 emits \
                     the handler regardless — g++ answers \"'bus' was not declared in \
                     this scope\"",
                    V1Status::EmitsUncompilable,
                ));
            }
            cycle_asts.push((h, Some(channel), *activation));
        } else if let Some((event, arg_payload, args)) = event_subscription(h, &fields) {
            validate_event_handler(name, h, &event, args)?;
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
            if transactor_is_analysis_source(t, record_ids)
                && !transactor_has_mode_sensitive_analysis_surface(t, record_ids) =>
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
fn event_subscription<'a>(
    h: &'a crate::ast::OnHandler,
    fields: &[ComponentFieldSchema],
) -> Option<(String, EventPayload, &'a [crate::ast::CallArg])> {
    let ExprKind::Call { callee, args } = &*h.event.kind else {
        return None;
    };
    let ExprKind::Ident(id) = &*callee.kind else {
        return None;
    };
    let ComponentFieldKind::Event { payload } = fields.iter().find(|f| f.name == id.name)?.kind
    else {
        return None;
    };
    Some((id.name.clone(), payload, args.as_slice()))
}

/// The channel name of an `on bus.<ch>.handshake(arg)` monitor handler
/// (`<ch>` from the `<bus>.<ch>.handshake` callee), or `None` if `h` is
/// not a handshake-monitor at all.
///
/// This is also the PREDICATE. A separate `is_bus_handshake_monitor`
/// used to test the same three conditions — a `Call`, a callee
/// `Field { name: "handshake" }`, and a `Field` target — and the caller
/// ran the predicate first, then this function, then reported "a
/// malformed `on bus.<ch>.handshake(...)` handler" if they disagreed.
/// They cannot: whenever the predicate passed, this returned `Some`.
/// One rule stated twice, and the second copy guarded nothing.
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

/// Validate an `on <event>(arg) ... end on` self-event subscription.
///
/// This used to also RESOLVE the subscription — re-deriving the three
/// facts `event_subscription` had already established to route the
/// handler here, and carrying a rejection arm for each: a non-`Call`
/// trigger, a dotted callee, a field that is not an `event`, and no such
/// field. All four were unreachable, as was the `h.periodic` check (a
/// periodic handler goes into `periodic_asts` at the item split and
/// never reaches this loop). Replacing each of the five with
/// `unreachable!()` left the whole suite green, and probing all five
/// shapes shows them landing on other diagnostics entirely — a dotted
/// `on tagger.in_ev(t)` on "transactor/method call `.in_ev(...)`", an
/// `on other(t)` naming a scalar field on "helper call `other(...)`".
///
/// So the resolution moved INTO the routing predicate, which now
/// returns what it found instead of a bool, and only the two checks
/// that a routed handler can still fail are left:
///
/// | trigger | v1 emits | verdict |
/// |---|---|---|
/// | `on in_ev(t) pre` | the handler with the hook side dropped, byte-identically | `SilentlyMisLowers` |
/// | `on in_ev()` | `[&](uint64_t _v) { … }` — a synthesized name for a payload the body cannot reference anyway | a real escape hatch |
/// | `on in_ev(t, u)` | `[&](uint64_t t) { … }` — the extra parameter is dropped without a word | `SilentlyMisLowers` |
///
/// The two arity halves were one arm until they were measured
/// separately. Zero arguments is the one shape v1 gets right: the
/// payload is unbound, which is what was written.
///
/// The multi-argument half was first labelled `EmitsUncompilable`, on a
/// body whose `u` had nothing else to resolve to. That is the LESSER of
/// the two things v1 does here, and an arm's status is the worst one.
/// Give `u` something to bind to — a component field `u : uint<8>
/// default 7`, or a file-scope `const u = 9` — and v1 emits
/// `tagger.seen = tagger.seen + tagger.u;` or `... + u;`, compiles
/// clean, and runs to a value the source never asked for.
/// `validate_cycle_handler`, a hundred lines above, is the same
/// two-shape arm and resolves it the other way round.
fn validate_event_handler(
    comp: &str,
    h: &crate::ast::OnHandler,
    event: &str,
    args: &[crate::ast::CallArg],
) -> Result<(), LowerError> {
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
    if args.len() > 1 {
        return Err(not_implemented(
            &format!(
                "an `on {event}(...)` handler with {} arguments on `{comp}`",
                args.len()
            ),
            "event handlers take exactly one payload argument; v1 emits the lambda with \
             only the first parameter and says nothing, so a body naming a later one \
             either fails to resolve or silently picks up a same-named component field \
             or file-scope `const`",
            V1Status::SilentlyMisLowers,
        ));
    }
    if args.is_empty() {
        return Err(unsupported(
            &format!("an `on {event}()` handler with no payload argument on `{comp}`"),
            "event handlers bind the payload to exactly one name; v1 synthesizes one",
        ));
    }
    Ok(())
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
    declared_records: &std::collections::HashSet<String>,
) -> Result<ComponentFieldKind, LowerError> {
    let fname = &f.name.name;
    if f.bound_to.is_some() {
        // The FIFTH copy of the `bound to` rule — the per-FIELD
        // spelling on an env/agent/sequencer/transactor, sibling of the
        // scoreboard one in `scoreboards.rs`. It was the last arm in
        // the family still promising `--codegen v1`, and v1 discards
        // the clause: with the bound and unbound sources padded to the
        // SAME byte length (so no source-offset residue can explain
        // it), v1's output is byte-identical.
        return Err(not_implemented(
            &format!("a `bound to` clause on field `{comp}.{fname}`"),
            "v1 discards the clause — with the bound and unbound sources padded to equal \
             length its output is byte-identical, so the binding silently does not happen",
            V1Status::SilentlyMisLowers,
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
        TypeExpr::Builtin {
            name: BuiltinTy::Vec,
            args,
            ..
        } => {
            if f.direction.is_some() {
                return Err(unsupported(
                    &format!("a directional fixed-vector field `{comp}.{fname}`"),
                    "fixed component state is non-directional",
                ));
            }
            if f.default.is_some() {
                return Err(unsupported(
                    &format!("a default value on fixed-vector field `{comp}.{fname}`"),
                    "fixed vectors are value-initialized; aggregate defaults are not lowered",
                ));
            }
            let elem = match args.first() {
                Some(TypeArg::Type(ty)) => scalar_ir_type(ty),
                _ => None,
            }
            .filter(|ty| matches!(ty,
                IrType::UInt(Some(w)) | IrType::SInt(Some(w)) if *w > 0
            ) || matches!(ty, IrType::Bool))
            .ok_or_else(|| unsupported(
                &format!("fixed-vector field `{comp}.{fname}` with an unsupported element type"),
                "only nonzero-width uint/sint/bits/bool/bit elements up to 64 bits are lowered; nested vectors and record elements are not yet supported",
            ))?;
            let len = match args.get(1) {
                Some(TypeArg::Expr(e)) => match &*e.kind {
                    ExprKind::Int(s) => s.replace('_', "").parse::<usize>().ok(),
                    _ => None,
                },
                _ => None,
            }
            .filter(|n| *n != 0)
            .ok_or_else(|| unsupported(
                &format!("fixed-vector field `{comp}.{fname}` with an invalid length"),
                "the length must be a nonzero decimal compile-time literal",
            ))?;
            Ok(ComponentFieldKind::FixedVec(FixedVecSchema { elem, len }))
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
            // A record type held by value is persistent host-side state,
            // not a DUT handle. This is the component-routed counterpart
            // of `StateFieldKind::Record` on a standalone transactor.
            //
            // `declared_records`, not `record_ids`: by the time
            // components lower, `record_ids` also holds every regblock's
            // MIRROR record under the regblock's own name, and v1's
            // `Emitter::is_record_type` is transactions ∪ structs. Using
            // the contaminated map here would lower a regblock-typed
            // field to a record member, which v1 emits as `VDmaRegs*`.
            if declared_records.contains(simple) {
                let record = record_ids[simple];
                if f.default.is_some() {
                    return Err(unsupported(
                        &format!("a default value on record field `{comp}.{fname}`"),
                        "record fields use their type-derived default initialization",
                    ));
                }
                return Ok(ComponentFieldKind::Record { record });
            }
            // A REGBLOCK-typed field looks like a record to `record_ids`
            // — its mirror record is in there under the regblock's name
            // — but not to v1, whose `is_record_type` is transactions ∪
            // structs. v1 emits `VDmaRegs* r = nullptr;` for it, with
            // only the test DUT's Verilated header included, so the type
            // is undeclared and the output does not compile.
            if record_ids.contains_key(simple) {
                return Err(not_implemented(
                    &format!("a regblock-typed field `{comp}.{fname}` of type `{simple}`"),
                    "a regblock is instantiated by `let <name> : <Regblock> = bind \
                     <helper>`, not held as a component field; v1 emits a `V<Name>*` \
                     member for it and the emitted C++ does not compile",
                    V1Status::EmitsUncompilable,
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
            if h.periodic || event_subscription(h, fields).is_none() {
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
            if h.periodic || event_subscription(h, fields).is_some() {
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
    // A `TSeq<T>` parameter. `RecordSeq` for a record element and
    // `Seq(scalar)` for a scalar one, mirroring how `collect_tseq_records`
    // types a scalar-element tseq result (#453). v1 renders both as
    // `std::vector<T>`; only the element C++ type differs.
    if let Some(seq) = helpers::tseq_ir_type(ty, &ctx.record_ids) {
        return seq;
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
    if let Some(seq) = helpers::tseq_ir_type(ty, record_ids) {
        return seq;
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
    // Leave a naturally completed body unterminated. `FuncBuilder::finish`
    // synthesizes its Return and records that block in `implicit_returns`,
    // distinguishing it from an explicit source `return` for post-hook
    // emission.
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
            kind: ComponentFieldKind::Event { payload },
            activation,
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
    // The SEMANTIC sink checks below. Measured on an INSTANTIATED env,
    // which is the only place v1 looks at the edge at all:
    //
    //   * not `hookable` — v1 emits `for (auto& _s : sink.plain)`, and
    //     a `function` method is not a struct member. g++: "'struct
    //     Sink' has no member named 'plain'".
    //   * neither a method nor an event field — `for (auto& _s :
    //     sink.other)` over a `uint64_t`. g++: "there are no arguments
    //     to 'begin' that depend on a template parameter".
    //   * wrong parameter count / returns a value — v1 raises its OWN
    //     error: "connect: hookable sink `sink.two` must take exactly
    //     one payload argument, got 2" and "connect: hookable sink
    //     `sink.ret` must return void".
    //   * payload mismatch — MIXED, and this is the row a first pass
    //     got wrong. `event_payload_matches_ir_type` compares
    //     signedness and record identity only, so one arm covers two
    //     very different shapes:
    //       - record vs scalar (`event<uint<8>>` into a `Beat` sink) —
    //         the bridge lambda is generic, but converting it to the
    //         source's `std::function<void(uint64_t)>` instantiates it,
    //         so it bites at the wiring line. g++: "no match for call
    //         to '(<lambda(Sink&, Beat)>) (Sink&, long unsigned int&)'".
    //       - SIGNEDNESS only (`event<uint<8>>` into a `sint<8>` sink)
    //         — v1 emits `Sink_write_obs(e.sb, _t)` from a
    //         `void(uint64_t)` channel into an `int64_t` parameter.
    //         That is an implicit conversion, and it COMPILES AND RUNS
    //         CORRECTLY: built and run, count=2 sum=8, exactly what the
    //         program asks for. So v1 implements that program and the
    //         suggestion is honest for it.
    //
    // These are therefore NOT `Invalid`. An earlier pass made them so on
    // the grounds that an uninstantiated env is "v1 not looking, not v1
    // running the program" — but v1 emits, compiles and RUNS such a
    // program to completion, so "a program error under every backend"
    // is false for every one of these arms, and it was the same
    // observation the endpoint-shape arms above cite for keeping their
    // suggestion. One observation cannot reach opposite verdicts in one
    // function.
    //
    // What separates these from the endpoint arms is narrower: an
    // instantiated malformed PATH can still mean something to v1 (a
    // single-segment endpoint resolves against the owner's own
    // hookable), while an instantiated bad SINK never does — except the
    // signedness row, which is split out below precisely because it
    // does.
    let (sink, sink_activation) = if let Some(sm) = sink_comp.method(&sink_name) {
        if !sm.hookable {
            return Err(not_implemented(
                &format!(
                    "a `connect` sink method `{}.{sink_name}` that is not `hookable`",
                    sink_path.join(".")
                ),
                "analysis sinks must be declared `hookable`; v1 emits a fan-out over the \
                 method name as if it were an event vector, which is not a member of the \
                 emitted struct at all",
                V1Status::EmitsUncompilable,
            ));
        }
        if sm.param_names.len() != 1 {
            return Err(not_implemented(
                &format!(
                    "a `connect` sink method `{sink_name}` with {} parameters",
                    sm.param_names.len()
                ),
                "analysis sinks take exactly one payload parameter; v1 refuses it too, \
                 with \"must take exactly one payload argument\"",
                V1Status::Rejects,
            ));
        }
        if sm.has_ret {
            return Err(not_implemented(
                &format!(
                    "a `connect` sink method `{}.{sink_name}` that returns a value",
                    sink_path.join(".")
                ),
                "analysis sinks must not return a value; v1 refuses it too, with \"must \
                 return void\"",
                V1Status::Rejects,
            ));
        }
        if !event_payload_matches_ir_type(src_payload, &sm.param_tys[0]) {
            return Err(connect_payload_mismatch(
                &src_path.join("."),
                &src_event,
                &sink_path.join("."),
                &sink_name,
                both_scalar_payload_and_param(src_payload, &sm.param_tys[0]),
            ));
        }
        (
            crate::ir::ConnectSink::Method { method: sink_name },
            sm.activation,
        )
    } else if let Some(ComponentFieldSchema {
        kind: ComponentFieldKind::Event { payload },
        activation,
        ..
    }) = sink_comp.field(&sink_name)
    {
        if *payload != src_payload {
            return Err(connect_payload_mismatch(
                &src_path.join("."),
                &src_event,
                &sink_path.join("."),
                &sink_name,
                both_scalar_payloads(src_payload, *payload),
            ));
        }
        (
            crate::ir::ConnectSink::Event { event: sink_name },
            *activation,
        )
    } else {
        return Err(not_implemented(
            &format!(
                "a `connect` sink `{}.{sink_name}` that is neither a `hookable` sink \
                 method nor an `event` field",
                sink_path.join(".")
            ),
            "v1 emits a fan-out over the name as if it were an event vector; on a scalar \
             field that does not compile",
            V1Status::EmitsUncompilable,
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

/// A `connect` payload mismatch, split on the one distinction that
/// decides whether v1 runs the program.
///
/// `event_payload_matches_ir_type` compares signedness and record
/// identity, so one check covers two shapes that behave completely
/// differently under v1:
///
///   * both sides SCALAR, differing only in signedness — v1's bridge
///     lambda is generic, and converting it to the source's
///     `std::function<void(uint64_t)>` gives an implicit conversion into
///     the sink's `int64_t` parameter. Built and run: `count=2 sum=8`,
///     exactly what the program asks for. v1 implements it, so the
///     suggestion is honest.
///   * anything else — a RECORD against a scalar in either direction,
///     two DIFFERENT records, or a component-typed sink parameter
///     (`method_schema_ir_type` can produce `IrType::Component`). The
///     same conversion has nothing to convert. g++: "no match for call
///     to '(<lambda(Sink&, Beat)>) (Sink&, long unsigned int&)'". A
///     first version of this comment named only the record-vs-scalar
///     row, which is one of three.
///
/// `scalars` is the caller's answer to "are both sides scalars?", which
/// is the exact discriminator rather than a proxy for it.
fn connect_payload_mismatch(
    src_path: &str,
    src_event: &str,
    sink_path: &str,
    sink_name: &str,
    scalars: bool,
) -> LowerError {
    let construct = format!(
        "a `connect` payload mismatch from `{src_path}.{src_event}` to \
         `{sink_path}.{sink_name}`"
    );
    if scalars {
        unsupported(
            &construct,
            "source and sink scalar payloads must agree in signedness; v1 lets the \
             implicit conversion through and the program runs",
        )
    } else {
        not_implemented(
            &construct,
            "the payload shapes cannot be bridged — a record against a scalar, two \
             different records, or a component-typed parameter; v1 emits the generic \
             bridge anyway and the emitted C++ does not compile",
            V1Status::EmitsUncompilable,
        )
    }
}

/// Whether a `connect` source payload and a sink EVENT payload are both
/// scalars — the discriminator `connect_payload_mismatch` splits on,
/// for the event-sink branch.
fn both_scalar_payloads(src: EventPayload, sink: EventPayload) -> bool {
    matches!(
        (src, sink),
        (EventPayload::Scalar { .. }, EventPayload::Scalar { .. })
    )
}

/// The same discriminator for the METHOD-sink branch, where the sink
/// side is a declared parameter type rather than an event payload.
///
/// Named for what it answers, not for the branch it is called from: it
/// is only ever consulted once the payloads are known NOT to match, so
/// a `true` here means "both scalars, and the mismatch is signedness".
fn both_scalar_payload_and_param(payload: EventPayload, ty: &IrType) -> bool {
    matches!(
        (payload, ty),
        (
            EventPayload::Scalar { .. },
            IrType::UInt(_) | IrType::SInt(_) | IrType::Bool
        )
    )
}

/// The component predicates v1 and TB-IR both implement without them
/// being DECLARED methods, so `comp.method(..).is_none()` is true for
/// them and they reach the "has no method" arms.
///
/// `as_component_idle` lowers the first three in expression position and
/// `quiesced` has its own resolver; a binding position reaches neither,
/// which is a TB-IR gap rather than a bad program. Keeping this list
/// beside those two resolvers is the point — if one grows a predicate,
/// this has to grow with it or a working construct starts being reported
/// as a program error.
///
/// Two callers, and neither of them is `user_override_wins` — an
/// earlier version of this comment said otherwise, and a maintainer
/// following it would have added a fifth name here and found it had no
/// override behaviour. The resolvers match their names inline
/// (`as_component_idle`'s `"idle" | "idle_in" | "idle_out"`,
/// `as_component_quiesced`'s `"quiesced"`), so a new predicate has to
/// be added in BOTH places:
///
///   * here, so the "has no method" arms carve it out, and
///   * in the resolver's own `match`, so it lowers at all.
///
/// The callers are `as_component_method_call`'s two error arms (path
/// form and component-typed-parameter form) and
/// `as_transactor_method_call`'s.
pub(crate) fn is_builtin_component_predicate(name: &str) -> bool {
    matches!(name, "idle" | "idle_in" | "idle_out" | "quiesced")
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
    fold_field_default(
        d,
        Some(ty),
        consts,
        &format!("component field `{comp}.{fname}`"),
    )
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
    /// component and `method` is one of its methods.
    ///
    /// `None` means "not a component-method call" — a callee this
    /// resolver has no claim on, which the caller passes to the next
    /// lowering path.
    ///
    /// Two different errors come out of it, and they are not the same
    /// verdict:
    ///
    ///   * `Invalid` — a receiver that clearly INTENDED a component
    ///     (head segment is a component local) naming a method nothing
    ///     declares. v1 emits `c.nosuch(3)` against a struct with no
    ///     such member and g++ rejects it, so no backend runs it.
    ///   * `Unsupported` — a well-formed call to one of the built-in
    ///     predicates outside expression position. Nothing declares
    ///     those either, so they reach the same arm, but both backends
    ///     implement them; see `is_builtin_component_predicate`.
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
                    if comp.method(&method).is_none() {
                        // "Not a declared method" is NOT the same set as
                        // "does not exist", and a first pass made this
                        // arm `Invalid` on that conflation.
                        //
                        // The built-in component predicates — `idle`,
                        // `idle_in`, `idle_out`, `quiesced` — are not
                        // declared methods and land here. v1 implements
                        // all of them (`emit_idle_predicate`,
                        // `resolve_component_quiesced_predicate`), and
                        // TB-IR implements them too, one statement
                        // position over: `assert c.idle(2)` lowers
                        // through `as_component_idle` and emits, while
                        // `let q = c.idle(2)` came through here and was
                        // told its program was invalid. Both backends
                        // run it; only this seam does not.
                        //
                        // So a built-in in a binding position is a real
                        // TB-IR gap with a working escape hatch, and
                        // anything else is the program error the first
                        // pass measured: v1 emits `uint64_t x =
                        // c.nosuch(3);` against a struct with no such
                        // member — g++: "'struct Calc' has no member
                        // named 'nosuch'".
                        if is_builtin_component_predicate(&method) {
                            // THREE landings share this arm, not the two
                            // an earlier version named: `let q =
                            // c.idle(2)`, `x = c.idle(2)`, and the bare
                            // statement `c.idle(2)`, which has neither a
                            // binding nor a local. So the construct
                            // names the predicate and the detail names
                            // what IS lowered, rather than describing
                            // one landing's syntax as if it were all of
                            // them.
                            return Err(unsupported(
                                &format!(
                                    "the built-in predicate `{}` outside expression position",
                                    path.join(".")
                                ),
                                "TB-IR lowers `idle`/`idle_in`/`idle_out`/`quiesced` where \
                                 their value is USED (`assert c.idle(2)`, `while \
                                 !c.idle_in(4)`), but not as a `let`/assignment right-hand \
                                 side or a bare statement; v1 emits it in all of them",
                            ));
                        }
                        return Err(LowerError::Invalid(format!(
                            "component `{}` has no method `{method}` (in `{}`)",
                            comp.name,
                            path.join(".")
                        )));
                    }
                    // main's activation gate, kept for the case the
                    // block above lets through: a method declared only
                    // inside `when active` is not callable from an
                    // always-on position.
                    let method_schema = comp
                        .method(&method)
                        .expect("the arm above returns for every absent method");
                    self.require_component_activation(
                        &path[0],
                        head_cid,
                        self.binding_mode(&path[0]),
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
                            // Same condition as the path-shaped sibling
                            // above, reached through a component-typed
                            // PARAMETER instead — and reachable: a
                            // `function observe(a: uint<8>, m: Model)`
                            // calling `m.nosuch(a)` lands here directly.
                            // (An earlier note said "not probed, every
                            // source was claimed by the transactor-method
                            // arm first"; that arm only fires for
                            // transactor-typed params.) So this is a
                            // SECOND landing for a missing method, not
                            // the single one an earlier entry claimed.
                            //
                            // Same built-in carve-out, for the same
                            // reason.
                            if is_builtin_component_predicate(&method.name) {
                                return Err(unsupported(
                                    &format!(
                                        "the built-in predicate `{}.{}` on a \
                                         component-typed parameter",
                                        recv.name, method.name
                                    ),
                                    "TB-IR lowers `idle`/`idle_in`/`idle_out`/`quiesced` \
                                     in expression position but not here; v1 emits it \
                                     either way",
                                ));
                            }
                            return Err(LowerError::Invalid(format!(
                                "component `{}` has no method `{}` (on parameter `{}`)",
                                comp.name, method.name, recv.name
                            )));
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
                            if let Some(method_schema) = sub_comp.method(&method.name) {
                                // This arm returned unconditionally, unlike
                                // the path arm above and the self-call arm,
                                // so an active-only method reached through a
                                // `passive` sub-component field emitted its
                                // call: `env Wrap { relay : ModeRelay
                                // passive; hookable kick() relay.activate()
                                // end kick }` emitted
                                // `ModeRelay_activate(self.relay);`. The
                                // receiver is a self-rooted field, so the
                                // mode is the field's own — resolved by
                                // `require_self_sub_activation` rather than
                                // by the test-scope `component_modes` map,
                                // which a component body is not keyed in.
                                self.require_self_sub_activation(
                                    &sub.name,
                                    self_cid,
                                    method_schema.activation,
                                    "method",
                                    &method.name,
                                )?;
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
            self.binding_mode(&recv[0]),
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
        // This is a speculative recognizer, so a non-`Sub` receiver segment
        // means "not a DUT bind" rather than an error. In particular,
        // `src.current.value = ...` traverses record state, not a component
        // receiver; the record-field assignment resolver below must claim it.
        let mut cid = head_cid;
        for seg in &tail[..recv_tail_len] {
            let comp = &self.ctx.components[cid.index()];
            let Some(ComponentFieldKind::Sub { component, .. }) = comp.field(seg).map(|f| &f.kind)
            else {
                return Ok(false);
            };
            cid = *component;
        }
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

    /// Resolve a subfield of a record-valued component field. The IR's
    /// component member access stores the validated C++ member suffix as
    /// one string (`current.value`); emission already renders it after the
    /// resolved component base.
    fn as_component_record_field(
        &self,
        e: &AstExpr,
    ) -> Result<Option<(ComponentBase, String)>, LowerError> {
        let Some(path) = dotted_path(e) else {
            return Ok(None);
        };
        let path = self.strip_tb_prefix(&path);
        if path.len() < 2 {
            return Ok(None);
        }

        let validate = |record: RecordId, sub: &[String]| -> Result<(), LowerError> {
            let mut rid = record;
            for (i, seg) in sub.iter().enumerate() {
                let schema = &self.ctx.records[rid.index()];
                let Some(field) = schema.field(seg) else {
                    return Err(LowerError::Invalid(format!(
                        "record `{}` has no field `{seg}`",
                        schema.name
                    )));
                };
                if i + 1 < sub.len() {
                    match field.ty {
                        IrType::Record(next) if field.vec_len.is_none() => rid = next,
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
                } else if field.vec_len.is_some() {
                    return Err(unsupported(
                        &format!("a whole-`Vec` component record field `{}`", path.join(".")),
                        "indexed component-record `Vec` access is not lowered by TB-IR yet",
                    ));
                }
            }
            Ok(())
        };

        // Method-body form: `current.value`, where `current` is a field
        // on the implicit `self` component.
        if self.lookup(&path[0]).is_none() {
            if let Some(cid) = self.self_component {
                let comp = &self.ctx.components[cid.index()];
                if let Some(schema) = comp.field(&path[0]) {
                    if let ComponentFieldKind::Record { record } = schema.kind {
                        self.require_self_activation(schema.activation, "field", &path[0])?;
                        validate(record, &path[1..])?;
                        return Ok(Some((ComponentBase::SelfField, path.join("."))));
                    }
                }
            }
        }

        // Test/nested-component form: `env.source.current.value`. Walk
        // through zero or more `Sub` fields until the record field is
        // reached, then validate the remainder against its record schema.
        let Some((head_cid, mut base, tail, head_mode)) = self.component_path_head(&path) else {
            return Ok(None);
        };
        for recv_len in 0..tail.len() {
            let Ok(cid) = self.resolve_component_recv(head_cid, &tail[..recv_len]) else {
                break;
            };
            let comp = &self.ctx.components[cid.index()];
            let Some(schema) = comp.field(&tail[recv_len]) else {
                continue;
            };
            let ComponentFieldKind::Record { record } = schema.kind else {
                continue;
            };
            let sub = &tail[recv_len + 1..];
            if sub.is_empty() {
                return Ok(None);
            }
            self.require_component_activation(
                &path[0],
                head_cid,
                head_mode,
                &tail[..recv_len],
                schema.activation,
                "field",
                &tail[recv_len],
            )?;
            validate(record, sub)?;
            base.extend_from_slice(&tail[..recv_len]);
            return Ok(Some((
                ComponentBase::Path(base),
                tail[recv_len..].join("."),
            )));
        }
        Ok(None)
    }

    pub(crate) fn as_component_field_target(
        &self,
        target: &AstExpr,
    ) -> Result<Option<(ComponentBase, String)>, LowerError> {
        if let Some(record_field) = self.as_component_record_field(target)? {
            return Ok(Some(record_field));
        }
        // Self-relative bare field (only inside a method body, and only
        // when the name is NOT a shadowing local).
        if let ExprKind::Ident(id) = &*target.kind {
            if self.lookup(&id.name).is_none() {
                if let Some(cid) = self.self_component {
                    let comp = &self.ctx.components[cid.index()];
                    if let Some(field) = comp.field(&id.name) {
                        // A whole-RECORD field is written the same way —
                        // v1 emits `self.cur = b;` and g++ accepts it.
                        // Accepting only `Scalar` here sent `cur = b`
                        // down to "assignment to unknown name `cur`",
                        // labelled `EmitsUncompilable`, which was wrong
                        // on both halves: `cur` is a declared field of
                        // this very component, and v1 compiles the file.
                        if matches!(
                            field.kind,
                            ComponentFieldKind::Scalar { .. } | ComponentFieldKind::Record { .. }
                        ) {
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
                if let Some((head_cid, base_head, tail, head_mode)) =
                    self.component_path_head(&path)
                {
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
                        // A whole-RECORD component field takes the same
                        // path: v1 emits `_tb.s.cur = y;` for a
                        // same-typed RHS and g++ accepts it. The caller
                        // type-checks the RHS against the field's record
                        // (`lower_assign`), so a mismatched one is a
                        // precise `Invalid` rather than this arm's
                        // blanket "not a scalar component field", which
                        // promised `--codegen v1` for `s.cur = 5` —
                        // "no match for 'operator=', operand types are
                        // 'Beat' and 'int'".
                        Some(schema)
                            if matches!(
                                schema.kind,
                                ComponentFieldKind::Scalar { .. }
                                    | ComponentFieldKind::Record { .. }
                            ) =>
                        {
                            self.require_component_activation(
                                &path[0],
                                head_cid,
                                head_mode,
                                recv_tail,
                                schema.activation,
                                "field",
                                &field,
                            )?;
                            let mut base = base_head;
                            base.extend_from_slice(recv_tail);
                            return Ok(Some((ComponentBase::Path(base), field)));
                        }
                        // A whole sub-component copy is resolved after field
                        // writes. Decline here so `checker.sb = sb` reaches
                        // that dedicated, type-checking resolver.
                        Some(schema) if matches!(schema.kind, ComponentFieldKind::Sub { .. }) => {
                            return Ok(None);
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

    /// Resolve a component scalar or whole-record field READ as an
    /// `Expr::ComponentField`: bare `count` / `current` (self), or paths
    /// `env.sb.count` / `env.source.current`.
    pub(crate) fn as_component_field_read(
        &self,
        e: &AstExpr,
    ) -> Result<Option<IrExpr>, LowerError> {
        if let Some((base, field)) = self.as_component_record_field(e)? {
            return Ok(Some(IrExpr::ComponentField { base, field }));
        }
        if let ExprKind::Ident(id) = &*e.kind {
            if self.lookup(&id.name).is_none() {
                if let Some(cid) = self.self_component {
                    let comp = &self.ctx.components[cid.index()];
                    if let Some(field) = comp.field(&id.name) {
                        if matches!(
                            field.kind,
                            ComponentFieldKind::Scalar { .. } | ComponentFieldKind::Record { .. }
                        ) {
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
                if let Some((head_cid, base_head, tail, head_mode)) =
                    self.component_path_head(&path)
                {
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
                        if matches!(
                            schema.kind,
                            ComponentFieldKind::Scalar { .. } | ComponentFieldKind::Record { .. }
                        ) {
                            self.require_component_activation(
                                &path[0],
                                head_cid,
                                head_mode,
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

    /// Resolve the unindexed field portion of a direct component `Vec` access.
    pub(crate) fn as_component_vec_field(
        &self,
        e: &AstExpr,
    ) -> Result<Option<(ComponentBase, String, FixedVecSchema)>, LowerError> {
        if let ExprKind::Ident(id) = &*e.kind {
            if self.lookup(&id.name).is_none() {
                if let Some(cid) = self.self_component {
                    if let Some(schema) = self.ctx.components[cid.index()].field(&id.name) {
                        if let ComponentFieldKind::FixedVec(vec) = &schema.kind {
                            self.require_self_activation(schema.activation, "field", &id.name)?;
                            return Ok(Some((
                                ComponentBase::SelfField,
                                id.name.clone(),
                                vec.clone(),
                            )));
                        }
                    }
                }
            }
        }
        if let Some(path) = dotted_path(e) {
            let path = self.strip_tb_prefix(&path);
            if path.len() >= 2 {
                if let Some((head_cid, mut base, tail, head_mode)) =
                    self.component_path_head(&path)
                {
                    let (recv_tail, last) = tail.split_at(tail.len() - 1);
                    if !self.recv_is_scoreboard_sub(head_cid, recv_tail) {
                        let cid = self.resolve_component_recv(head_cid, recv_tail)?;
                        if let Some(schema) = self.ctx.components[cid.index()].field(&last[0]) {
                            if let ComponentFieldKind::FixedVec(vec) = &schema.kind {
                                self.require_component_activation(
                                    &path[0], head_cid, head_mode, recv_tail,
                                    schema.activation, "field", &last[0],
                                )?;
                                base.extend_from_slice(recv_tail);
                                return Ok(Some((ComponentBase::Path(base), last[0].clone(), vec.clone())));
                            }
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
    pub(crate) fn resolve_component_recv(
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
        inherited: Option<ComponentInstanceMode>,
        segs: &[String],
    ) -> Result<Option<ComponentInstanceMode>, LowerError> {
        // The mode context in force AT the head is the caller's to supply:
        // a test-scope binding reads it out of `component_modes` by name,
        // while a self-relative head (`relay.acalls` inside the env that
        // declares `relay : ModeRelay active`) carries it on the `Sub`
        // field itself. Deriving it here from `head_name` got the second
        // case wrong twice over — `component_path_head` hands back the
        // literal `"self"`, which is in no binding map, so the lookup
        // missed and a guard on the miss then rejected the access.
        //
        // That guard is gone with it. It fired whenever a transactor head
        // had no mode of its own, which is legal for a head that does not
        // require one (`mon : Mon`, a reactive monitor) and whose mode for
        // THIS access comes from a descendant binding further down the
        // path (`sub : Relay active`). The post-walk check below is the
        // honest form of the same question: it asks whether the resolved
        // TARGET requires a mode and lacks one.
        let resolved =
            crate::ir::resolve_component_path_mode(&self.ctx.components, head, inherited, segs)
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
        Ok(
            matches!(target.kind, crate::ir::ComponentKindTag::Transactor)
                .then_some(resolved.effective_mode)
                .flatten(),
        )
    }

    /// The mode context a TEST-SCOPE binding name carries (`env.mon.x`,
    /// rooted at the `env` local). A self-relative head is not keyed here
    /// — see `component_path_head`.
    fn binding_mode(&self, head_name: &str) -> Option<ComponentInstanceMode> {
        self.ctx.component_modes.get(head_name).copied().flatten()
    }

    fn require_component_activation(
        &self,
        head_name: &str,
        head: ComponentId,
        inherited: Option<ComponentInstanceMode>,
        segs: &[String],
        activation: Activation,
        member_kind: &str,
        member: &str,
    ) -> Result<(), LowerError> {
        if matches!(activation, Activation::Always) {
            return Ok(());
        }
        let mode = self.resolve_component_mode(head_name, head, inherited, segs)?;
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

    /// Activation check for a member reached through a SELF sub-component
    /// field (`relay.activate()` inside the env that declares `relay`).
    ///
    /// Only a field declared `passive` is rejected here. A component body
    /// is lowered ONCE and shared by every instance of its type, so the
    /// mode of a field that declares none is whatever its holder was
    /// bound as — `env Wrap { relay : ModeRelay }` is `active` under `let
    /// wrap : Wrap active` and passive under another binding, and this
    /// seam sees neither. A declared `passive` is decidable because a
    /// declared mode WINS over the inherited one (`resolve_component_-
    /// path_mode`: `declared.or(inherited)`), so it is passive at every
    /// binding.
    ///
    /// Reading the declared mode as the whole context is what an earlier
    /// version of this check did, and it rejected the mode-less field
    /// above — reporting it as a "passive transactor" when it had no mode
    /// at all — while the same call spelled `wrap.relay.activate()` was
    /// still accepted. The inherited case stays uncaught here; catching
    /// it needs per-instance body specialization, which is also why the
    /// path form resolves it and this one cannot.
    fn require_self_sub_activation(
        &self,
        field: &str,
        self_cid: ComponentId,
        activation: Activation,
        member_kind: &str,
        member: &str,
    ) -> Result<(), LowerError> {
        if matches!(activation, Activation::Always) {
            return Ok(());
        }
        let mode = match self.ctx.components[self_cid.index()]
            .field(field)
            .map(|f| &f.kind)
        {
            Some(ComponentFieldKind::Sub { mode, .. }) => *mode,
            _ => None,
        };
        if matches!(mode, Some(ComponentInstanceMode::Passive)) {
            return Err(LowerError::Invalid(format!(
                "active-only {member_kind} `{member}` is used through passive transactor `{field}`"
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
    ///
    /// The fourth element is the mode context in force at the returned
    /// head — read from the test-scope binding for the first shape, and
    /// from the `Sub` field's own declared mode for the second. It is
    /// returned rather than re-derived from the base segments because
    /// those start with the literal `"self"` in the second shape, which
    /// names no binding.
    fn component_path_head<'a>(
        &self,
        path: &'a [String],
    ) -> Option<(
        ComponentId,
        Vec<String>,
        &'a [String],
        Option<ComponentInstanceMode>,
    )> {
        if let Some(&head_cid) = self.ctx.component_fields.get(&path[0]) {
            return Some((
                head_cid,
                vec![path[0].clone()],
                &path[1..],
                self.binding_mode(&path[0]),
            ));
        }
        let self_cid = self.self_component?;
        let comp = &self.ctx.components[self_cid.index()];
        match comp.field(&path[0]).map(|f| &f.kind) {
            Some(ComponentFieldKind::Sub { component, mode }) => Some((
                *component,
                vec!["self".to_string(), path[0].clone()],
                &path[1..],
                *mode,
            )),
            _ => None,
        }
    }

    /// Lower the args of a component method call (port-hoisted, like any
    /// host-side call).
    /// `declared` is the callee's parameter names when the caller
    /// resolved a method schema, and `None` for the `emit <ev>(...)`
    /// payload callers — an event payload has no parameter list to
    /// check against, and inventing one is the mistake `record_write`
    /// already made.
    pub(crate) fn lower_component_call_args(
        &mut self,
        args: &[CallArg],
        declared: Option<&[String]>,
    ) -> Result<Vec<IrExpr>, LowerError> {
        if let Some(declared) = declared {
            // With the parameter list in hand the three cases split:
            // a name in its own position is inert (v1 drops names and
            // binds by position, so it emits exactly the positional
            // call), a name elsewhere silently swaps, and a name
            // matching nothing is a program error. The arity-based
            // approximation below is what this replaces — it could only
            // ask "is there more than one argument?", so it refused the
            // inert form too.
            super::reject_misplaced_named_args(args, declared, "a component method call")?;
            // `lower_expr_no_ports`, matching the positional path below
            // exactly. An earlier draft used `lower_expr` here and the
            // verifier caught it: `PortInDisallowedPosition` on a
            // `ComponentCall arg`. A named argument must lower through
            // the same seam as a positional one, or "the name is inert"
            // stops being true.
            let mut out = Vec::with_capacity(args.len());
            for a in args {
                let (CallArg::Expr(e) | CallArg::Named { value: e, .. }) = a;
                out.push(self.lower_expr_no_ports(e)?);
            }
            return Ok(out);
        }
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
                // call under v1, and naming the argument is not what
                // caused that — so the wording below describes the name
                // without pretending the call is well-formed.
                //
                // This used to add "…but so does the positional
                // `axil_write(t.value)`, which tbir lowers and verifies
                // clean". It no longer does: the component-method arity
                // check added later in the same sweep rejects it. The
                // point the sentence was making does not depend on it.
                //
                // Reached ONLY when the caller had no parameter list to
                // pass — the `emit <ev>(...)` payload callers. An event
                // payload has no declared names to check a written name
                // against, so the arity split below is the best this
                // seam can do.
                //
                // Method calls no longer land here: they pass
                // `declared` and take the `reject_misplaced_named_args`
                // path at the top, which splits in-order (inert, and it
                // lowers) from reordered (a silent swap) from a name
                // matching nothing (a program error). This comment used
                // to say arity ≥ 2 could not be split because "telling
                // the two apart needs the callee's parameter list" —
                // true at the time, and the list is now carried in
                // `ComponentMethodSchema::param_names`.
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
                // "component call", not "method call": every method
                // caller now passes `declared` and takes the guarded
                // path above, so this is reached ONLY from the
                // `emit <ev>(...)` payload callers. The single-argument
                // arm was already worded that way for the same reason;
                // this one said "method call" and was measured saying
                // it about `emit tagger.in_ev(a = 1, b = 2)`.
                return Err(not_implemented(
                    "named arguments in a component call",
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
                            self.binding_mode(&head),
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
                // The arity check the test-scope `let e : event<T>`
                // branch below has had all along, missing here and at
                // the self-relative site. Measured at both, on both
                // backends: `emit tagger.in_ev(i + 1, 2)` makes TB-IR
                // emit `_s((i + 1), 2)` against a
                // `std::function<void(uint64_t)>` — g++: "no match for
                // call to '(std::function<void(long unsigned int)>)
                // (int, int)'" — while v1 emits `_s(i + 1)`, silently
                // dropping the extra payload. Under-supply is
                // uncompilable under both. So no backend runs the
                // program as written, and `Invalid` is the verdict the
                // sibling branch already gives it.
                //
                // `Invalid` refuses outright, so the arity rule was
                // checked against v1 rather than read off the spec.
                // Both `in_ev : event` (no type argument) and
                // `in_ev : event<uint<8>, uint<8>>` pass `harc check`,
                // and v1 emits `std::function<void(uint64_t)>` for
                // both: one payload slot however the field is spelled.
                // There is no other arity to be right about.
                if args.len() != 1 {
                    return Err(LowerError::Invalid(format!(
                        "`emit {}` carries {} argument(s); an event payload is exactly one",
                        path_str(name),
                        args.len()
                    )));
                }
                let lowered = self.lower_component_call_args(args, None)?;
                // The payload TYPE, not just the arity. The channel
                // renders as `std::function<void(uint64_t)>` or
                // `std::function<void(<Record>)>`, and passing one where
                // the other is expected is a hard C++ error in both
                // backends — measured on all four combinations.
                if let Some(payload) = self.component_event_payload(cid, &event) {
                    self.check_slot_type(
                        &lowered[0],
                        match payload {
                            EventPayload::Record(r) => Some(r),
                            EventPayload::Scalar { .. } => None,
                        },
                        &format!("event `{event}`"),
                    )?;
                }
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
                    let lowered = self.lower_component_call_args(args, None)?;
                    // Shape must agree: the channel renders as
                    // `std::function<void(uint64_t)>` or
                    // `std::function<void(<Record>)>`, and passing one
                    // where the other is expected is a hard C++ error.
                    // Signedness is left alone — both backends widen a
                    // scalar payload to a 64-bit slot, so `sint` into an
                    // `event<uint<8>>` is the same benign conversion v1
                    // performs.
                    // `record_id_of_expr`, not `expr_type`: the latter
                    // has no arm for a record-valued component field,
                    // transactor-state field or ternary, and this match
                    // waved `Unknown` straight through — so exactly the
                    // shapes divergence 108 taught the compiler to type
                    // were the ones this check could not see.
                    let got = self.record_id_of_expr(&lowered[0]);
                    let shape_ok = match payload {
                        EventPayload::Record(want) => got == Some(want),
                        EventPayload::Scalar { .. } => got.is_none(),
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
        // Same missing arity check as the path form above; see the
        // measurement there.
        if args.len() != 1 {
            return Err(LowerError::Invalid(format!(
                "`emit {event}` carries {} argument(s); an event payload is exactly one",
                args.len()
            )));
        }
        let lowered = self.lower_component_call_args(args, None)?;
        if let Some(payload) = self.component_event_payload(cid, &event) {
            self.check_slot_type(
                &lowered[0],
                match payload {
                    EventPayload::Record(r) => Some(r),
                    EventPayload::Scalar { .. } => None,
                },
                &format!("event `{event}`"),
            )?;
        }
        self.push(IrStmt::ComponentEmit {
            base: ComponentBase::SelfField,
            event,
            args: lowered,
        });
        Ok(())
    }

    /// The payload an `event` field of `cid` carries — `Record` or
    /// `Scalar`.
    ///
    /// The `None` results are UNREACHABLE from both callers, and kept
    /// only so the condition has one verdict everywhere it is written
    /// (the same reason the record-typed `let` arm above keeps its
    /// no-such-method check). Each caller has already matched the field
    /// against `ComponentFieldKind::Event` and returned `Err` otherwise
    /// — the path form a few lines up, and the self-relative form
    /// through its `match comp.field(&event)` — so by the time this runs
    /// the field exists and is an event. Confirmed by mutation: making
    /// the non-event arm `panic!` fails no test.
    ///
    /// An earlier version of this comment said `Err`, and the version
    /// after that explained what `None` means for the caller as though
    /// it were a live branch. Neither described the code.
    fn component_event_payload(&self, cid: ComponentId, event: &str) -> Option<EventPayload> {
        match self.ctx.components.get(cid.index())?.field(event)?.kind {
            ComponentFieldKind::Event { payload, .. } => Some(payload),
            _ => None,
        }
    }

    /// Whether the receiver's own declaration beats the built-in
    /// predicate of the same name. The built-ins are a DEFAULT, not a
    /// reserved word: a component that declares `hookable idle(n)`
    /// means its own method, and v1 has said so since before TB-IR
    /// existed — both `resolve_component_idle_predicate` and
    /// `resolve_component_quiesced_predicate` return `None` on a
    /// declared hookable of the same name — `resolve_component_idle_predicate`
    /// naming the shipped `buf_mgr_test` fixture as the reason, and
    /// `resolve_component_quiesced_predicate` doing it through
    /// `component_has_hookable`.
    ///
    /// TB-IR had no such guard, and `as_component_idle` runs BEFORE
    /// component-method resolution, so the heartbeat won every time.
    /// Measured on an `agent Calc` with `hookable idle(n) -> uint<32>`
    /// returning 7, against `assert c.idle(2) == 7`:
    ///
    /// * v1 — `if (!(Calc_idle(c, 2) == 7))`: the method runs, the
    ///   assertion holds.
    /// * TB-IR, before this guard — `if (!((((cycle_count -
    ///   c._last_in_cycle) >= 2) && ((cycle_count - c._last_out_cycle)
    ///   >= 2)) == 7))`: the heartbeat runs, the assertion fails.
    ///
    /// Both compile and run, and they disagree — the worst shape a
    /// divergence takes, and the reason this is a fix rather than a
    /// classification.
    fn user_override_wins(&self, cid: ComponentId, method: &str) -> bool {
        self.ctx.components[cid.index()].method(method).is_some()
    }

    /// Lower `<comp>.idle(N)` / `.idle_in(N)` / `.idle_out(N)` to an
    /// `Expr::ComponentIdle` when the callee resolves to a component
    /// instance path. Returns `None` when the callee is not an idle
    /// predicate on a known component (caller falls through to other
    /// call-lowering paths), and — see `user_override_wins` — when the
    /// receiver's component declares a method of that name itself.
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
        let recv_cid = self.resolve_component_recv(head_cid, &path[1..])?;
        if self.user_override_wins(recv_cid, &name.name) {
            return Ok(None);
        }
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
            // Deliberately NOT name-checked, unlike `record_read`, which
            // was given a hand-written `["addr"]` so that an unknown
            // name becomes `Invalid`. The difference is whether there
            // is a name to check AGAINST: the compiler's own arity
            // message here says "exactly one cycle-count argument" and
            // the docs write `idle(N)`, where `N` is a value
            // placeholder — no parameter name is stated anywhere.
            // `record_read`'s `addr` came from the compiler's own
            // diagnostic AND the docs. Inventing one here is the
            // `record_write` mistake, so this stays as `bitbash` does.
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
        if self.user_override_wins(recv_cid, &name.name) {
            return Ok(None);
        }
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
