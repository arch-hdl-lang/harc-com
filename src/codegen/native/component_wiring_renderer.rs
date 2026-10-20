//! Native C++ component wiring and bound-monitor emission.

use super::*;

impl Emitter {
    /// Generic on-handler registration for any component (agent /
    /// transactor / sequencer / scoreboard). Dispatches each handler
    /// by the shape of its trigger expression:
    ///
    /// - `on event_field(arg) ... end on` (a `Call` expression) →
    ///   register a `[&]`-capturing closure into the corresponding
    ///   event vector. Used for drivers' `on req(t)`, sequencers'
    ///   subscriptions, etc.
    /// - `on <bool-expr> ... end on` (any other shape) → register a
    ///   per-cycle checker closure with the requested edge mode
    ///   (rising/falling/level). Used for monitors' `on dut.x && y`.
    ///
    /// `tag_prefix` distinguishes the static cycle-trigger state
    /// across invocations so concurrent components at the same source
    /// span don't collide. Inside both paths, body bare-references to
    /// component fields rewrite to `<instance>.<field>` via
    /// `field_subs`.
    /// Emit `on`-handler subscriber closures for a component instance.
    ///
    /// `bound_bus = Some((BusDecl, root))` is set when the component is
    /// declared `bound to BusType` and the user instantiated it via
    /// `let drv : Drv = bind <bus_binding>`. While each handler body is
    /// emitted, `bus_bindings["bus"]` is temporarily set to this binding
    /// so `bus.<ch>.send(t.addr, …)` and `bus.<ch>.<sig>` inside the
    /// body resolve to flat DUT signals through the existing bus-typing
    /// lowerers. The temporary entry is popped after each body so it
    /// doesn't leak across components.
    pub(super) fn emit_component_handler_registrations(
        &mut self,
        comp: &ComponentDecl,
        instance: &str,
        depth: usize,
        tag_prefix: &str,
    ) {
        self.emit_component_handler_registrations_bound(comp, instance, depth, tag_prefix, None);
    }

    fn field_type_in_component_type(&self, ty: &str, field: &str) -> Option<String> {
        let lookup = |items: &[ComponentItem]| -> Option<String> {
            items.iter().find_map(|it| {
                if let ComponentItem::Field(f) = it {
                    if f.name.name == field {
                        return type_simple_name(Some(&f.ty)).map(String::from);
                    }
                }
                None
            })
        };
        if let Some(comp) = self.components.get(ty) {
            lookup(&comp.items)
        } else if let Some(t) = self.transactors.get(ty) {
            let synth = synth_component_from_transactor(t, /*include_active*/ true);
            lookup(&synth.items)
        } else {
            None
        }
    }

    fn hookable_method_info(&self, ty: &str, method: &str) -> Option<(usize, bool)> {
        let lookup = |items: &[ComponentItem]| -> Option<(usize, bool)> {
            items.iter().find_map(|it| {
                if let ComponentItem::Hookable(h) = it {
                    if h.is_hookable && h.name.name == method {
                        return Some((h.params.len(), h.return_ty.is_some()));
                    }
                }
                None
            })
        };
        if let Some(comp) = self.components.get(ty) {
            lookup(&comp.items)
        } else if let Some(t) = self.transactors.get(ty) {
            let synth = synth_component_from_transactor(t, /*include_active*/ true);
            lookup(&synth.items)
        } else {
            None
        }
    }

    fn resolve_connect_hookable_sink(
        &mut self,
        owner: &ComponentDecl,
        instance: &str,
        to_path: &str,
    ) -> Option<ConnectHookableSink> {
        let parts: Vec<&str> = to_path.split('.').collect();
        let method = parts.last()?;
        let mut cur_ty = owner.name.name.clone();
        let mut target_instance = instance.to_string();
        for seg in &parts[..parts.len().saturating_sub(1)] {
            let Some(next_ty) = self.field_type_in_component_type(&cur_ty, seg) else {
                return None;
            };
            target_instance.push('.');
            target_instance.push_str(seg);
            cur_ty = next_ty;
        }
        let Some((param_count, has_return)) = self.hookable_method_info(&cur_ty, method) else {
            return None;
        };
        let mut sink_path: Vec<String> = instance.split('.').map(String::from).collect();
        sink_path.extend(
            parts[..parts.len().saturating_sub(1)]
                .iter()
                .map(|seg| (*seg).to_string()),
        );
        if self.method_lives_in_when_active(&cur_ty, method) {
            if let Some(TransactorMode::Passive) = self.resolve_path_mode(&sink_path) {
                self.errors.push(format!(
                    "connect: hookable sink `{}.{}` lives inside `when active` of transactor `{}`, \
                     so it does not exist on a passive instance. Change the let-binding or field to `{} active`, \
                     or remove the connect edge. See spec §8.1.",
                    target_instance, method, cur_ty, cur_ty,
                ));
                return Some(ConnectHookableSink {
                    comp_ty: cur_ty,
                    instance: target_instance,
                    method: (*method).to_string(),
                });
            }
        }
        if param_count != 1 {
            self.errors.push(format!(
                "connect: hookable sink `{to_path}` must take exactly one payload argument, got {param_count}"
            ));
            return Some(ConnectHookableSink {
                comp_ty: cur_ty,
                instance: target_instance,
                method: (*method).to_string(),
            });
        }
        if has_return {
            self.errors.push(format!(
                "connect: hookable sink `{to_path}` must return void"
            ));
            return Some(ConnectHookableSink {
                comp_ty: cur_ty,
                instance: target_instance,
                method: (*method).to_string(),
            });
        }
        Some(ConnectHookableSink {
            comp_ty: cur_ty,
            instance: target_instance,
            method: (*method).to_string(),
        })
    }

    pub(super) fn emit_connect_edge(
        &mut self,
        owner: &ComponentDecl,
        instance: &str,
        edge: &ConnectEdge,
        depth: usize,
    ) {
        let from = expr_path_str(&edge.from);
        let to = expr_path_str(&edge.to);
        let (Some(from), Some(to)) = (from, to) else {
            self.errors
                .push("connect: edge endpoints must be plain field paths in v0 cpp_tb".into());
            return;
        };
        self.pad(depth);
        if let Some(sink) = self.resolve_connect_hookable_sink(owner, instance, &to) {
            // Same glue-member rule as a direct call site: a capsule can
            // only reach a shared callable through `_glue.`.
            let glue = if self.flavor == Flavor::Capsule
                && self.is_glue_member(&format!("{}_{}", sink.comp_ty, sink.method))
            {
                "_glue."
            } else {
                ""
            };
            writeln!(
                self.out,
                "{}.{}.push_back([&](auto _t) {{ {}{}_{}({}, _t); }});",
                instance, from, glue, sink.comp_ty, sink.method, sink.instance,
            )
            .ok();
        } else {
            writeln!(
                self.out,
                "{}.{}.push_back([&](auto _t) {{ for (auto& _s : {}.{}) _s(_t); }});",
                instance, from, instance, to,
            )
            .ok();
        }
    }

    /// Recursively walk a component's `Field`s and register handlers
    /// for each sub-component or sub-transactor. Mode flows down the
    /// inheritance chain via `inherited_mode`:
    ///
    /// ```text
    ///   let topenv : OuterEnv passive
    ///                ^^^^^^^^^^^^^^^^^
    ///                root inherited_mode = Some(Passive)
    ///
    ///   env OuterEnv { ag : MyAgent }      → ag inherits Passive
    ///   agent MyAgent { drv : T }          → drv inherits Passive
    /// ```
    ///
    /// At each sub-field, the **field-level explicit mode** (e.g. `drv
    /// : T active`) wins; only when the field has no mode does the
    /// inherited mode apply. If a transactor sub-field ends up with no
    /// mode (neither field-level nor inherited), it's a clear error.
    ///
    /// Sub-component connect edges are emitted here too — paths like
    /// `sequencer.dispatched -> drv.req` inside an agent get prefixed
    /// with the agent's instance path so they wire correctly when the
    /// agent is composed into a parent env.
    pub(super) fn emit_subcomponent_handler_registrations(
        &mut self,
        comp: &ComponentDecl,
        instance_path: &str,
        inherited_mode: Option<TransactorMode>,
        depth: usize,
    ) {
        for ci in &comp.items {
            let f = match ci {
                ComponentItem::Field(f) => f,
                _ => continue,
            };
            let field_ty = match type_simple_name(Some(&f.ty)) {
                Some(t) => t,
                None => continue,
            };
            // Resolve effective mode: field-explicit wins over inherited.
            let field_mode = match &f.ty {
                TypeExpr::Named { mode: Some(m), .. } => Some(*m),
                _ => None,
            };
            let effective_mode = field_mode.or(inherited_mode);

            // Sub-component (driver/agent/env/sequencer/scoreboard).
            if let Some(sub_comp) = self.components.get(field_ty).cloned() {
                let sub_inst = format!("{}.{}", instance_path, f.name.name);
                let sub_tag = format!("_{}_{}_", sub_comp.kind.keyword(), f.name.name);
                self.emit_component_handler_registrations(&sub_comp, &sub_inst, depth, &sub_tag);
                // Sub-component watchdog (spec §8.6) — install the
                // periodic checker prefixed with the sub-instance path
                // so each composed agent inside an env gets its own
                // independent watchdog firing.
                for sub_ci in &sub_comp.items {
                    if let ComponentItem::Watchdog(w) = sub_ci {
                        self.emit_watchdog_checker(&sub_comp, w, &sub_inst, depth);
                    }
                }
                // Recurse into the sub-component's own sub-fields.
                self.emit_subcomponent_handler_registrations(
                    &sub_comp,
                    &sub_inst,
                    effective_mode,
                    depth,
                );
                // Emit the sub-component's connect edges, prefixed
                // with its instance path. Without this, an agent's
                // `connect sequencer.dispatched -> drv.req` would
                // never wire when the agent is composed into a parent
                // env.
                for sub_ci in &sub_comp.items {
                    if let ComponentItem::Connect(cb) = sub_ci {
                        for edge in &cb.edges {
                            self.emit_connect_edge(&sub_comp, &sub_inst, edge, depth);
                        }
                    }
                }
                continue;
            }

            // Covergroup sub-field. Register the per-cycle sample
            // closure prefixed with the sub-instance path so the
            // testbench-bound form (`testbench Tb { cov : Cg }`)
            // gets its sample firing for free. Without this, a
            // covergroup declared as a testbench field never
            // samples (the test-scope `let cov : Cg` registration
            // is the only path otherwise — and the desugarer doesn't
            // mint a parallel let at test scope).
            if let Some(g) = self.covergroups.get(field_ty).cloned() {
                let sub_inst = format!("{}.{}", instance_path, f.name.name);
                self.emit_covergroup_sample_registration(&g, &sub_inst, depth);
                continue;
            }

            // Transactor sub-field. Resolve mode and emit handlers
            // through the synth ComponentDecl path.
            if let Some(t) = self.transactors.get(field_ty).cloned() {
                if t.bound_to.is_some() {
                    self.errors.push(format!(
                        "transactor field `{}.{} : {}` has a `bound to` clause; bound sub-components inside an env/agent are out of v0 scope",
                        instance_path, f.name.name, field_ty,
                    ));
                    continue;
                }
                let mode = match effective_mode {
                    Some(m) => m,
                    None => {
                        self.errors.push(format!(
                            "transactor field `{}.{} : {}` has no mode and no parent specifies one; annotate the field or one of its parent let/field sites with `active`/`passive`",
                            instance_path, f.name.name, field_ty,
                        ));
                        continue;
                    }
                };
                let include_active = matches!(mode, TransactorMode::Active);
                let synth = synth_component_from_transactor(&t, include_active);
                let sub_inst = format!("{}.{}", instance_path, f.name.name);
                let sub_tag = format!("_xactor_{}_", sub_inst.replace('.', "_"));
                self.emit_component_handler_registrations(&synth, &sub_inst, depth, &sub_tag);
            }
        }
    }

    /// Phase 2c: emit each `on bus.<ch>.handshake(arg)` handler in a
    /// `bound to BusType` monitor as an independent coroutine actor:
    ///
    /// ```text
    ///     while (true) {
    ///         co_await wait_until(_slot, [&]{ return valid && ready; });
    ///         auto <arg> = <first payload signal>;
    ///         <body — bus.<ch>.<sig> resolves through the bus binding>
    ///         co_await wait_cycles(_slot, 1);   // skip past this handshake
    ///     }
    /// ```
    ///
    /// Non-handshake on-handlers in the monitor (event subscribers,
    /// cycle triggers on bool expressions) fall through to the
    /// existing sync `_checkers`-based path. A monitor can mix both —
    /// each handler is independently classified.
    ///
    /// The trailing `wait_cycles(1)` matters: a handshake completes
    /// in exactly one cycle (valid && ready both high at the posedge),
    /// but the producer/consumer may keep the signals high across
    /// multiple cycles for back-to-back handshakes. Skipping forward
    /// one cycle ensures we don't re-fire on the same handshake.
    /// Back-to-back handshakes with no idle gap will fire on every
    /// cycle, which is exactly what arch §19 specifies.
    pub(super) fn emit_bound_monitor_actors(
        &mut self,
        mon: &ComponentDecl,
        instance: &str,
        depth: usize,
        binding: &(BusDecl, String, String),
    ) {
        // Build field substitution map (same shape as
        // emit_component_handler_registrations) so bare names inside
        // the handler body resolve to `instance.field`.
        let mut subs = std::collections::HashMap::new();
        let mut local_event_types: Vec<(String, String)> = Vec::new();
        let mut local_field_types: Vec<(String, String)> = Vec::new();
        for it in &mon.items {
            if let ComponentItem::Field(f) = it {
                let field_ref = if self.is_dut_pointer_field_type(&f.ty)
                    && self.pointer_vars.contains(&f.name.name)
                {
                    f.name.name.clone()
                } else {
                    format!("{instance}.{}", f.name.name)
                };
                subs.insert(f.name.name.clone(), field_ref);
                if let Some(ty) = type_simple_name(Some(&f.ty)) {
                    local_field_types.push((f.name.name.clone(), ty.to_string()));
                }
                if let TypeExpr::Builtin {
                    name: BuiltinTy::Event,
                    args,
                    ..
                } = &f.ty
                {
                    let inner = self.payload_type_for_arg(args.first());
                    local_event_types.push((f.name.name.clone(), inner));
                }
            }
        }

        // Walk handlers, classify each as handshake-actor vs sync.
        let mut sync_handlers: Vec<&OnHandler> = Vec::new();
        for it in &mon.items {
            if let ComponentItem::OnHandler(h) = it {
                if let Some((ch_name, arg_name)) = extract_bus_handshake_event(&h.event, "bus") {
                    // Resolve the channel in the bound bus.
                    let channel = match binding
                        .0
                        .handshakes
                        .iter()
                        .find(|hs| hs.name.name == ch_name)
                    {
                        Some(c) => c.clone(),
                        None => {
                            self.errors.push(format!(
                                "monitor {instance}: bus has no channel `{ch_name}`"
                            ));
                            continue;
                        }
                    };
                    if channel.payload.is_empty() {
                        self.errors.push(format!(
                            "monitor {instance}: channel `{ch_name}` has no payload signals to capture"
                        ));
                        continue;
                    }
                    let (bus_decl, root, sig_prefix) = binding;
                    let slot_var = format!("_{instance}_{ch_name}_slot");
                    let sched_var = format!("_{instance}_{ch_name}_sched");

                    if self.mt {
                        self.pad(depth);
                        writeln!(self.out, "harc_rt::ThreadScheduler {sched_var};").ok();
                    }
                    self.pad(depth);
                    writeln!(self.out, "harc_rt::ThreadSlot {slot_var};").ok();
                    self.pad(depth);
                    if self.mt {
                        writeln!(self.out, "{sched_var}.slots.push_back(&{slot_var});").ok();
                        self.actor_threads
                            .push((sched_var.clone(), slot_var.clone()));
                    } else {
                        writeln!(self.out, "sched.slots.push_back(&{slot_var});").ok();
                    }
                    self.pad(depth);
                    writeln!(
                        self.out,
                        "auto {slot_var}_lambda = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{",
                    ).ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "while (true) {{").ok();
                    let valid_port = self.bus_signal_name(&sig_prefix, &ch_name, "valid");
                    let ready_port = self.bus_signal_name(&sig_prefix, &ch_name, "ready");
                    self.pad(depth + 2);
                    writeln!(
                        self.out,
                        "co_await harc_rt::wait_until(_slot, [&]{{ return {root}->{valid_port} && {root}->{ready_port}; }});",
                    ).ok();
                    // Bind `arg` to the per-channel payload struct so the
                    // body can use `arg.data`, `arg.resp`, etc. The
                    // struct's implicit-conversion-to-first-field
                    // operator means scalar use (`sb.queue.push(arg)`)
                    // also keeps working — push receives the first
                    // payload value, matching pre-multi-payload behaviour.
                    self.pad(depth + 2);
                    let struct_name = format!("{}_{}_payload", bus_decl.name.name, ch_name);
                    write!(self.out, "{struct_name} {arg_name} = {{").ok();
                    let mut first = true;
                    for sig in &channel.payload {
                        if !first {
                            write!(self.out, ", ").ok();
                        }
                        first = false;
                        let sig_port = self.bus_signal_name(&sig_prefix, &ch_name, &sig.name.name);
                        write!(self.out, "{root}->{sig_port}").ok();
                    }
                    writeln!(self.out, "}};").ok();
                    // Activity tracking (spec §7.x): an observed bus
                    // handshake counts as an "in" for this monitor
                    // instance.
                    self.pad(depth + 2);
                    writeln!(
                        self.out,
                        "{instance}._last_in_cycle = (uint64_t)cycle_count;"
                    )
                    .ok();

                    // Body: install field subs + bus binding, mark
                    // coroutine context, emit, restore state.
                    let prev_subs = std::mem::replace(&mut self.field_subs, subs.clone());
                    let mut saved_field_types = Vec::new();
                    for (name, ty) in &local_field_types {
                        let prev = self.let_types.insert(name.clone(), ty.clone());
                        saved_field_types.push((name.clone(), prev));
                    }
                    let mut added_events = Vec::new();
                    for (name, ty) in &local_event_types {
                        if self.event_types.insert(name.clone(), ty.clone()).is_none() {
                            added_events.push(name.clone());
                        }
                    }
                    let prior_bus = self.bus_bindings.insert("bus".into(), binding.clone());
                    let prior_corout = self.in_coroutine;
                    self.in_coroutine = true;
                    let prior_inst = std::mem::replace(
                        &mut self.current_component_instance,
                        Some(instance.to_string()),
                    );

                    self.emit_block_with_bindings(
                        &h.body,
                        depth + 2,
                        std::iter::once(arg_name.as_str()),
                    );

                    self.current_component_instance = prior_inst;
                    self.in_coroutine = prior_corout;
                    match prior_bus {
                        Some(prev) => {
                            self.bus_bindings.insert("bus".into(), prev);
                        }
                        None => {
                            self.bus_bindings.remove("bus");
                        }
                    }
                    for n in added_events {
                        self.event_types.remove(&n);
                    }
                    for (name, prev) in saved_field_types {
                        match prev {
                            Some(ty) => {
                                self.let_types.insert(name, ty);
                            }
                            None => {
                                self.let_types.remove(&name);
                            }
                        }
                    }
                    self.field_subs = prev_subs;

                    // Skip past this handshake before re-arming.
                    self.pad(depth + 2);
                    writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "}}").ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "co_return;").ok();
                    self.pad(depth);
                    writeln!(self.out, "}};").ok();
                    self.pad(depth);
                    writeln!(
                        self.out,
                        "{slot_var}.thread = {slot_var}_lambda(&{slot_var});"
                    )
                    .ok();
                } else {
                    sync_handlers.push(h);
                }
            }
        }

        // Other on-handlers fall through to the existing sync
        // registration. We do this by emitting a temporary clone of
        // the monitor whose `items` contain only the leftover
        // handlers + the original fields, so existing field-sub /
        // event-subscription paths run unchanged.
        if !sync_handlers.is_empty() {
            let mut sync_view = mon.clone();
            sync_view.items.retain(|it| match it {
                ComponentItem::OnHandler(h) => {
                    extract_bus_handshake_event(&h.event, "bus").is_none()
                }
                _ => true,
            });
            self.emit_component_handler_registrations_bound(
                &sync_view,
                instance,
                depth,
                "_m_",
                Some(binding.clone()),
            );
        }
    }
}
