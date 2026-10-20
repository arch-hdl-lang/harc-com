//! Native C++ bound-component handler registration emission.

use super::*;

impl Emitter {
    pub(super) fn emit_component_handler_registrations_bound(
        &mut self,
        comp: &ComponentDecl,
        instance: &str,
        depth: usize,
        tag_prefix: &str,
        bound_bus: Option<(BusDecl, String, String)>,
    ) {
        let handler_probe_receivers = self.component_probe_receivers(comp);
        let prior_probe_receivers =
            std::mem::replace(&mut self.probe_receivers, handler_probe_receivers);
        // Build field-name substitution map for the body. Component
        // fields visible by bare name inside the handler body get
        // prefixed with the instance path.
        let mut subs = std::collections::HashMap::new();
        let mut local_event_types = Vec::new();
        let mut saved_field_types = Vec::new();
        let mut event_field_names: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for it in &comp.items {
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
                    let name = f.name.name.clone();
                    let prev = self.let_types.insert(name.clone(), ty.to_string());
                    saved_field_types.push((name, prev));
                }
                if let TypeExpr::Builtin {
                    name: BuiltinTy::Event,
                    args,
                    ..
                } = &f.ty
                {
                    event_field_names.insert(f.name.name.clone());
                    let inner = self.payload_type_for_arg(args.first());
                    local_event_types.push((f.name.name.clone(), inner));
                }
            }
        }

        let prev_subs = std::mem::replace(&mut self.field_subs, subs);
        let mut added_events = Vec::new();
        for (name, ty) in &local_event_types {
            if self.event_types.insert(name.clone(), ty.clone()).is_none() {
                added_events.push(name.clone());
            }
        }

        let comp_ty = comp.name.name.clone();
        for it in &comp.items {
            if let ComponentItem::OnHandler(h) = it {
                // While emitting a transactor's `on`-handler body, expose
                // the same `current_component_method` context that
                // ordinary methods do — receiver = instance path —
                // so a bare-ident call to a transactor-local
                // `hookable` rewrites to `<Type>_<method>(<instance>,
                // …)` (issue #300). Without this, the call emits as a
                // raw identifier and the generated C++ fails to
                // compile because the helper symbol isn't in scope.
                //
                // The override is installed via `CurrentMethodGuard`
                // so restoration happens on drop — any exit path
                // from `emit_single_on_handler_bound` (early
                // `return`, `?`, panic, fall-through) cannot leak
                // the override into the next iteration. A future
                // maintainer adding a new exit gets the restore for
                // free, no manual line to remember.
                let handler_active = self.handler_lives_in_when_active(&comp_ty, h);
                let mut method_guard = CurrentMethodGuard::new(
                    self,
                    Some((
                        comp_ty.clone(),
                        instance.to_string(),
                        format!("on_{}_{}", h.span.start, h.span.end),
                        handler_active,
                    )),
                );
                method_guard.emit_single_on_handler_bound(
                    h,
                    instance,
                    depth,
                    tag_prefix,
                    &event_field_names,
                    bound_bus.as_ref(),
                );
            }
        }

        self.field_subs = prev_subs;
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
        for n in added_events {
            self.event_types.remove(&n);
        }
        self.probe_receivers = prior_probe_receivers;
    }

    /// Emit a single `on`-handler registration for a bound component
    /// instance. Picks one of two shapes based on the trigger:
    ///   * event subscription (`on field { ... }`) — push a closure
    ///     into the field's subscriber list.
    ///   * bool-expression cycle trigger / periodic `on N cycles`
    ///     trigger (monitors, post_eval/checker) — fall through to
    ///     `emit_cycle_trigger`.
    ///
    /// Caller is responsible for installing
    /// `current_component_method` for this handler (via
    /// `CurrentMethodGuard`) before invoking; this helper only
    /// manages the `current_component_instance` scope local to its
    /// own arms.
    fn emit_single_on_handler_bound(
        &mut self,
        h: &OnHandler,
        instance: &str,
        depth: usize,
        tag_prefix: &str,
        event_field_names: &std::collections::HashSet<String>,
        bound_bus: Option<&(BusDecl, String, String)>,
    ) {
        if let Some((event_name, arg_name)) = extract_event_subscription(&h.event) {
            if event_field_names.contains(&event_name) {
                // Subscriber to a component event field.
                let arg_ty = self
                    .event_types
                    .get(&event_name)
                    .cloned()
                    .unwrap_or_else(|| "int64_t".into());
                self.pad(depth);
                // The handler argument is a lambda parameter — another
                // binder that hides a file-scope name.
                self.declared_value_names.insert(arg_name.clone());
                writeln!(
                    self.out,
                    "{instance}.{event_name}.push_back([&]({arg_ty} {arg_name}) {{",
                )
                .ok();
                // Activity tracking (spec §7.x): an incoming event
                // counts as an "in" for the component instance —
                // bump _last_in_cycle to the current cycle. The
                // current-component-instance scope is also set
                // so any `emit`/`bus.send`/`bus.recv` *inside*
                // this body bumps the same instance's
                // _last_out_cycle (those sites can't see the
                // static `instance` parameter directly).
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "{instance}._last_in_cycle = (uint64_t)cycle_count;"
                )
                .ok();
                let prior_inst = std::mem::replace(
                    &mut self.current_component_instance,
                    Some(instance.to_string()),
                );
                // For `bound to BusType` components, expose the
                // driver's bus binding inside the handler body
                // as the bare identifier `bus`. Same root +
                // BusDecl as the test-scope binding it was
                // bound from, so `bus.<ch>.send/recv` and
                // `bus.<ch>.<sig>` lower through the existing
                // bus_handshake / bus_field_access paths.
                let prior_bus = bound_bus
                    .as_ref()
                    .and_then(|b| self.bus_bindings.insert("bus".into(), (*b).clone()));
                self.emit_block_with_bindings(
                    &h.body,
                    depth + 1,
                    std::iter::once(arg_name.as_str()),
                );
                if bound_bus.is_some() {
                    match prior_bus {
                        Some(prev) => {
                            self.bus_bindings.insert("bus".into(), prev);
                        }
                        None => {
                            self.bus_bindings.remove("bus");
                        }
                    }
                }
                self.current_component_instance = prior_inst;
                self.pad(depth);
                writeln!(self.out, "}});").ok();
                return;
            }
        }
        // Fallback: bool-expression cycle trigger (monitors) or
        // periodic `on N cycles` trigger (post_eval / checker).
        let prior_inst = std::mem::replace(
            &mut self.current_component_instance,
            Some(instance.to_string()),
        );
        self.emit_cycle_trigger(h, depth, tag_prefix);
        self.current_component_instance = prior_inst;
    }
}
