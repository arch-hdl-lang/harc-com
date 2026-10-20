//! Native C++ component, hook, watchdog, and scoreboard rendering.
//!
//! These helpers emit component-owned declarations and behavior after path
//! and activation-mode resolution has selected the target schema.

use super::*;

impl Emitter {
    /// Emit the two `<Type>_<method>_pre` and `<Type>_<method>_post`
    /// `std::vector<std::function<void(args)>>` declarations that
    /// hold the registered hook subscribers for one hookable method.
    /// Empty by default; users push closures via `on obj.method pre`
    /// / `on obj.method post` at test scope.
    pub(super) fn emit_hook_vectors(
        &mut self,
        c: &ComponentDecl,
        h: &HookableMethod,
        depth: usize,
    ) {
        // `function` methods (testbench / env helpers, docs/test-
        // ergonomics.md §3.2) carry `is_hookable = false`: no pre/post
        // vectors emitted, and the corresponding fan-out in
        // `emit_component_method` is suppressed. The method itself
        // still emits as a free `<Type>_<method>` lambda.
        if !h.is_hookable {
            return;
        }
        let comp_ty = &c.name.name;
        let m_name = &h.name.name;
        let arg_tys: Vec<String> = h
            .params
            .iter()
            .map(|p| {
                p.ty.as_ref()
                    .map(|t| self.c_type_for_param(t))
                    .unwrap_or_else(|| "int64_t".to_string())
            })
            .collect();
        let arg_csv = arg_tys.join(", ");
        // Common layout: the vectors live on the suite runtime (one
        // registry per generated instance), so the run-scope
        // declarations are skipped; the capsule binds aliases instead.
        if self.flavor == Flavor::Legacy {
            self.pad(depth);
            writeln!(
                self.out,
                "std::vector<std::function<void({arg_csv})>> {comp_ty}_{m_name}_pre;",
            )
            .ok();
            self.pad(depth);
            writeln!(
                self.out,
                "std::vector<std::function<void({arg_csv})>> {comp_ty}_{m_name}_post;",
            )
            .ok();
        }
    }

    /// Emit a single `hookable` method on a component as a free
    /// `[&]`-capturing lambda named `<Type>_<method>`. The first
    /// parameter is `<Type>& self`; subsequent parameters mirror the
    /// HARC declaration. Inside the body, bare references to component
    /// fields are rewritten to `self.<field>` via `field_subs`. DUT
    /// pointer fields stay arrow-accessed: `self.dut->aw_addr = ...`.
    pub(super) fn emit_component_method(
        &mut self,
        c: &ComponentDecl,
        h: &HookableMethod,
        depth: usize,
    ) {
        let comp_ty = &c.name.name;
        let m_name = &h.name.name;
        let param_names = cpp_param_names(&h.params);
        self.declared_value_names
            .extend(param_names.iter().cloned());
        let method_probe_receivers = self.component_probe_receivers(c);
        let prior_probe_receivers =
            std::mem::replace(&mut self.probe_receivers, method_probe_receivers);
        self.push_probe_shadow_scope();
        for param_name in &param_names {
            self.declare_probe_shadow(param_name);
        }
        let ret = h
            .return_ty
            .as_ref()
            .map(|t| self.c_type_for_value(t))
            .unwrap_or_else(|| "void".to_string());
        self.pad(depth);
        // Common layout: out-of-line member of the per-run glue object
        // (defined once in the common TU). Legacy — and bus-bound
        // hookables under the common layout — emit a `[&]` lambda inside
        // `run_<Test>`. The body machinery below is shared verbatim.
        let common_member = self.is_glue_member(&format!("{comp_ty}_{m_name}"));
        if common_member {
            write!(
                self.out,
                "{ret} HarcSuiteGlue::{comp_ty}_{m_name}({comp_ty}& self"
            )
            .ok();
        } else {
            write!(self.out, "auto {comp_ty}_{m_name} = [&]({comp_ty}& self").ok();
        }
        // Track Named-typed params as pointers so dut.field rewrites
        // properly in the body. Restore on exit. Transaction / enum /
        // sub-component params are by-value (not pointer-shaped).
        let mut added: Vec<String> = Vec::new();
        let mut saved_param_types = Vec::new();
        for (i, p) in h.params.iter().enumerate() {
            let pty =
                p.ty.as_ref()
                    .map(|t| self.c_type_for_param(t))
                    .unwrap_or_else(|| "int64_t".to_string());
            write!(self.out, ", {pty} {}", param_names[i]).ok();
            if p.name.name != "_" {
                if let Some(ty) = type_simple_name(p.ty.as_ref()) {
                    if self.components.contains_key(ty) || self.transactors.contains_key(ty) {
                        let name = p.name.name.clone();
                        let prev = self.let_types.insert(name.clone(), ty.to_string());
                        saved_param_types.push((name, prev));
                    }
                }
            }
            if matches!(&p.ty, Some(TypeExpr::Named { .. }))
                && self.is_dut_pointer_field_type(p.ty.as_ref().unwrap())
                && p.name.name != "_"
            {
                if self.pointer_vars.insert(p.name.name.clone()) {
                    added.push(p.name.name.clone());
                }
            }
        }
        if common_member {
            writeln!(self.out, ") {{").ok();
        } else {
            writeln!(self.out, ") -> {ret} {{").ok();
        }
        // Build field-name substitution: bare `count` inside the body
        // resolves to `self.count`. Dut-pointer fields also get a
        // pointer_vars entry so `dut.field` lowers to `dut->field`.
        let mut subs = std::collections::HashMap::new();
        let mut added_pointer_fields: Vec<String> = Vec::new();
        let mut saved_field_types = Vec::new();
        for ci in &c.items {
            if let ComponentItem::Field(f) = ci {
                subs.insert(f.name.name.clone(), format!("self.{}", f.name.name));
                if let Some(ty) = type_simple_name(Some(&f.ty)) {
                    let name = f.name.name.clone();
                    let prev = self.let_types.insert(name.clone(), ty.to_string());
                    saved_field_types.push((name, prev));
                }
                if self.is_dut_pointer_field_type(&f.ty) {
                    if self.pointer_vars.insert(f.name.name.clone()) {
                        added_pointer_fields.push(f.name.name.clone());
                    }
                }
            }
        }
        let prev_subs = std::mem::replace(&mut self.field_subs, subs);

        // For a `bound to BusType` driver, the parent's bus binding
        // (resolved at codegen time from the test's `let drv : Drv =
        // bind axil` statement) propagates into hookable method
        // bodies. Inside the body, the bare identifier `bus` resolves
        // to the same `(BusDecl, root, prefix)` tuple as the
        // test-scope binding, so `bus.<ch>.send(...)`, `bus.<ch>.recv()`,
        // and `bus.<ch>.<sig>` all work identically to the patterns
        // available inside `on T t` handlers.
        //
        // Restriction (single-instance): we use the FIRST binding
        // discovered for this driver type. If the test instantiates
        // two `let A : Drv = bind X` and `let B : Drv = bind Y` with
        // different buses, both share A's binding inside the
        // hookable body. v0 doesn't have a use case for
        // multi-instance bound drivers; per-instance hookable
        // emission is a follow-up.
        let pushed_bus = self.driver_bus_for_hookables.get(comp_ty).cloned();
        let prior_bus = pushed_bus
            .as_ref()
            .and_then(|b| self.bus_bindings.insert("bus".into(), b.clone()));

        // Pre-hooks: fire `<Type>_<method>_pre` subscribers before the
        // body. The hook closures see the same args as the method —
        // empty vectors are a no-op so the wrap is always safe to
        // emit.
        let arg_csv = param_names.join(", ");
        // Pre/post hook fan-out is skipped for non-hookable `function`
        // methods (docs/test-ergonomics.md §3.2): no vectors were
        // emitted for them, so the fan-out would reference undeclared
        // symbols.
        if h.is_hookable {
            self.pad(depth + 1);
            writeln!(
                self.out,
                "for (auto& _h : {comp_ty}_{m_name}_pre) _h({arg_csv});"
            )
            .ok();
        }

        let current_method_active = self.method_lives_in_when_active(comp_ty, m_name);
        let prior_component_method = std::mem::replace(
            &mut self.current_component_method,
            Some((
                comp_ty.clone(),
                "self".to_string(),
                m_name.clone(),
                current_method_active,
            )),
        );
        self.emit_block(&h.body, depth + 1);
        self.current_component_method = prior_component_method;

        if h.is_hookable {
            self.pad(depth + 1);
            writeln!(
                self.out,
                "for (auto& _h : {comp_ty}_{m_name}_post) _h({arg_csv});"
            )
            .ok();
        }
        let _ = arg_csv;
        // Restore state.
        if pushed_bus.is_some() {
            match prior_bus {
                Some(prev) => {
                    self.bus_bindings.insert("bus".into(), prev);
                }
                None => {
                    self.bus_bindings.remove("bus");
                }
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
        for (name, prev) in saved_param_types {
            match prev {
                Some(ty) => {
                    self.let_types.insert(name, ty);
                }
                None => {
                    self.let_types.remove(&name);
                }
            }
        }
        for k in added_pointer_fields {
            self.pointer_vars.remove(&k);
        }
        for k in added {
            self.pointer_vars.remove(&k);
        }
        self.pop_probe_shadow_scope();
        self.probe_receivers = prior_probe_receivers;
        if common_member {
            self.pad(depth);
            writeln!(self.out, "}}").ok();
        } else {
            self.pad(depth);
            writeln!(self.out, "}};").ok();
        }
    }

    /// Default watchdog period (cycles) when the user writes `watchdog`
    /// without an explicit `period` clause. Spec §8.6.
    const WATCHDOG_DEFAULT_PERIOD: i64 = 1000;
    /// Default watchdog idle threshold (cycles) when the user writes
    /// `watchdog` without an explicit `max_idle` clause.
    const WATCHDOG_DEFAULT_MAX_IDLE: i64 = 10000;

    /// Emit hook-vector declarations + the synthetic `<Type>_watchdog`
    /// method for a component's `watchdog` block (spec §8.6). The
    /// method body, in order:
    ///   1. Pre-hooks (`<Type>_watchdog_pre`) — for `on <Type>.watchdog
    ///      pre` aspect attachments.
    ///   2. User-supplied body statements (typically `log(info, …)`
    ///      debug prints). Field references rewrite to `self.<field>`
    ///      via the same `field_subs` mechanism used for hookable
    ///      methods.
    ///   3. The idle check: if BOTH `_last_in_cycle` AND
    ///      `_last_out_cycle` are ≥ `max_idle` cycles behind
    ///      `cycle_count`, log `FAIL` with a watchdog-specific message
    ///      and bump `errors`.
    ///   4. Post-hooks (`<Type>_watchdog_post`).
    ///
    /// `disabled` watchdogs emit nothing — no hook vectors, no method.
    /// External `on <Type>.watchdog pre/post` referencing a disabled
    /// component's watchdog will surface as a missing-symbol C++
    /// compile error, which is the intended signal that the aspect
    /// has no target. (Reasonable people who turned the watchdog
    /// off will know to remove their hooks.)
    pub(super) fn emit_watchdog(&mut self, c: &ComponentDecl, w: &WatchdogDecl, depth: usize) {
        if w.disabled {
            return;
        }
        let watchdog_probe_receivers = self.component_probe_receivers(c);
        let prior_probe_receivers =
            std::mem::replace(&mut self.probe_receivers, watchdog_probe_receivers);
        let comp_ty = &c.name.name;
        // Hook vectors — `watchdog` takes no args, so `void()` signature.
        // Common layout: they live on the suite runtime instead.
        if self.flavor == Flavor::Legacy {
            self.pad(depth);
            writeln!(
                self.out,
                "std::vector<std::function<void()>> {comp_ty}_watchdog_pre;"
            )
            .ok();
            self.pad(depth);
            writeln!(
                self.out,
                "std::vector<std::function<void()>> {comp_ty}_watchdog_post;"
            )
            .ok();
        }
        // The method itself: a `[&]`-capturing lambda parallelling the
        // shape of `emit_component_method` so the hookable-dispatch
        // path (`<Type>_<method>(obj, args)`) finds the same symbol.
        self.pad(depth);
        let common_member = self.is_glue_member(&format!("{comp_ty}_watchdog"));
        if common_member {
            writeln!(
                self.out,
                "void HarcSuiteGlue::{comp_ty}_watchdog({comp_ty}& self) {{"
            )
            .ok();
        } else {
            writeln!(
                self.out,
                "auto {comp_ty}_watchdog = [&]({comp_ty}& self) -> void {{"
            )
            .ok();
        }
        // Field substitution: bare `wdog_max_idle` inside the user body
        // resolves to `self.wdog_max_idle`. Same shape as
        // emit_component_method's `subs` setup.
        let mut subs = std::collections::HashMap::new();
        let mut added_pointer_fields: Vec<String> = Vec::new();
        for ci in &c.items {
            if let ComponentItem::Field(f) = ci {
                subs.insert(f.name.name.clone(), format!("self.{}", f.name.name));
                if self.is_dut_pointer_field_type(&f.ty) {
                    if self.pointer_vars.insert(f.name.name.clone()) {
                        added_pointer_fields.push(f.name.name.clone());
                    }
                }
            }
        }
        let prev_subs = std::mem::replace(&mut self.field_subs, subs);

        // Pre-hooks.
        self.pad(depth + 1);
        writeln!(self.out, "for (auto& _h : {comp_ty}_watchdog_pre) _h();").ok();

        // User body (typically debug logging).
        self.emit_block(&w.body, depth + 1);

        // Idle check. We emit the C++ directly (not through the HARC
        // `idle()` predicate dispatch) because the receiver is `self`,
        // not a let-bound variable, and `resolve_component_idle_predicate`
        // expects to resolve through `let_types`. Direct C++ keeps
        // the synthetic method self-contained and avoids polluting
        // let_types with the synthetic name `self`.
        self.pad(depth + 1);
        write!(self.out, "int64_t _wdog_max_idle = (int64_t)(").ok();
        if let Some(m) = &w.max_idle {
            self.emit_expr(m);
        } else {
            write!(self.out, "{}", Self::WATCHDOG_DEFAULT_MAX_IDLE).ok();
        }
        writeln!(self.out, ");").ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "if (_wdog_max_idle > 0 \
             && (int64_t)((uint64_t)cycle_count - self._last_in_cycle) >= _wdog_max_idle \
             && (int64_t)((uint64_t)cycle_count - self._last_out_cycle) >= _wdog_max_idle) {{"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "sim_log_line(\"FAIL\", \"watchdog: {comp_ty} has been idle for >= %lld cycles\", \
             (long long)_wdog_max_idle);"
        )
        .ok();
        self.pad(depth + 2);
        let runtime_context = self.randomize_context_ref().to_string();
        writeln!(self.out, "{runtime_context}.errors++;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();

        // Post-hooks.
        self.pad(depth + 1);
        writeln!(self.out, "for (auto& _h : {comp_ty}_watchdog_post) _h();").ok();

        // Restore state.
        self.field_subs = prev_subs;
        for k in added_pointer_fields {
            self.pointer_vars.remove(&k);
        }
        self.probe_receivers = prior_probe_receivers;
        self.pad(depth);
        if common_member {
            writeln!(self.out, "}}").ok();
        } else {
            writeln!(self.out, "}};").ok();
        }
    }

    /// Emit a periodic `_checkers` closure that calls `<Type>_watchdog(<inst>)`
    /// every `period` cycles. Installed at every `let foo : Agent` site
    /// (including sub-component composition). The closure re-reads
    /// the period expression each cycle so per-test overrides via
    /// field assignment work without re-installation.
    pub(super) fn emit_watchdog_checker(
        &mut self,
        c: &ComponentDecl,
        w: &WatchdogDecl,
        instance: &str,
        depth: usize,
    ) {
        if w.disabled {
            return;
        }
        let watchdog_probe_receivers = self.component_probe_receivers(c);
        let prior_probe_receivers =
            std::mem::replace(&mut self.probe_receivers, watchdog_probe_receivers);
        let comp_ty = &c.name.name;
        // The instance path may contain dots (`env.agent`); the static
        // tag needs to be a valid C++ identifier, so flatten dots to
        // underscores.
        let inst_tag = instance.replace('.', "_");
        // Field substitution: a period expression like `wdog_period`
        // resolves to `<instance>.wdog_period` (parallels how field
        // refs inside `on event(t)` bodies work at let-time).
        let mut subs = std::collections::HashMap::new();
        for ci in &c.items {
            if let ComponentItem::Field(f) = ci {
                subs.insert(f.name.name.clone(), format!("{instance}.{}", f.name.name));
            }
        }
        let prev_subs = std::mem::replace(&mut self.field_subs, subs);

        self.pad(depth);
        writeln!(self.out, "_checkers.push_back([&]() {{").ok();
        let wdog_last = self.per_run_cell(format!("_wdog_{inst_tag}_last"), "int64_t", Some("0"));
        self.pad(depth + 1);
        write!(self.out, "int64_t _wdog_{inst_tag}_period = (int64_t)(").ok();
        if let Some(p) = &w.period {
            self.emit_expr(p);
        } else {
            write!(self.out, "{}", Self::WATCHDOG_DEFAULT_PERIOD).ok();
        }
        writeln!(self.out, ");").ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "if (_wdog_{inst_tag}_period > 0 \
             && (int64_t)cycle_count - {wdog_last} >= _wdog_{inst_tag}_period) {{"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "{wdog_last} = (int64_t)cycle_count;").ok();
        self.pad(depth + 2);
        let glue = if self.is_glue_member(&format!("{comp_ty}_watchdog")) {
            "_glue."
        } else {
            ""
        };
        writeln!(self.out, "{glue}{comp_ty}_watchdog({instance});").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        self.pad(depth);
        writeln!(self.out, "}});").ok();

        self.field_subs = prev_subs;
        self.probe_receivers = prior_probe_receivers;
    }

    /// Emit a `driver` / `agent` / `env` / `sequencer` body as a C++
    /// struct. Field types are resolved through `component_field_c_type`,
    /// which recognizes:
    ///
    /// - `event<T>` → `std::vector<std::function<void(T)>>`
    /// - `queue<T>` → `harc_rt::HarcQueue<T>` (when scoreboards are also present)
    /// - Named DUT module → `V<Name>*` pointer (the same shape the
    ///   test-level `let dut : T` already uses)
    /// - Named sub-component → that struct directly (for `env` composing
    ///   `agent` / `driver` / etc. by-value)
    /// - Builtins → uint64_t / int64_t / bool
    ///
    /// `hookable` methods are emitted separately as free `[&]`-capturing
    /// lambdas (see `emit_component_methods`); the struct body holds
    /// data only.
    pub(super) fn emit_component_struct(&mut self, c: &ComponentDecl) {
        writeln!(self.out, "struct {} {{", c.name.name).ok();
        if c.kind == ComponentKind::Sequencer {
            let unique_registry =
                component_unique_registry_name(c.items.iter().filter_map(|item| {
                    let ComponentItem::Field(field) = item else {
                        return None;
                    };
                    Some(field.name.name.as_str())
                }));
            writeln!(
                self.out,
                "{INDENT}harc_rt::random::HarcUniqueRegistry {unique_registry};"
            )
            .ok();
        }
        for it in &c.items {
            if let ComponentItem::Field(f) = it {
                let cty = self.component_field_c_type(&f.ty);
                let init = if let Some(d) = &f.default {
                    format!(" = {}", format_simple_expr(d))
                } else if matches!(&f.ty, TypeExpr::Named { .. })
                    && self.is_dut_pointer_field_type(&f.ty)
                {
                    // Pointer fields default to nullptr — caller assigns
                    // via `drv.dut = dut` after construction.
                    " = nullptr".into()
                } else if matches!(
                    &f.ty,
                    TypeExpr::Builtin {
                        name: BuiltinTy::Vec,
                        ..
                    }
                ) {
                    "{}".into()
                } else {
                    "".into()
                };
                writeln!(self.out, "{INDENT}{cty} {}{};", f.name.name, init).ok();
            }
        }
        // Auto-injected activity-tracking fields (spec §7.x). Bumped by
        // codegen at every place the framework knows an in/out has just
        // happened — `on <ev>` handler body entry, bus handshake actor
        // body entry, `bus.<ch>.send/recv`, `emit ev(arg)` — and read
        // by the `obj.idle(N)` / `idle_in(N)` / `idle_out(N)` predicate
        // lowering. Initial value 0 means "no activity yet"; the
        // predicate `idle(N)` correctly reports false until at least N
        // cycles have elapsed since the last bump.
        writeln!(self.out, "{INDENT}uint64_t _last_in_cycle = 0;").ok();
        writeln!(self.out, "{INDENT}uint64_t _last_out_cycle = 0;").ok();
        writeln!(self.out, "}};").ok();
        writeln!(self.out, "").ok();
    }

    pub(super) fn emit_scoreboard(&mut self, s: &ComponentDecl) {
        writeln!(self.out, "struct {} {{", s.name.name).ok();
        for it in &s.items {
            if let ComponentItem::Field(f) = it {
                let cty = self.scoreboard_field_c_type(&f.ty);
                let init = if let Some(d) = &f.default {
                    format!(" = {}", format_simple_expr(d))
                } else if matches!(
                    &f.ty,
                    TypeExpr::Builtin {
                        name: BuiltinTy::Vec,
                        ..
                    }
                ) {
                    "{}".into()
                } else {
                    "".into()
                };
                writeln!(self.out, "{INDENT}{cty} {}{};", f.name.name, init).ok();
            }
        }
        // Auto-injected activity-tracking fields (spec §7.x). Same shape
        // as for driver/agent/env/sequencer in `emit_component_struct`
        // — scoreboards also count as components, so `sb.idle(N)` is
        // a valid watchdog predicate.
        writeln!(self.out, "{INDENT}uint64_t _last_in_cycle = 0;").ok();
        writeln!(self.out, "{INDENT}uint64_t _last_out_cycle = 0;").ok();
        writeln!(self.out, "}};").ok();
        writeln!(self.out, "").ok();
    }
}
