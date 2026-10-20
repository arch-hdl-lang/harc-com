//! Native C++ bus handshake and bound-field access emission.

use super::*;

impl Emitter {
    /// Detect `<bus>.<channel>.send(args)` / `<bus>.<channel>.recv()`
    /// calls and emit the auto-generated valid/ready handshake. The
    /// channel must be a `handshake_channel` on the bus the binding
    /// names; the call's arity (for `send`) must match the channel's
    /// payload signal count.
    ///
    /// `let_name = Some("x")` for the let-rhs form (`let x = bus.r.recv()`):
    /// the captured payload is assigned into `auto x = ...;`. With
    /// `None` the call is a discarded statement (drives the dance,
    /// throws away any received data).
    ///
    /// Returns `true` when the call was a recognized bus handshake
    /// and was emitted; `false` to fall through to plain Call lowering.
    pub(super) fn try_emit_bus_handshake(
        &mut self,
        e: &Expr,
        let_name: Option<&str>,
        depth: usize,
    ) -> bool {
        let ExprKind::Call { callee, args } = &*e.kind else {
            return false;
        };
        let ExprKind::Field {
            target,
            name: method,
        } = &*callee.kind
        else {
            return false;
        };
        let ExprKind::Field {
            target: outer,
            name: ch,
        } = &*target.kind
        else {
            return false;
        };
        let ExprKind::Ident(id) = &*outer.kind else {
            return false;
        };
        let Some((bus, root, sig_prefix)) = self.bus_bindings.get(&id.name).cloned() else {
            return false;
        };
        let Some(h) = bus
            .handshakes
            .iter()
            .find(|h| h.name.name == ch.name)
            .cloned()
        else {
            return false;
        };
        let valid_port = self.bus_signal_name(&sig_prefix, &ch.name, "valid");
        let ready_port = self.bus_signal_name(&sig_prefix, &ch.name, "ready");

        match method.name.as_str() {
            "send" => {
                if args.len() != h.payload.len() {
                    self.errors.push(format!(
                        "bus.{}.send: expected {} payload arg(s), got {}",
                        ch.name,
                        h.payload.len(),
                        args.len(),
                    ));
                    return true;
                }
                if let_name.is_some() {
                    self.errors.push(format!(
                        "bus.{}.send returns no value; use it as a statement",
                        ch.name,
                    ));
                    return true;
                }
                self.pad(depth);
                writeln!(self.out, "// bus.{}.send", ch.name).ok();
                // Drive payload signals.
                for (sig, arg) in h.payload.iter().zip(args.iter()) {
                    let sig_port = self.bus_signal_name(&sig_prefix, &ch.name, &sig.name.name);
                    self.pad(depth);
                    write!(self.out, "{root}->{sig_port} = ").ok();
                    match arg {
                        CallArg::Expr(e) => self.emit_expr(e),
                        CallArg::Named { value, .. } => self.emit_expr(value),
                    }
                    writeln!(self.out, ";").ok();
                }
                self.pad(depth);
                writeln!(self.out, "{root}->{valid_port} = 1;").ok();
                // Wait until ready=1 (bounded). The bound matches the sync
                // 16-cycle budget so a stuck DUT terminates the test (now
                // with a FAIL diagnostic) rather than hanging.
                let advance = if self.in_coroutine {
                    "co_await harc_rt::wait_cycles(_slot, 1)"
                } else {
                    "tick()"
                };
                self.pad(depth);
                writeln!(
                    self.out,
                    "{}",
                    crate::codegen::bounded_handshake_wait(
                        &format!("{root}->{ready_port}"),
                        crate::codegen::TLM_WAIT_BOUND,
                        advance,
                        &format!("handshake {}.send ready", ch.name),
                    )
                )
                .ok();
                self.pad(depth);
                writeln!(self.out, "{advance};").ok();
                self.pad(depth);
                writeln!(self.out, "{root}->{valid_port} = 0;").ok();
                // Activity tracking (spec §7.x): a completed
                // bus.<ch>.send counts as an "out" for the surrounding
                // component instance, if any. Bus calls inside
                // free/test-run code (no enclosing component) skip the
                // bump.
                if let Some(inst) = self.current_component_instance.clone() {
                    self.pad(depth);
                    writeln!(self.out, "{inst}._last_out_cycle = (uint64_t)cycle_count;").ok();
                }
                true
            }
            "recv" => {
                if !args.is_empty() {
                    self.errors.push(format!(
                        "bus.{}.recv: expected 0 args, got {}",
                        ch.name,
                        args.len(),
                    ));
                    return true;
                }
                if h.payload.is_empty() {
                    self.errors.push(format!(
                        "bus.{}.recv: channel has no payload signals to receive",
                        ch.name,
                    ));
                    return true;
                }
                // Multi-payload receive captures the full payload into
                // a per-channel struct (`<BusName>_<chan>_payload`),
                // emitted at file scope. The struct exposes an implicit
                // conversion to the first payload field's C++ type so
                // pre-existing scalar usage (`assert val == 0xCAFE`,
                // `field = val`) keeps compiling without change. Users
                // who want named field access write `val.data`,
                // `val.resp`, etc.
                self.pad(depth);
                writeln!(self.out, "// bus.{}.recv", ch.name).ok();
                self.pad(depth);
                writeln!(self.out, "{root}->{ready_port} = 1;").ok();
                self.pad(depth);
                writeln!(
                    self.out,
                    "{}",
                    crate::codegen::bounded_handshake_wait(
                        &format!("{root}->{valid_port}"),
                        crate::codegen::TLM_WAIT_BOUND,
                        if self.in_coroutine {
                            "co_await harc_rt::wait_cycles(_slot, 1)"
                        } else {
                            "tick()"
                        },
                        &format!("handshake {}.recv valid", ch.name),
                    )
                )
                .ok();
                // Capture BEFORE the trailing tick: the destination signals
                // are valid in the same cycle as `valid` is high.
                if let Some(name) = let_name {
                    self.pad(depth);
                    let struct_name = format!("{}_{}_payload", bus.name.name, ch.name);
                    write!(self.out, "{struct_name} {name} = {{").ok();
                    let mut first = true;
                    for sig in &h.payload {
                        if !first {
                            write!(self.out, ", ").ok();
                        }
                        first = false;
                        let sig_port = self.bus_signal_name(&sig_prefix, &ch.name, &sig.name.name);
                        write!(self.out, "{root}->{sig_port}").ok();
                    }
                    writeln!(self.out, "}};").ok();
                }
                self.pad(depth);
                if self.in_coroutine {
                    writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
                } else {
                    writeln!(self.out, "tick();").ok();
                }
                self.pad(depth);
                writeln!(self.out, "{root}->{ready_port} = 0;").ok();
                // Activity tracking (spec §7.x): a completed
                // bus.<ch>.recv counts as an "in" for the surrounding
                // component instance, if any.
                if let Some(inst) = self.current_component_instance.clone() {
                    self.pad(depth);
                    writeln!(self.out, "{inst}._last_in_cycle = (uint64_t)cycle_count;").ok();
                }
                true
            }
            _ => false,
        }
    }

    /// Is a plain bus signal present (not gated OFF) at this bind site?
    ///
    /// Looks up the named signal on the bus; if it carries a `generate_if`
    /// gate, evaluates it against the bind's effective param env (falling back
    /// to the bus's param defaults if no explicit env was recorded). Returns
    /// `false` only when the signal exists but its gate is provably false —
    /// i.e. `arch build` would have omitted the port. Unknown signal ⇒ `false`
    /// (let the caller's not-found path fire). Unfoldable gate ⇒ `true`
    /// (conservative: present, never a silent drop).
    fn bus_signal_present(&self, binding_key: &str, bus: &BusDecl, sig_name: &str) -> bool {
        let Some(sig) = bus.signals.iter().find(|s| s.name.name == sig_name) else {
            return false;
        };
        let owned_env;
        let env = match self.bus_param_envs.get(binding_key) {
            Some(e) => e,
            None => {
                owned_env = bus_param_env(bus, None);
                &owned_env
            }
        };
        gate_passes(sig.gate.as_ref(), env)
    }

    pub(super) fn try_emit_bus_field_access(
        &mut self,
        target: &Expr,
        name: &Ident,
    ) -> Option<String> {
        // <binding>.<signal>
        if let ExprKind::Ident(id) = &*target.kind {
            if let Some((bus, root, sig_prefix)) = self.bus_bindings.get(&id.name).cloned() {
                // Honor the signal's `generate_if` gate: a signal gated OFF
                // under this bind's effective params is absent (the DUT built
                // by `arch build` has no such port), so it falls through to
                // the not-found error below rather than resolving to a phantom
                // port — which would silently diverge from the SV backend.
                if self.bus_signal_present(&id.name, &bus, &name.name) {
                    return Some(format!("{root}->{}_{}", sig_prefix, name.name));
                }
                // Already-flattened `<chan>_<sig>` form (or `<chan>_valid`
                // / `<chan>_ready`).
                for h in &bus.handshakes {
                    let chprefix = format!("{}_", h.name.name);
                    if name.name.starts_with(&chprefix) {
                        let tail = &name.name[chprefix.len()..];
                        if tail == "valid"
                            || tail == "ready"
                            || h.payload.iter().any(|s| s.name.name == tail)
                        {
                            return Some(format!("{root}->{}_{}", sig_prefix, name.name));
                        }
                    }
                }
                // Channel-name itself (used as a leaf in the two-level
                // form `bus.ch.sig`) — defer to the field-access path.
                if bus.handshakes.iter().any(|h| h.name.name == name.name) {
                    return None;
                }
                // Distinguish "gated OFF" from "never declared": a gated-OFF
                // access is a real TB bug (it drives a channel the DUT was
                // built without), so name it precisely.
                if bus.signals.iter().any(|s| s.name.name == name.name) {
                    self.errors.push(format!(
                        "bus `{}` (binding `{}`) signal `{}` is gated OFF by its \
                         `generate_if` condition under the bind's params — `arch build` \
                         omits this port, so the testbench must not access it",
                        bus.name.name, id.name, name.name,
                    ));
                } else {
                    self.errors.push(format!(
                        "bus `{}` (binding `{}`) has no signal or channel named `{}`",
                        bus.name.name, id.name, name.name,
                    ));
                }
                return Some(format!("/* unresolved: {}.{} */ 0", id.name, name.name));
            }
        }
        // <binding>.<channel>.<signal>
        if let ExprKind::Field {
            target: outer,
            name: ch,
        } = &*target.kind
        {
            if let ExprKind::Ident(id) = &*outer.kind {
                if let Some((bus, root, sig_prefix)) = self.bus_bindings.get(&id.name).cloned() {
                    if let Some(h) = bus.handshakes.iter().find(|h| h.name.name == ch.name) {
                        if name.name == "valid"
                            || name.name == "ready"
                            || h.payload.iter().any(|s| s.name.name == name.name)
                        {
                            let sig_port = self.bus_signal_name(&sig_prefix, &ch.name, &name.name);
                            return Some(format!("{root}->{sig_port}"));
                        }
                        let valid_options: Vec<&str> = std::iter::once("valid")
                            .chain(std::iter::once("ready"))
                            .chain(h.payload.iter().map(|s| s.name.name.as_str()))
                            .collect();
                        self.errors.push(format!(
                            "bus `{}` channel `{}` has no signal `{}` (valid: {})",
                            bus.name.name,
                            ch.name,
                            name.name,
                            valid_options.join(", "),
                        ));
                        return Some(format!(
                            "/* unresolved: {}.{}.{} */ 0",
                            id.name, ch.name, name.name
                        ));
                    }
                    self.errors.push(format!(
                        "bus `{}` (binding `{}`) has no channel `{}`",
                        bus.name.name, id.name, ch.name,
                    ));
                    return Some(format!(
                        "/* unresolved: {}.{}.{} */ 0",
                        id.name, ch.name, name.name
                    ));
                }
            }
        }
        None
    }
}
