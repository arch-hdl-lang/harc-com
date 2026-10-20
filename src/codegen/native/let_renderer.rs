//! Native C++ local declaration and instantiation emission.

use super::*;

impl Emitter {
    /// The supported instantiation forms for a type this emitter
    /// *declares an instance of* rather than *computes a value of* —
    /// transactor, component (driver / agent / env / sequencer /
    /// scoreboard), covergroup, bus, and the RAL blocks. Returns
    /// `(what it is, supported forms)` for use in a diagnostic, or
    /// `None` for any name this emitter does not declare.
    ///
    /// `None` is the load-bearing case: the DUT's SV top-module type
    /// legitimately reaches `c_type_for`'s `V<Name>*` mapping, and so
    /// does any other type name the emitter has no declaration for.
    /// Only names it *does* declare are known-bad there.
    ///
    /// Value types (transaction, struct, enum) are deliberately absent:
    /// `let t2 : Txn = t1` is a copy, not an instantiation, and records
    /// already lower to a real C++ struct in `local_value_c_type`.
    fn instantiation_only_forms(&self, name: &str) -> Option<(String, String)> {
        if let Some(t) = self.transactors.get(name) {
            // Mode is mandatory at every transactor instantiation
            // (spec §8.1); `bound to` additionally requires the bind.
            let forms = if t.bound_to.is_some() {
                format!(
                    "`let <name> : {name} active = bind <bus-binding>` or \
                     `let <name> : {name} passive = bind <bus-binding>` (spec §8.1)"
                )
            } else {
                format!(
                    "`let <name> : {name} active` or `let <name> : {name} passive` \
                     (spec §8.1)"
                )
            };
            return Some((format!("transactor `{name}`"), forms));
        }
        if let Some(c) = self.components.get(name) {
            let kw = c.kind.keyword();
            let forms = if c.bound_to.is_some() {
                format!("`let <name> : {name} = bind <bus-binding>`")
            } else {
                format!("`let <name> : {name}`, which default-constructs")
            };
            return Some((format!("{kw} `{name}`"), forms));
        }
        if self.covergroups.contains_key(name) {
            return Some((
                format!("covergroup `{name}`"),
                format!("`let <name> : {name}`, which default-constructs"),
            ));
        }
        if self.buses.contains_key(name) {
            return Some((
                format!("bus `{name}`"),
                format!("`let <name> : {name} = bind <dut-expr>`"),
            ));
        }
        if self.regblocks.contains_key(name) {
            return Some((
                format!("regblock `{name}`"),
                format!("`let <name> : {name} = bind <helper>`"),
            ));
        }
        if self.addrmaps.contains_key(name) {
            return Some((
                format!("addrmap `{name}`"),
                format!("`let <name> : {name} = bind <helper>`"),
            ));
        }
        None
    }

    pub(super) fn emit_let(&mut self, l: &LetStmt, depth: usize) {
        if l.name.name == "_" {
            self.pad(depth);
            if let Some(v) = &l.value {
                write!(self.out, "(void)(").ok();
                self.emit_expr(v);
                writeln!(self.out, ");").ok();
            } else {
                writeln!(self.out, "// let _ (discard)").ok();
            }
            return;
        }
        // Every `let` binds a C++ name that hides any file-scope one, so
        // record it before anything else: a local named like a suppressed
        // enum variant must not draw that variant's diagnostic.
        self.declared_value_names.insert(l.name.name.clone());
        // Track let-binding type for randomize(t) resolution. Done first
        // (before the dut shortcut) so even nested lets register.
        if let Some(s) = type_simple_name(l.ty.as_ref()) {
            self.let_types.insert(l.name.name.clone(), s.to_string());
        }
        // Track mode annotation for the call-site passive-mode check.
        // Same gate as the pre-pass at lowering entry — only typed lets
        // with an explicit `active`/`passive` populate; inheritance is
        // resolved at lookup time via `resolve_path_mode`.
        if let Some(TypeExpr::Named { mode: Some(m), .. }) = l.ty.as_ref() {
            self.let_modes.insert(l.name.name.clone(), *m);
        }
        // Track explicit bit-width for width-method intrinsics
        // (`.trunc<N>()` etc.). Same logic as the per-test loop seed
        // in `emit_with_opts` — typed lets only; bare `let x = expr`
        // falls back to RHS inference.
        if let Some(TypeExpr::Builtin { name, args, .. }) = l.ty.as_ref() {
            // Signedness gate FIRST, and outside the `UInt | SInt | Bits`
            // match below: that match excludes `SIntCap`, so a check nested
            // inside it could never fire for `let s : SInt<8> = …`.
            if matches!(name, BuiltinTy::SInt | BuiltinTy::SIntCap) {
                if let (Some(w), Some(v)) = (type_arg_width(args), l.value.as_ref()) {
                    self.check_signed_wrap_destination(&l.name.name, w, v);
                }
            }
            if matches!(name, BuiltinTy::UInt | BuiltinTy::SInt | BuiltinTy::Bits) {
                if let Some(w) = type_arg_width(args) {
                    // Narrowing initializer: reject before emitting. v1
                    // used to emit `HarcWide<7> b = a;` for a 256-bit `a`,
                    // which does not compile, and silently accepted the
                    // ≤64-bit cases TB-IR rejects. Same diagnostic and same
                    // accepted set as TB-IR's `check_scalar_assign_width`.
                    let dw = w as u32;
                    let aw = l
                        .value
                        .as_ref()
                        .filter(|_| dw > 0)
                        .and_then(|v| narrowing_source_width(self, v));
                    if let Some(aw) = aw {
                        if aw > dw {
                            self.errors.push(format!(
                                "assignment of a {aw}-bit value to `{}`, declared {dw} \
                                 bits, narrows. Widths must not shrink implicitly — use \
                                 `.trunc<{dw}>()` to narrow explicitly, or widen the \
                                 declaration to {aw} bits.",
                                l.name.name
                            ));
                        }
                    }
                    let lw = LetWidth {
                        bits: dw,
                        signed: matches!(name, BuiltinTy::SInt),
                    };
                    if self.let_widths.insert(l.name.name.clone(), lw).is_some() {
                        self.shadowed_lets.insert(l.name.name.clone());
                    }
                }
            }
        }
        // Bus binding: `let axil : BusAxiLite = bind dut`. Track the
        // (bus, dut-root) pair so subsequent `axil.signal` accesses
        // can lower to flat DUT signals (`dut->axil_signal`). The
        // binding itself emits no C++ — it's purely a typing
        // assertion and a name-prefix declaration.
        if let Some(simple) = type_simple_name(l.ty.as_ref()) {
            if let Some(bus) = self.buses.get(simple).cloned() {
                if let Some(v) = &l.value {
                    // Capture the bind expression as a string. For the
                    // common case of `bind dut`, that's just "dut".
                    let mut buf = String::new();
                    std::mem::swap(&mut self.out, &mut buf);
                    self.emit_expr(v);
                    std::mem::swap(&mut self.out, &mut buf);
                    // Test-scope binding: identifier and prefix are
                    // the same. Aliases (e.g. driver's `bus`) reuse
                    // this prefix when registering — see
                    // `emit_component_handler_registrations_bound`.
                    let prefix = l.name.name.clone();
                    // Effective param env for `generate_if` gate evaluation:
                    // bus defaults overlaid with bind-site generic overrides and
                    // then the DUT port's own override (authoritative for which
                    // gated channels `arch build` flattened — bind name == DUT
                    // port name by convention).
                    let env = bus_param_env_with_port_override(
                        &bus,
                        l.ty.as_ref(),
                        self.dut_bus_port_overrides.get(&l.name.name),
                    );
                    self.bus_param_envs.insert(l.name.name.clone(), env);
                    self.bus_bindings
                        .insert(l.name.name.clone(), (bus, buf, prefix.clone()));
                    // Translate the AST's bind_remap entries into a
                    // (channel, signal) → port_name lookup table. The
                    // path is required to be exactly `<channel>.<signal>`
                    // (two segments) — single-segment paths and 3+
                    // levels error out cleanly.
                    if !l.bind_remap.is_empty() {
                        let mut map: std::collections::HashMap<(String, String), String> =
                            std::collections::HashMap::new();
                        for entry in &l.bind_remap {
                            if entry.path.len() != 2 {
                                self.errors.push(format!(
                                    "bind {} with: signal path `{}` must be exactly `<channel>.<signal>` (2 segments, got {})",
                                    l.name.name,
                                    entry.path.iter().map(|i| i.name.as_str()).collect::<Vec<_>>().join("."),
                                    entry.path.len(),
                                ));
                                continue;
                            }
                            map.insert(
                                (entry.path[0].name.clone(), entry.path[1].name.clone()),
                                entry.port.clone(),
                            );
                        }
                        self.bus_remap.insert(prefix, map);
                    }
                    return;
                }
            }
        }
        // RAL addrmap binding: `let chip : <AddrmapType> = bind <helper>`.
        // Same shape as regblock binding — emits a value-typed mirror
        // struct and records the helper.
        if let Some(simple) = type_simple_name(l.ty.as_ref()) {
            if self.addrmaps.contains_key(simple) {
                if l.bind {
                    if let Some(v) = &l.value {
                        if let ExprKind::Ident(rhs) = &*v.kind {
                            self.let_helper
                                .insert(l.name.name.clone(), rhs.name.clone());
                        } else {
                            self.errors.push(format!(
                                "let {} : {simple} = bind <expr>: addrmap binding RHS must be a helper transactor identifier",
                                l.name.name,
                            ));
                        }
                    }
                } else {
                    self.errors.push(format!(
                        "let {} : {simple}: addrmap instantiation requires `= bind <helper>`",
                        l.name.name,
                    ));
                }
                self.pad(depth);
                writeln!(self.out, "{simple}_Mirror {};", l.name.name).ok();
                return;
            }
        }
        // RAL regblock binding: `let regs : <RegblockType> = bind <helper>`.
        // Emit `<T>_Mirror regs;` (reset values populate from the struct
        // default initializers emitted at file scope) and record the
        // helper variable name so accessor lowering can resolve it.
        // See docs/ral-support.md.
        if let Some(simple) = type_simple_name(l.ty.as_ref()) {
            if self.regblocks.contains_key(simple) {
                if l.bind {
                    if let Some(v) = &l.value {
                        if let ExprKind::Ident(rhs) = &*v.kind {
                            self.let_helper
                                .insert(l.name.name.clone(), rhs.name.clone());
                        } else {
                            self.errors.push(format!(
                                "let {} : {simple} = bind <expr>: regblock binding RHS must be a helper transactor identifier",
                                l.name.name,
                            ));
                        }
                    }
                } else {
                    self.errors.push(format!(
                        "let {} : {simple}: regblock instantiation requires `= bind <helper>` (a transactor with write/read methods)",
                        l.name.name,
                    ));
                }
                self.pad(depth);
                writeln!(self.out, "{simple}_Mirror {};", l.name.name).ok();
                // Per-register write-callback holder; slots are filled by
                // `on regs.REG ... end on` and fired from `record_write`.
                self.pad(depth);
                writeln!(self.out, "{simple}_Callbacks {}_cbs;", l.name.name).ok();
                // Recursion-depth counter for `record_write` -> callback ->
                // `record_write` cascades. Each entry into the decode block
                // bumps this, and we abort if it exceeds
                // `HARC_RAL_CB_MAX_DEPTH`. See docs/ral-support.md §3.2.
                self.pad(depth);
                writeln!(self.out, "uint32_t {}_cb_depth = 0;", l.name.name).ok();
                return;
            }
        }
        // Component binding: `let drv : AxilDrv = bind axil` for a
        // `driver` / `agent` / `monitor` declared `bound to BusType`.
        // The binding ties this driver instance to the named bus
        // binding so its on-handlers can use the bare `bus` identifier
        // for typed signal/handshake access. Emits the struct exactly
        // as the no-value path would, plus on-handler registrations
        // with the bus binding pushed for the duration of each body.
        if l.bind {
            if let Some(simple) = type_simple_name(l.ty.as_ref()) {
                // Bound transactor: `let xact : T mode = bind axil`
                // for a `transactor T bound to BusType`. Compose a
                // synthetic ComponentDecl from the transactor body —
                // including or excluding the `when active` items
                // based on the instantiation's mode — and route
                // through the existing bound-driver-actor +
                // bound-monitor-actor paths. (Spec §8.1, T-2 in the
                // implementation roadmap.)
                if let Some(t) = self.transactors.get(simple).cloned() {
                    // Mode is mandatory at the binding site for
                    // transactors. We read it from the let's
                    // TypeExpr::Named.mode field, populated by
                    // parse_let_stmt.
                    let mode = match l.ty.as_ref() {
                        Some(TypeExpr::Named { mode: Some(m), .. }) => *m,
                        _ => {
                            self.errors.push(format!(
                                "let {}: transactor instantiation requires a mode annotation (`{} active` or `{} passive`)",
                                l.name.name, simple, simple,
                            ));
                            return;
                        }
                    };
                    let include_active = matches!(mode, TransactorMode::Active);
                    let synth = synth_component_from_transactor(&t, include_active);

                    // Four shapes:
                    //   bound transactor + `= bind axil`  → bus-actor path
                    //   bound transactor + no value       → error
                    //   unbound transactor + no value     → handler-registration path (same as plain driver/agent)
                    //   unbound transactor + `= bind axil`→ error
                    match (&t.bound_to, &l.value) {
                        (Some(bus_ty), Some(v)) => {
                            if let ExprKind::Ident(rhs) = &*v.kind {
                                if let Some(binding) = self.bus_bindings.get(&rhs.name).cloned() {
                                    let want = type_simple_name(Some(bus_ty));
                                    if want != Some(binding.0.name.name.as_str()) {
                                        self.errors.push(format!(
                                            "let {} : {} = bind {}: transactor is bound to `{}`, but `{}` is a `{}`",
                                            l.name.name, simple, rhs.name,
                                            want.unwrap_or("?"),
                                            rhs.name, binding.0.name.name,
                                        ));
                                    }
                                    self.pad(depth);
                                    writeln!(self.out, "{simple} {};", l.name.name).ok();

                                    // Driver-actor path only fires for
                                    // active mode (passive synth has no
                                    // input event field, so it would
                                    // bail anyway — but skipping the
                                    // call is cleaner).
                                    if include_active {
                                        self.try_emit_bound_driver_actor(
                                            &synth,
                                            &l.name.name,
                                            depth,
                                            &binding,
                                        );
                                    }
                                    // Monitor-actor path: handshake
                                    // handlers in the always-present
                                    // body always fire, regardless of
                                    // mode. This is the observation
                                    // half of the transactor.
                                    self.emit_bound_monitor_actors(
                                        &synth,
                                        &l.name.name,
                                        depth,
                                        &binding,
                                    );
                                    self.emit_bound_tlm_target_actors(
                                        &synth,
                                        &l.name.name,
                                        depth,
                                        &binding,
                                    );
                                    return;
                                } else {
                                    self.errors.push(format!(
                                        "let {} : {} = bind {}: `{}` is not a known bus binding",
                                        l.name.name, simple, rhs.name, rhs.name,
                                    ));
                                    return;
                                }
                            }
                        }
                        (None, None) => {
                            // Unbound transactor: default-construct
                            // the struct, then walk the synth
                            // ComponentDecl to wire up `on`-handler
                            // subscribers. Same path as plain
                            // `driver`/`agent` instantiation. No bus
                            // actor coroutines spawn — the active
                            // half drives raw DUT signals (or
                            // sideband), reacting to `emit
                            // xact.req(t)` events from the test
                            // scope. The passive half observes via
                            // event-typed `on` handlers if present.
                            self.pad(depth);
                            writeln!(self.out, "{simple} {};", l.name.name).ok();
                            let tag = format!("_xactor_{}_", l.name.name);
                            self.emit_component_handler_registrations(
                                &synth,
                                &l.name.name,
                                depth,
                                &tag,
                            );
                            return;
                        }
                        (Some(_), None) => {
                            self.errors.push(format!(
                                "let {} : {}: transactor `{}` has a `bound to` clause; instantiation requires `= bind <bus>`",
                                l.name.name, simple, simple,
                            ));
                            return;
                        }
                        (None, Some(_)) => {
                            self.errors.push(format!(
                                "let {} : {} = bind ...: transactor `{}` has no `bound to` clause; remove the `= bind` clause",
                                l.name.name, simple, simple,
                            ));
                            return;
                        }
                    }
                    self.errors.push(format!(
                        "let {} : {} = bind <expr>: rhs must be a bare bus-binding name in v0",
                        l.name.name, simple,
                    ));
                    return;
                }
                // (Legacy bound `monitor MonName` instantiation path
                // is removed — `monitor` construct retired in PR-B.
                // Observation handlers live in transactor always-on
                // bodies, lowered via the synth ComponentDecl path
                // above.)
                if let Some(comp) = self.components.get(simple).cloned() {
                    if let Some(bus_ty) = &comp.bound_to {
                        // The rhs must name an existing bus binding (a
                        // previous `let X : BusType = bind dut`). Plain
                        // ident only in v0; richer expressions deferred.
                        if let Some(v) = &l.value {
                            if let ExprKind::Ident(rhs) = &*v.kind {
                                if let Some(binding) = self.bus_bindings.get(&rhs.name).cloned() {
                                    // Type-check: the bus binding's
                                    // BusDecl name must match the
                                    // driver's `bound to <BusType>`.
                                    let want = type_simple_name(Some(bus_ty));
                                    if want != Some(binding.0.name.name.as_str()) {
                                        self.errors.push(format!(
                                            "let {} : {} = bind {}: driver is bound to `{}`, but `{}` is a `{}`",
                                            l.name.name, simple, rhs.name,
                                            want.unwrap_or("?"),
                                            rhs.name, binding.0.name.name,
                                        ));
                                    }
                                    self.pad(depth);
                                    writeln!(self.out, "{simple} {};", l.name.name).ok();
                                    // Try the Phase 2b coroutine actor
                                    // path first: if the driver has a
                                    // single input event with a
                                    // matching `on <ev>(t)` handler,
                                    // spawn an independent coroutine
                                    // that loops over a transaction
                                    // queue. Falls through to the
                                    // sync subscriber path when the
                                    // driver isn't actor-shaped (no
                                    // input event, or multiple
                                    // handlers — the latter stays sync
                                    // for now to keep the model
                                    // tractable).
                                    if self.try_emit_bound_driver_actor(
                                        &comp,
                                        &l.name.name,
                                        depth,
                                        &binding,
                                    ) {
                                        return;
                                    }
                                    let tag = format!("_{}_", comp.kind.keyword());
                                    self.emit_component_handler_registrations_bound(
                                        &comp,
                                        &l.name.name,
                                        depth,
                                        &tag,
                                        Some(binding),
                                    );
                                    return;
                                } else {
                                    self.errors.push(format!(
                                        "let {} : {} = bind {}: `{}` is not a known bus binding (declare it first via `let {} : <BusType> = bind dut`)",
                                        l.name.name, simple, rhs.name, rhs.name, rhs.name,
                                    ));
                                    return;
                                }
                            }
                        }
                        self.errors.push(format!(
                            "let {} : {} = bind <expr>: rhs must be a bare bus-binding name in v0",
                            l.name.name, simple,
                        ));
                        return;
                    }
                }
            }
        }
        // Skip the `let dut : T` decl — already emitted at main() prelude.
        if l.name.name == "dut" {
            return;
        }
        // `let e : event<T>` — pub/sub primitive. Lower to a vector of
        // std::function<void(T)>; `on e(arg) body end on` registers a
        // closure; `emit e(v)` calls every subscriber. (Spec §3.4 + §7.3.)
        if let Some(TypeExpr::Builtin {
            name: BuiltinTy::Event,
            args,
            ..
        }) = &l.ty
        {
            let inner = args
                .first()
                .map(|a| match a {
                    TypeArg::Type(t) => self.record_field_c_type(t),
                    _ => "uint64_t".into(),
                })
                .unwrap_or_else(|| "uint64_t".into());
            self.pad(depth);
            writeln!(
                self.out,
                "std::vector<std::function<void({inner})>> {};",
                l.name.name
            )
            .ok();
            self.event_types.insert(l.name.name.clone(), inner);
            return;
        }
        if let Some(v) = &l.value {
            // `let x = fork bus.<method>(args)` issues the request now
            // and captures the response into x at a later `join_all`.
            if self.try_emit_bus_tlm_fork(v, Some(&l.name.name), depth) {
                return;
            }
            // `bus.<method>(args)` on the rhs expands the req/rsp
            // transaction-method protocol and captures the response
            // payload into the let when the method returns a value.
            if self.try_emit_bus_tlm_method(v, Some(&l.name.name), depth) {
                return;
            }
            // `bus.<ch>.recv()` on the rhs expands the handshake +
            // captures the payload signal directly into the let.
            // Defer pad emission so the handshake helper can write its
            // own indentation.
            if self.try_emit_bus_handshake(v, Some(&l.name.name), depth) {
                return;
            }
            // Instantiation-only declarations have no C++ value type
            // here. The generic path below types the local through
            // `local_value_c_type` → `c_type_for`, whose
            // `TypeExpr::Named` arm means one specific thing —
            // "Verilator DUT handle" — and yields `V<Name>*`. That is
            // right for `let dut : <SvTopModule>` and wrong for every
            // HARC-declared type, because no DUT defines a `VDrv`. The
            // user then reads clang's `unknown type name 'VDrv'`
            // against a generated line they never wrote, with nothing
            // pointing back at the `let`.
            //
            // Every legal instantiation form for these families returns
            // from an arm above (bus bind, RAL bind, component/
            // transactor bind, the no-initializer path). Reaching here
            // with one of their type names means the form is not a
            // supported one — say so, and name the forms that are.
            if let Some(simple) = type_simple_name(l.ty.as_ref()) {
                if let Some((what, forms)) = self.instantiation_only_forms(simple) {
                    self.errors.push(format!(
                        "let {} : {simple} = <expr>: {what} has no value form, so an \
                         initializer is not one of its instantiation forms. Use {forms}.",
                        l.name.name,
                    ));
                    return;
                }
            }
            self.pad(depth);
            // Explicitly typed initialized locals must keep their
            // declared scalar width. In particular, `let x : uint<75> =
            // ...` needs `_harc_u128`, not the historical `int64_t`
            // fallback used for untyped integer-shaped lets.
            let ty = if let Some(t) = &l.ty {
                self.local_value_c_type(t)
            } else if rhs_wants_auto(v, &self.tseq_names) {
                "auto".into()
            } else {
                // Default to `int64_t` for untyped integer-shaped lets
                // so 32-bit DUT signals zero-extend on assignment
                // (matters for the `assert got == expected` pattern
                // when comparing widened C++ ints against narrow
                // Verilator outputs). Switch to `auto` when the rhs is
                // a call — function/tseq/method returns can be
                // `std::vector<T>` or a transaction value, neither of
                // which fit in int64_t.
                "int64_t".into()
            };
            write!(self.out, "{ty} {} = ", l.name.name).ok();
            self.emit_expr(v);
            writeln!(self.out, ";").ok();
        } else if let Some(t) = &l.ty {
            self.pad(depth);
            // No initializer. Transactions / scoreboards default-
            // construct (their struct field defaults run). Transactors
            // additionally register `on`-handlers (see the unbound-
            // transactor arm below).
            let simple = type_simple_name(Some(t));
            if let Some(name) = simple {
                if self.is_record_type(name) || self.scoreboards.contains(name) {
                    writeln!(self.out, "{name} {};", l.name.name).ok();
                    return;
                }
                // Unbound transactor: `let xact : T mode` for a
                // `transactor T` declared without `bound to BusType`.
                // Same lowering as plain `driver`/`agent` instantiation —
                // default-construct the struct, then walk the synth
                // ComponentDecl to register `on`-handler subscribers.
                // The active half drives raw DUT signals (or sideband)
                // reacting to `emit xact.req(t)`; the passive half (if
                // present) observes via event-typed `on` handlers.
                if let Some(t) = self.transactors.get(name).cloned() {
                    let mode = match l.ty.as_ref() {
                        Some(TypeExpr::Named { mode: Some(m), .. }) => *m,
                        _ => {
                            self.errors.push(format!(
                                "let {}: transactor instantiation requires a mode annotation (`{} active` or `{} passive`)",
                                l.name.name, name, name,
                            ));
                            return;
                        }
                    };
                    if t.bound_to.is_some() {
                        self.errors.push(format!(
                            "let {} : {}: transactor `{}` has a `bound to` clause; instantiation requires `= bind <bus>`",
                            l.name.name, name, name,
                        ));
                        return;
                    }
                    let include_active = matches!(mode, TransactorMode::Active);
                    let synth = synth_component_from_transactor(&t, include_active);
                    writeln!(self.out, "{name} {};", l.name.name).ok();
                    let tag = format!("_xactor_{}_", l.name.name);
                    self.emit_component_handler_registrations(&synth, &l.name.name, depth, &tag);
                    return;
                }
                if let Some(comp) = self.components.get(name).cloned() {
                    // driver / agent / env / sequencer → default-construct
                    // the struct. The user assigns DUT pointers and other
                    // field values explicitly afterward (`drv.dut = dut;`).
                    writeln!(self.out, "{name} {};", l.name.name).ok();
                    // Register on-handlers in the body the same way
                    // monitors do — events get subscriber closures,
                    // bool exprs get cycle-trigger checkers. Tag prefix
                    // is the kind keyword so concurrent components at
                    // the same source span don't collide.
                    let tag = format!("_{}_", comp.kind.keyword());
                    self.emit_component_handler_registrations(&comp, &l.name.name, depth, &tag);
                    // Recurse through sub-component / sub-transactor
                    // fields with mode propagation. The let-instance's
                    // mode is the root of the inheritance chain:
                    //
                    //   let topenv : OuterEnv passive
                    //                ^^^^^^^^^^^^^^^^^
                    //   propagates →  env's agent fields         (no mode → passive)
                    //   propagates →    agent's transactor fields (no mode → passive)
                    //
                    // At each level a field-level explicit mode wins
                    // over the inherited mode. See
                    // `emit_subcomponent_handler_registrations`.
                    let root_mode = match l.ty.as_ref() {
                        Some(TypeExpr::Named { mode: Some(m), .. }) => Some(*m),
                        _ => None,
                    };
                    self.emit_subcomponent_handler_registrations(
                        &comp,
                        &l.name.name,
                        root_mode,
                        depth,
                    );
                    // Top-level watchdog (spec §8.6). Install a periodic
                    // `_checkers` closure that fires `<Type>_watchdog(<inst>)`
                    // every `period` cycles. Sub-component watchdogs are
                    // installed by `emit_subcomponent_handler_registrations`.
                    for ci in &comp.items {
                        if let ComponentItem::Watchdog(w) = ci {
                            self.emit_watchdog_checker(&comp, w, &l.name.name, depth);
                        }
                    }
                    // Top-level connect edges (the let's own component's
                    // connect block, if any). Nested sub-component
                    // connects are emitted inside the recursive helper
                    // above so they're prefixed by the sub-instance path.
                    for ci in &comp.items {
                        if let ComponentItem::Connect(cb) = ci {
                            for edge in &cb.edges {
                                self.emit_connect_edge(&comp, &l.name.name, edge, depth);
                            }
                        }
                    }
                    return;
                }
                if let Some(g) = self.covergroups.get(name).cloned() {
                    writeln!(self.out, "{name} {};", l.name.name).ok();
                    // Register an inline sample closure (per-cycle level
                    // trigger). Spec §6 says sampling fires on the
                    // covergroup's clocking expression — for our v0
                    // single-clock primary-clock model, every tick is a
                    // sample. Multi-clock would tie this to the named
                    // clock instead.
                    self.emit_covergroup_sample_registration(&g, &l.name.name, depth);
                    return;
                }
            }
            writeln!(self.out, "int64_t {} = 0;", l.name.name).ok();
        } else {
            self.pad(depth);
            writeln!(self.out, "// let {} (no type / no value)", l.name.name).ok();
        }
    }
}
