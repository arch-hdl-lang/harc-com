//! Native C++ active bound-driver coroutine emission.

use super::*;

impl Emitter {
    /// Phase 2b: if `comp` is a `bound to BusType` driver/agent with
    /// exactly one `in event<T>` field plus a matching
    /// `on <event_name>(t)` handler, emit it as an independent
    /// coroutine actor:
    ///
    /// 1. A per-instance transaction queue (`std::deque<T>`).
    /// 2. A subscriber on the input event that *pushes* incoming
    ///    transactions onto the queue (no longer runs the body
    ///    inline — the actor body lives in the coroutine).
    /// 3. A `ThreadSlot` registered with `sched`.
    /// 4. The actor coroutine: loop forever, `co_await wait_until`
    ///    until the queue is non-empty, pop a transaction, run the
    ///    `on T t` handler body in coroutine context (so internal
    ///    `wait N cycles` and `bus.<ch>.send/recv` lower to `co_await`),
    ///    repeat. The actor never returns; after the *run* coroutine
    ///    finishes, teardown destroys auxiliary actors that remain
    ///    suspended in WaitUntil.
    ///
    /// Returns `true` on success — caller should NOT also emit the
    /// sync subscriber path. Returns `false` to fall through.
    pub(super) fn try_emit_bound_driver_actor(
        &mut self,
        comp: &ComponentDecl,
        instance: &str,
        depth: usize,
        binding: &(BusDecl, String, String),
    ) -> bool {
        // Only `agent` and `transactor` (synth ComponentDecl from a
        // bound active transactor) are eligible — envs / sequencers /
        // scoreboards aren't actor-shaped at all.
        if !matches!(comp.kind, ComponentKind::Agent | ComponentKind::Transactor) {
            return false;
        }

        // Find a single `in event<T>` field. Multi-input drivers stay
        // sync for now (each input would need its own queue + the
        // coroutine would have to multi-way wait — separate PR).
        let mut input_event: Option<(String, String)> = None;
        for it in &comp.items {
            if let ComponentItem::Field(f) = it {
                if matches!(f.direction, Some(Direction::In)) {
                    if let TypeExpr::Builtin {
                        name: BuiltinTy::Event,
                        args,
                        ..
                    } = &f.ty
                    {
                        let payload = self.payload_type_for_arg(args.first());
                        if input_event.is_some() {
                            return false; // multi-input — bail out
                        }
                        input_event = Some((f.name.name.clone(), payload));
                    }
                }
            }
        }
        let Some((event_name, payload_ty)) = input_event else {
            return false; // no input event — nothing to actor-ify
        };

        // Find the matching on-handler. If absent or there's more
        // than one matching, fall back. (Other handlers on different
        // events are fine — they just don't get coroutine-ified by
        // this PR; the sync registration still runs for them.)
        let mut matched_handler: Option<&OnHandler> = None;
        let mut other_handlers: Vec<&OnHandler> = Vec::new();
        for it in &comp.items {
            if let ComponentItem::OnHandler(h) = it {
                if let Some((ev, _arg)) = extract_event_subscription(&h.event) {
                    if ev == event_name {
                        if matched_handler.is_some() {
                            return false; // ambiguous multi-handler — bail
                        }
                        matched_handler = Some(h);
                    } else {
                        other_handlers.push(h);
                    }
                }
            }
        }
        let Some(handler) = matched_handler else {
            return false;
        };
        let arg_name = match extract_event_subscription(&handler.event) {
            Some((_, a)) => a,
            None => return false,
        };

        // ── Emit the actor topology ─────────────────────────────────
        // Cooperative mode (default): all actor slots go into the
        // single `sched` and tick together with `_run_slot` on the
        // main thread. This is faster than MT on typical fixtures
        // because there's no barrier overhead.
        //
        // MT mode (`--mt`, Phase 3a): each actor gets its own
        // `ThreadScheduler` and lives on a dedicated OS thread,
        // synchronized via dual barriers per posedge. `tick()`
        // itself is not MT-safe so per-actor schedulers avoid locks.
        let queue_var = format!("_{instance}_q");
        let slot_var = format!("_{instance}_slot");
        let sched_var = format!("_{instance}_sched");

        self.pad(depth);
        writeln!(self.out, "std::deque<{payload_ty}> {queue_var};").ok();
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

        // Pusher subscriber on the input event. Replaces the sync
        // body-callback; emit/connect bridges that fan out to this
        // event now feed the actor's queue.
        self.pad(depth);
        writeln!(
            self.out,
            "{instance}.{event_name}.push_back([&]({payload_ty} {arg_name}) {{ {queue_var}.push_back({arg_name}); }});",
        ).ok();

        // Spawn the coroutine. Body emits the on-handler body in
        // coroutine context with the bus binding active for typed
        // signal access.
        self.pad(depth);
        writeln!(
            self.out,
            "auto {slot_var}_lambda = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{",
        )
        .ok();
        self.pad(depth + 1);
        writeln!(self.out, "while (true) {{").ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "co_await harc_rt::wait_until(_slot, [&]{{ return !{queue_var}.empty(); }});",
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "auto {arg_name} = {queue_var}.front();").ok();
        self.pad(depth + 2);
        writeln!(self.out, "{queue_var}.pop_front();").ok();
        // Activity tracking (spec §7.x): popping a transaction off
        // the per-actor queue counts as an "in" for this instance.
        self.pad(depth + 2);
        writeln!(
            self.out,
            "{instance}._last_in_cycle = (uint64_t)cycle_count;"
        )
        .ok();

        // Build field-name substitution map: bare names inside the
        // handler body resolve to `instance.field`. Event-typed
        // fields also register their payload types so any nested
        // `emit <ev>(arg)` finds the right param shape.
        let mut subs = std::collections::HashMap::new();
        let mut local_event_types: Vec<(String, String)> = Vec::new();
        let mut saved_field_types = Vec::new();
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

        // Push the bus binding under the alias `"bus"` so
        // `bus.<ch>.send/recv` and `bus.<ch>.<sig>` inside the actor
        // body resolve to flat DUT signals via the bus-typing
        // lowerers — same mechanism Phase 2a's sync path uses.
        let prior_bus = self.bus_bindings.insert("bus".into(), binding.clone());
        // Mark coroutine context so wait/tick lower to co_await.
        let prior_corout = self.in_coroutine;
        self.in_coroutine = true;
        // Set current-component scope so `emit`/`bus.<ch>.send/recv`
        // nested in the actor body bump this instance's heartbeat
        // fields.
        let prior_inst = std::mem::replace(
            &mut self.current_component_instance,
            Some(instance.to_string()),
        );

        self.emit_block_with_bindings(&handler.body, depth + 2, std::iter::once(arg_name.as_str()));

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

        self.pad(depth + 1);
        writeln!(self.out, "}}").ok(); // close while(true)
        self.pad(depth + 1);
        writeln!(self.out, "co_return;").ok(); // unreachable but required
        self.pad(depth);
        writeln!(self.out, "}};").ok();
        self.pad(depth);
        writeln!(
            self.out,
            "{slot_var}.thread = {slot_var}_lambda(&{slot_var});"
        )
        .ok();

        // Other on-handlers (on different events) still register
        // sync — they don't go through the actor's queue and may
        // legitimately want sync dispatch.
        if !other_handlers.is_empty() {
            let tag = format!("_{}_", comp.kind.keyword());
            // Only register the non-matching handlers via the sync
            // path. We do that by emitting them one-at-a-time using
            // the existing helper after temporarily replacing comp's
            // items list. Simpler: filter manually here.
            let mut filtered = comp.clone();
            filtered.items.retain(|it| match it {
                ComponentItem::OnHandler(h) => extract_event_subscription(&h.event)
                    .map(|(ev, _)| ev != event_name)
                    .unwrap_or(true),
                _ => true,
            });
            self.emit_component_handler_registrations_bound(
                &filtered,
                instance,
                depth,
                &tag,
                Some(binding.clone()),
            );
        }

        true
    }
}
