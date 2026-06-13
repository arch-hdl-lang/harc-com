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

use super::{FuncBuilder, LowerCtx, LowerError, helpers, unsupported};
use crate::ast::{
    BuiltinTy, ComponentDecl, ComponentField, ComponentItem, ConnectEdge, Direction, ExprKind,
    HookableMethod, TransactorDecl, TypeArg, TypeExpr,
};
use crate::ir::{
    ComponentFieldKind, ComponentFieldSchema, ComponentId, ComponentKindTag,
    ComponentMethodSchema, ComponentSchema, ConnectEdgeSchema, EventPayload, FunctionId,
    FunctionKind, IrType, RecordId, ScoreboardId, TbFunction, Terminator, TypedParam,
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
/// A `bound to` target responder is excluded (separate TLM path).
pub(crate) fn transactor_is_component(t: &TransactorDecl) -> bool {
    if t.bound_to.is_some() {
        return false;
    }
    let mut has_event = false;
    let mut has_in_event = false;
    let mut has_on_handler = false;
    let mut has_module_field = false;
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
            _ => {}
        }
    }
    // Pure analysis source: events, no DUT.
    if has_event && !has_module_field {
        return true;
    }
    // Event-driven consumer transactor: an `in event` + a subscribing
    // `on` handler (the DUT field, if any, is the handler's poke target).
    has_in_event && has_on_handler
}

/// True when a transactor is an *event-driven consumer* — it has an
/// `in event<T>` field and a subscribing `on <ev>` handler. These accept
/// an `active`/`passive` instance mode (a transactor concept) even though
/// they route to the composite-component table; a pure analysis source
/// (out-event only, no DUT, no `on`) does not.
pub(crate) fn transactor_is_event_driven(t: &TransactorDecl) -> bool {
    if t.bound_to.is_some() {
        return false;
    }
    let mut has_in_event = false;
    let mut has_on_handler = false;
    for it in t.items.iter().chain(t.when_active.iter().flatten()) {
        match it {
            ComponentItem::Field(f)
                if is_event_field(f)
                    && matches!(f.direction, Some(crate::ast::Direction::In)) =>
            {
                has_in_event = true;
            }
            ComponentItem::OnHandler(h) if !h.periodic => has_on_handler = true,
            _ => {}
        }
    }
    has_in_event && has_on_handler
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
) -> Result<ComponentSchema, LowerError> {
    let (name, kind, items, when_active): (
        &str,
        ComponentKindTag,
        &[ComponentItem],
        Option<&[ComponentItem]>,
    ) = match src {
        CompSource::Env(c) => (&c.name.name, ComponentKindTag::Env, &c.items, None),
        CompSource::Agent(c) => (&c.name.name, ComponentKindTag::Agent, &c.items, None),
        CompSource::Sequencer(c) => {
            (&c.name.name, ComponentKindTag::Sequencer, &c.items, None)
        }
        CompSource::Scoreboard(c) => {
            (&c.name.name, ComponentKindTag::Scoreboard, &c.items, None)
        }
        CompSource::Transactor(t) => (
            &t.name.name,
            ComponentKindTag::Transactor,
            &t.items,
            t.when_active.as_deref(),
        ),
    };
    if let CompSource::Transactor(t) = src {
        if !t.params.is_empty() {
            return Err(unsupported(
                &format!("generic parameters on analysis-source `{name}`"),
                "",
            ));
        }
    }
    if let CompSource::Env(c)
    | CompSource::Scoreboard(c)
    | CompSource::Agent(c)
    | CompSource::Sequencer(c) = src
    {
        if !c.params.is_empty() {
            return Err(unsupported(&format!("parameters on `{name}`"), ""));
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
    let mut on_asts: Vec<&crate::ast::OnHandler> = Vec::new();
    let mut periodic_asts: Vec<&crate::ast::OnHandler> = Vec::new();
    // Cycle-trigger handlers (`on <bool-expr> ... end on`) — the monitor
    // half. Distinguished from event-subscription `on ev(arg)` by the
    // trigger expression shape (a non-`Call`, or a `Call` whose callee is
    // not a self-event field). Resolved after the field loop.
    let mut cycle_asts: Vec<&crate::ast::OnHandler> = Vec::new();
    // At most one `watchdog` per component (the second is rejected).
    let mut watchdog_ast: Option<&crate::ast::WatchdogDecl> = None;
    // An event-driven transactor may carry a module-typed DUT handle field
    // (`dut : AxiLiteRegs`); a Named non-component type is then a DUT
    // pointer rather than an unknown sub-component. Only transactors host
    // a DUT field — env/scoreboard/agent/sequencer never do.
    let is_transactor = matches!(src, CompSource::Transactor(_));
    for it in items.iter().chain(when_active.into_iter().flatten()) {
        match it {
            ComponentItem::Field(f) => {
                let fk = lower_field(name, f, ids, scoreboard_ids, record_ids, is_transactor)?;
                if fields.iter().any(|x| x.name == f.name.name) {
                    return Err(LowerError::Invalid(format!(
                        "component `{name}` declares field `{}` more than once",
                        f.name.name
                    )));
                }
                fields.push(ComponentFieldSchema {
                    name: f.name.name.clone(),
                    kind: fk,
                });
            }
            ComponentItem::Hookable(h) => {
                let n_params = h.params.len();
                let has_ret = h.return_ty.is_some();
                let fid = FunctionId(*next_fn);
                *next_fn += 1;
                methods.push(ComponentMethodSchema {
                    name: h.name.name.clone(),
                    function: fid,
                    n_params,
                    has_ret,
                    hookable: h.is_hookable,
                });
            }
            // Connect blocks are resolved separately (env-binding stage).
            ComponentItem::Connect(_) => {}
            ComponentItem::Lifecycle(..) => {}
            ComponentItem::OnHandler(h) if h.periodic => periodic_asts.push(h),
            ComponentItem::OnHandler(h) => on_asts.push(h),
            ComponentItem::Watchdog(w) => {
                if watchdog_ast.is_some() {
                    return Err(unsupported(
                        &format!("a second `watchdog` on `{name}`"),
                        "a component may declare at most one `watchdog`",
                    ));
                }
                watchdog_ast = Some(w);
            }

            ComponentItem::TargetTlmThread(_) | ComponentItem::Apply(_) => {
                return Err(unsupported(
                    &format!("an unsupported item in component `{name}`"),
                    "",
                ));
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
    for h in &on_asts {
        if is_event_subscription(h, &fields) {
            let (event, arg_payload) = resolve_on_handler_event(name, h, &fields)?;
            let fid = FunctionId(*next_fn);
            *next_fn += 1;
            on_handlers.push(crate::ir::OnHandlerSchema {
                event,
                arg_payload,
                function: fid,
            });
        } else {
            cycle_asts.push(h);
        }
    }

    // Periodic handlers (`on <N> cycles`). Reserved AFTER the event
    // handlers so the FunctionId blocks stay contiguous in pass-2
    // declaration order (methods → event on-handlers → periodic → watchdog).
    // The period expression lowers in pass 2 (it may reference component
    // fields); pass 1 records a placeholder.
    let mut periodic_handlers: Vec<crate::ir::PeriodicHandlerSchema> = Vec::new();
    for h in &periodic_asts {
        validate_periodic_handler(name, h)?;
        let fid = FunctionId(*next_fn);
        *next_fn += 1;
        periodic_handlers.push(crate::ir::PeriodicHandlerSchema {
            period: crate::ir::Expr::CycleCount, // placeholder; pass 2 fills it
            function: fid,
        });
    }

    // Cycle-trigger handlers (`on <bool-expr>`). Reserved AFTER periodic
    // and BEFORE the watchdog so the FunctionId blocks stay contiguous in
    // pass-2 declaration order (methods → event on-handlers → periodic →
    // cycle-trigger → watchdog). The trigger predicate lowers in pass 2
    // (it reads DUT/component fields); pass 1 records a placeholder.
    let mut cycle_handlers: Vec<crate::ir::CycleTriggerHandlerSchema> = Vec::new();
    for h in &cycle_asts {
        validate_cycle_handler(name, h)?;
        let fid = FunctionId(*next_fn);
        *next_fn += 1;
        cycle_handlers.push(crate::ir::CycleTriggerHandlerSchema {
            trigger: crate::ir::Expr::CycleCount, // placeholder; pass 2 fills it
            edge: edge_to_ir(h.edge),
            function: fid,
        });
    }

    // Watchdog (at most one). A `disabled` watchdog emits nothing — no
    // FunctionId, no schema entry (mirrors v1's `emit_watchdog` early
    // return). The body FunctionId is reserved LAST; period/max_idle
    // lower in pass 2.
    let watchdog = match watchdog_ast {
        Some(w) if !w.disabled => {
            let fid = FunctionId(*next_fn);
            *next_fn += 1;
            Some(crate::ir::WatchdogSchema {
                period: None,   // pass 2 fills from `w.period`
                max_idle: None, // pass 2 fills from `w.max_idle`
                function: fid,
            })
        }
        _ => None,
    };

    Ok(ComponentSchema {
        name: name.to_string(),
        kind,
        fields,
        methods,
        // Connects resolved in a third pass once all schemas exist.
        connects: Vec::new(),
        on_handlers,
        periodic_handlers,
        cycle_handlers,
        watchdog,
    })
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
        return Err(unsupported(
            &format!("a `pre`/`post` hook on a cycle-trigger `on` handler on `{comp}`"),
            "cycle-trigger handlers take no hook side",
        ));
    }
    if !matches!(h.phase, crate::ast::OnPhase::Checker) {
        return Err(unsupported(
            &format!("a non-default-phase cycle-trigger `on` handler on `{comp}`"),
            "only the default (checker) phase is lowered for cycle-trigger handlers",
        ));
    }
    Ok(())
}

/// Validate an `on <N> cycles ... end on` periodic handler: it must carry
/// no `pre`/`post` hook side and (in this component subset) the default
/// `Checker` phase. A `post_eval`-phased periodic handler is a transactor/
/// monitor reactive form handled by the transactor lowering, not the
/// agent-component path here.
fn validate_periodic_handler(
    comp: &str,
    h: &crate::ast::OnHandler,
) -> Result<(), LowerError> {
    if h.hook.is_some() {
        return Err(unsupported(
            &format!("a `pre`/`post` hook on an `on <N> cycles` handler on `{comp}`"),
            "periodic handlers take no hook side",
        ));
    }
    if !matches!(h.phase, crate::ast::OnPhase::Checker) {
        return Err(unsupported(
            &format!("a non-default-phase `on <N> cycles` handler on `{comp}`"),
            "only the default (checker) phase is lowered for component periodic handlers",
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
        return Err(unsupported(
            &format!("a `pre`/`post` hook `on` handler on `{comp}`"),
            "only bare `on <event>(arg)` self-subscriptions are lowered",
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

fn lower_field(
    comp: &str,
    f: &ComponentField,
    ids: &HashMap<String, ComponentId>,
    scoreboard_ids: &HashMap<String, ScoreboardId>,
    record_ids: &HashMap<String, RecordId>,
    is_transactor: bool,
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
            let signed = match args.first() {
                Some(TypeArg::Type(ty)) => match scalar_ir_type(ty) {
                    Some(IrType::SInt(_)) => true,
                    Some(IrType::UInt(_)) | Some(IrType::Bool) => false,
                    _ => {
                        return Err(unsupported(
                            &format!("a non-scalar queue element on `{comp}.{fname}`"),
                            "",
                        ));
                    }
                },
                _ => false,
            };
            Ok(ComponentFieldKind::Queue { signed })
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
            let default = scalar_default(&f.default, comp, fname)?;
            Ok(ComponentFieldKind::Scalar { ty, default })
        }
        TypeExpr::Named { name, .. } => {
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
                return Ok(ComponentFieldKind::ScoreboardSub { scoreboard: *sid });
            }
            // A known component type is a nested sub-component (`source :
            // AnalysisSource`).
            if let Some(cid) = ids.get(simple) {
                return Ok(ComponentFieldKind::Sub { component: *cid });
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
    constraint_sites: &std::cell::RefCell<Vec<crate::ir::ConstraintSite>>,
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
            funcs.push(lower_method_body(h, m.function, cid, ctx, helpers, constraint_sites)?);
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
            funcs.push(lower_on_handler_body(h, oh, cid, ctx, helpers, constraint_sites)?);
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
            let (body, period) =
                lower_periodic_body(h, ph.function, cid, ctx, helpers, constraint_sites)?;
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
            let (body, trigger) =
                lower_cycle_body(h, ch.function, cid, ctx, helpers, constraint_sites)?;
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
                let (body, period, max_idle) =
                    lower_watchdog_body(w, ws.function, cid, ctx, helpers, constraint_sites)?;
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
    cid: ComponentId,
    ctx: &LowerCtx,
    helpers: &helpers::HelperRegistry<'_>,
    constraint_sites: &std::cell::RefCell<Vec<crate::ir::ConstraintSite>>,
) -> Result<(TbFunction, crate::ir::Expr), LowerError> {
    let mut b = FuncBuilder::new(ctx, helpers, constraint_sites);
    b.self_component = Some(cid);
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

/// Lower an `on <N> cycles ... end on` periodic-handler body as a zero-
/// param `ComponentMethod` (`self` only) and lower its period expression
/// in the same self-component context. Returns `(body, period_expr)`.
fn lower_periodic_body(
    h: &crate::ast::OnHandler,
    fid: FunctionId,
    cid: ComponentId,
    ctx: &LowerCtx,
    helpers: &helpers::HelperRegistry<'_>,
    constraint_sites: &std::cell::RefCell<Vec<crate::ir::ConstraintSite>>,
) -> Result<(TbFunction, crate::ir::Expr), LowerError> {
    let mut b = FuncBuilder::new(ctx, helpers, constraint_sites);
    b.self_component = Some(cid);
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
    cid: ComponentId,
    ctx: &LowerCtx,
    helpers: &helpers::HelperRegistry<'_>,
    constraint_sites: &std::cell::RefCell<Vec<crate::ir::ConstraintSite>>,
) -> Result<(TbFunction, Option<crate::ir::Expr>, Option<crate::ir::Expr>), LowerError> {
    let mut b = FuncBuilder::new(ctx, helpers, constraint_sites);
    b.self_component = Some(cid);
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
    constraint_sites: &std::cell::RefCell<Vec<crate::ir::ConstraintSite>>,
) -> Result<TbFunction, LowerError> {
    let mut b = FuncBuilder::new(ctx, helpers, constraint_sites);
    b.self_component = Some(cid);
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
    let params = vec![TypedParam {
        name: arg_name,
        ty,
    }];
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
        return IrType::Unknown;
    }
    helpers::ir_type_of(ty)
}

fn lower_method_body(
    h: &HookableMethod,
    fid: FunctionId,
    cid: ComponentId,
    ctx: &LowerCtx,
    helpers: &helpers::HelperRegistry<'_>,
    constraint_sites: &std::cell::RefCell<Vec<crate::ir::ConstraintSite>>,
) -> Result<TbFunction, LowerError> {
    let mut b = FuncBuilder::new(ctx, helpers, constraint_sites);
    b.self_component = Some(cid);
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
            edges.push(resolve_one_connect(env, env_schema, components, e)?);
        }
    }
    Ok(edges)
}

fn resolve_one_connect(
    env: &ComponentDecl,
    env_schema: &ComponentSchema,
    components: &[ComponentSchema],
    edge: &ConnectEdge,
) -> Result<ConnectEdgeSchema, LowerError> {
    // `source.observed -> sb.write_obs`: both sides are dotted paths
    // rooted at an env sub-component field.
    let from = dotted_path(&edge.from).ok_or_else(|| {
        unsupported(
            &format!("a non-path `connect` source in env `{}`", env.name.name),
            "",
        )
    })?;
    let to = dotted_path(&edge.to).ok_or_else(|| {
        unsupported(
            &format!("a non-path `connect` sink in env `{}`", env.name.name),
            "",
        )
    })?;
    // The source path is `<subcomp>.<event>` (final segment is the event
    // port on the source sub-component).
    if from.len() < 2 {
        return Err(unsupported(
            &format!("a `connect` source `{}` without an event field", from.join(".")),
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
    let src_cid = resolve_sub_path(env_schema, components, src_path)?;
    let src_comp = &components[src_cid.index()];
    match src_comp.field(&src_event) {
        Some(ComponentFieldSchema {
            kind: ComponentFieldKind::Event { .. },
            ..
        }) => {}
        _ => {
            return Err(unsupported(
                &format!(
                    "a `connect` source `{}.{src_event}` that is not an `out event` port",
                    src_path.join(".")
                ),
                "",
            ));
        }
    }

    // Resolve the sink sub-component. The final segment is either a
    // hookable sink method (`sb.write_obs`) or an `in event<T>` field on
    // an event-driven transactor (`drv.req`); pick the matching sink shape.
    let sink_cid = resolve_sub_path(env_schema, components, sink_path)?;
    let sink_comp = &components[sink_cid.index()];
    let sink = if let Some(sm) = sink_comp.method(&sink_name) {
        if sm.n_params != 1 {
            return Err(unsupported(
                &format!(
                    "a `connect` sink method `{sink_name}` with {} parameters",
                    sm.n_params
                ),
                "analysis sinks take exactly one payload parameter",
            ));
        }
        crate::ir::ConnectSink::Method { method: sink_name }
    } else if matches!(
        sink_comp.field(&sink_name).map(|f| &f.kind),
        Some(ComponentFieldKind::Event { .. })
    ) {
        crate::ir::ConnectSink::Event { event: sink_name }
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
        sink_path: sink_path.to_vec(),
        sink_component: sink_cid,
        sink,
    })
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
            unsupported(
                &format!("a `connect` path segment `{seg}` that is not a sub-component field"),
                "",
            )
        })?;
        match &f.kind {
            ComponentFieldKind::Sub { component } => {
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
fn lower_event_payload(
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
                _ => Err(reject_named(
                    type_arg_simple_name(ty).unwrap_or("<expr>"),
                )),
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

fn scalar_default(
    default: &Option<crate::ast::Expr>,
    comp: &str,
    fname: &str,
) -> Result<u64, LowerError> {
    match default {
        None => Ok(0),
        Some(d) => match &*d.kind {
            ExprKind::Int(s) => super::exprs::parse_int_literal(s).ok_or_else(|| {
                unsupported(
                    &format!("component field default `{comp}.{fname} default {s}`"),
                    "not a plain integer literal",
                )
            }),
            ExprKind::Bool(b) => Ok(*b as u64),
            _ => Err(unsupported(
                &format!("a non-literal default on component field `{comp}.{fname}`"),
                "",
            )),
        },
    }
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
                    if comp.method(&method).is_none() {
                        return Err(unsupported(
                            &format!(
                                "component `{}` has no method `{method}` (in `{}`)",
                                comp.name,
                                path.join(".")
                            ),
                            "",
                        ));
                    }
                    return Ok(Some((ComponentBase::Path(recv.to_vec()), cid, method)));
                }
            }
        }
        // Self-call form: bare `publish` inside a method body.
        if let ExprKind::Ident(id) = &*callee.kind {
            if let Some(cid) = self.self_component {
                let comp = &self.ctx.components[cid.index()];
                if comp.method(&id.name).is_some() {
                    return Ok(Some((ComponentBase::SelfField, cid, id.name.clone())));
                }
            }
        }
        Ok(None)
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
        let Some(&head_cid) = self.ctx.component_fields.get(&path[0]) else {
            return Ok(false);
        };
        let (recv, field) = path.split_at(path.len() - 1);
        let field = &field[0];
        // A path through a data-only scoreboard sub-component is not a
        // `Dut`-handle bind — let the scoreboard handlers claim it.
        if self.recv_is_scoreboard_sub(head_cid, &recv[1..]) {
            return Ok(false);
        }
        let cid = self.resolve_component_recv(head_cid, &recv[1..])?;
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
                    path.join(".")
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
                    if matches!(
                        comp.field(&id.name).map(|f| &f.kind),
                        Some(ComponentFieldKind::Scalar { .. })
                    ) {
                        return Ok(Some((ComponentBase::SelfField, id.name.clone())));
                    }
                }
            }
        }
        // Dotted path `env.sb.errors`.
        if let Some(path) = dotted_path(target) {
            let path = self.strip_tb_prefix(&path);
            if path.len() >= 2 {
                if let Some(&head_cid) = self.ctx.component_fields.get(&path[0]) {
                    let (recv, field) = path.split_at(path.len() - 1);
                    let field = field[0].clone();
                    // A receiver ending in a data-only scoreboard sub
                    // (`top.sb.<scalar>`) is NOT a component-field write —
                    // fall through so the scoreboard scalar-write path
                    // (`scoreboard_root`) handles it.
                    if self.recv_is_scoreboard_sub(head_cid, &recv[1..]) {
                        return Ok(None);
                    }
                    let cid = self.resolve_component_recv(head_cid, &recv[1..])?;
                    let comp = &self.ctx.components[cid.index()];
                    match comp.field(&field).map(|f| &f.kind) {
                        Some(ComponentFieldKind::Scalar { .. }) => {
                            return Ok(Some((ComponentBase::Path(recv.to_vec()), field)));
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
                    if matches!(
                        comp.field(&id.name).map(|f| &f.kind),
                        Some(ComponentFieldKind::Scalar { .. })
                    ) {
                        return Ok(Some(IrExpr::ComponentField {
                            base: ComponentBase::SelfField,
                            field: id.name.clone(),
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
                    // A receiver ending in a data-only scoreboard sub
                    // (`top.sb.<scalar>`) is a scoreboard read, not a
                    // component-field read — fall through to `scoreboard_root`.
                    if self.recv_is_scoreboard_sub(head_cid, &recv[1..]) {
                        return Ok(None);
                    }
                    let cid = self.resolve_component_recv(head_cid, &recv[1..])?;
                    let comp = &self.ctx.components[cid.index()];
                    if matches!(
                        comp.field(&field).map(|f| &f.kind),
                        Some(ComponentFieldKind::Scalar { .. })
                    ) {
                        return Ok(Some(IrExpr::ComponentField {
                            base: ComponentBase::Path(recv.to_vec()),
                            field,
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
                Some(ComponentFieldKind::Sub { component }) => cid = *component,
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
                Some(ComponentFieldKind::Sub { component }) => cid = *component,
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

    /// Lower the args of a component method call (port-hoisted, like any
    /// host-side call).
    pub(crate) fn lower_component_call_args(
        &mut self,
        args: &[CallArg],
    ) -> Result<Vec<IrExpr>, LowerError> {
        let mut out = Vec::with_capacity(args.len());
        for a in args {
            let CallArg::Expr(e) = a else {
                return Err(unsupported("named arguments in a component method call", ""));
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
                match comp.field(&event).map(|f| &f.kind) {
                    Some(ComponentFieldKind::Event { .. }) => {}
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
        // Self-relative form, inside a method/on-handler body.
        let cid = self.self_component.ok_or_else(|| {
            unsupported(
                "`emit` outside a component method body",
                "only a component event field supports `emit` in this subset",
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
        match comp.field(&event).map(|f| &f.kind) {
            Some(ComponentFieldKind::Event { .. }) => {}
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
            return Err(unsupported(
                &format!("a named argument to `{}`", name.name),
                "",
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
            return Err(unsupported("a named argument to `quiesced`", ""));
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
        let first = terms.next().expect(
            "collect_quiesce_leaves always yields at least the receiver as a leaf",
        );
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
                ComponentFieldKind::Sub { component } => {
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
