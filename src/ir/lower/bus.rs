//! Bus-construct lowering: bind validation, protocol-typed signal
//! access (`<bind>.<sig>` / `<bind>.<ch>.<sig>`), channel auto-
//! handshakes (`<bind>.<ch>.send/recv`), and blocking `tlm_method`
//! calls.
//!
//! Two deliberately different lowering shapes (docs/tbir-mvp.md §bus):
//!
//! * **Channel handshakes CFG-inline.** `send`/`recv` are pin-level
//!   protocol sugar the TB itself performs — v1 emits the valid/ready
//!   dance inline — so they lower structurally to `DutWrite` /
//!   `DutRead` / budget-loop blocks. Placement sees exactly what they
//!   are: cycle-anchored Tier-0 pin work.
//! * **`tlm_method` calls retain protocol seams.** A blocking method call
//!   lowers to `Assign(dest, Expr::Call(CallTarget::TransactorMethod
//!   { bus_field, method, target }, args))` and is never inlined at the IR
//!   level — the sequence→transactor boundary is the Tier-1/Tier-0
//!   placement cut (docs/tb-ir-design.md §CallTarget), and the verifier
//!   enforces the seam. A direct `out_of_order` call lowers to an adjacent
//!   tagged `TlmFork` / one-entry `TlmJoinAll`, reusing the existing
//!   asynchronous protocol seam synchronously. Backends expand their req/rsp
//!   wire protocols.

use super::{not_implemented, FuncBuilder, LowerError, V1Status};
use crate::ast::{
    BusDecl, CallArg, Expr as AstExpr, ExprKind, HandshakeChannel, LetStmt, TlmMethod, TypeExpr,
};
use crate::ir::{self, BinOp, Expr, IrType, PortAccess, PortRef, Stmt, Terminator, UnOp};

/// Where a bus call's produced value goes. Only the two positions v1
/// supports exist: `let`-RHS and statement position (`x = bus.m(...)`
/// is rejected at expression lowering, matching v1's surface).
pub(crate) enum BusCallDest<'a> {
    /// `let x = <call>` — declare a fresh local.
    Declare(&'a str),
    /// Statement position — drive the protocol, discard any value.
    Discard,
}

/// Validate one test-scope bus binding and produce its schema plus an
/// owned copy of the declaration for the per-test lowering context.
/// The subset gate lives here: features whose v1 lowering depends on
/// information the IR pipeline does not model yet are rejected with
/// precise messages instead of silently mis-lowering. Bind-site generics
/// (`Bus<P=...>`) and `generate_if`-gated signals are NOT rejected: the
/// owned `BusDecl` carries the gates through, and the tbir emitter applies
/// the DUT-port param-override layering (defaults < bind generic < DUT-port
/// override) to decide gated-signal presence at the access site, matching
/// `arch build`'s flattened port set.
pub(super) fn lower_bus_binding(
    l: &LetStmt,
    decl: &BusDecl,
    id: ir::BusBindingId,
    record_ids: &std::collections::HashMap<String, ir::RecordId>,
) -> Result<(ir::BusBindingSchema, BusDecl), LowerError> {
    let bind = &l.name.name;
    if !l.probes.is_empty() {
        return Err(not_implemented(
            "probe declarations on a bus binding",
            "declare probes on `let dut` — no other binding gets a probe accessor, so \
             the declaration is inert and any read of it fails to compile",
            V1Status::EmitsUncompilable,
        ));
    }
    // `bind ... with { ch.sig: "port", ... }` signal remaps. Each path
    // must be exactly `<channel>.<signal>` (2 segments), mirroring v1's
    // `bind_remap` → `bus_remap` translation; the resulting
    // `(channel, signal) → port` table overrides the
    // `<field>_<channel>_<signal>` flat-name convention at wire
    // emission. Sorted by key for deterministic dump-ir output.
    //
    // Both `tlm_method` wire emission (`emit_transactor_call` /
    // `emit_target_actor`, via `binding.wire_name`) and handshake-channel
    // access (`bus.<ch>.<sig>` / `.send` / `.recv`) honor the remap. A
    // channel access lowers to a `PortRef` whose 3-segment path
    // `[binding, channel, signal]` is collapsed to the override's flat
    // name when `(channel, signal)` is mapped — directly for test-scope
    // access or through a typed adapter for a shared bound callable body.
    // Already-flattened `<ch>_<sig>` access and plain `<bind>.<sig>`
    // access are NOT remapped, mirroring v1's `try_emit_bus_field_access`.
    let mut remap: Vec<((String, String), String)> = Vec::new();
    for entry in &l.bind_remap {
        if entry.path.len() != 2 {
            return Err(LowerError::Invalid(format!(
                "bind {bind} with: signal path `{}` must be exactly \
                 `<channel>.<signal>` (2 segments, got {})",
                entry
                    .path
                    .iter()
                    .map(|i| i.name.as_str())
                    .collect::<Vec<_>>()
                    .join("."),
                entry.path.len()
            )));
        }
        let key = (entry.path[0].name.clone(), entry.path[1].name.clone());
        // v1 stores remaps in a HashMap (last write wins on a duplicate
        // `(channel, signal)` key); mirror that — replace an existing
        // entry rather than shadowing it with a first-wins linear scan.
        if let Some(existing) = remap.iter_mut().find(|(k, _)| *k == key) {
            existing.1 = entry.port.clone();
        } else {
            remap.push((key, entry.port.clone()));
        }
    }
    remap.sort();
    // Bind-site generic overrides (`let s : Bus<WRITE=0> = bind dut`) are
    // NOT rejected and NOT evaluated here: lowering has no param env. The
    // binding lowers through carrying every signal's `gate` expression
    // intact (the owned `BusDecl` returned below), and the tbir emitter —
    // which has `EmitOpts` + the `SourceFile` — recovers the bind-site
    // `TypeExpr` from the test let, layers it into the effective env
    // (`bus_param_env_with_port_override`: defaults < bind generic < DUT-
    // port override), and rejects only an ACCESS to a signal gated OFF
    // under that env. This mirrors v1's `bus_param_env` / `bus_signal_present`
    // exactly. See `src/codegen/tbir/mod.rs::check_gated_bus_access`.
    match l.value.as_ref().map(|v| &*v.kind) {
        Some(ExprKind::Ident(id)) if id.name == "dut" => {}
        _ => {
            // v1 does not resolve the bind target: it substitutes the
            // bind EXPRESSION where the DUT pointer goes and dereferences
            // it. Neither shape compiles, for two different reasons —
            // `= bind nope` emits `nope->mem_read_addr` against a symbol
            // v1 never declares, and `= bind dut.core` emits
            // `harc_rt::harc_read(dut->core)->mem_read_addr`, applying
            // `operator->` to the VALUE `harc_read` returns. Both probed
            // by mutating `tlm_method_blocking_bus_test` (registered,
            // passing under both backends) one token; the bare-ident case
            // changes 72 lines, every one of them a `dut->` access.
            return Err(not_implemented(
                &format!("bus binding `{bind}` to a non-DUT target"),
                "only `= bind dut` is lowered; v1 substitutes the bind expression for \
                 the DUT pointer and dereferences it, which resolves to no symbol at \
                 all for a bare name and to a non-pointer value for a field path",
                V1Status::EmitsUncompilable,
            ));
        }
    }
    // `generate_if`-gated signals are NOT evaluated here: lowering has no
    // param env (only the emission-side `EmitOpts` carries the DUT-port
    // override that selects which gated channels `arch build` flattened).
    // The owned `BusDecl` returned below keeps every signal's `gate`
    // expression intact, and the tbir emitter (which has `EmitOpts` + the
    // `SourceFile`) evaluates each *accessed* signal's gate against the
    // effective env, mirroring v1's `bus_signal_present` / gated-OFF error.
    // See `src/codegen/tbir/mod.rs::check_gated_bus_access`.

    let methods = decl
        .tlm_methods
        .iter()
        .map(|m| {
            let mode = match m.mode.name.as_str() {
                "blocking" => ir::TlmMethodMode::Blocking,
                "out_of_order" => {
                    let tags = m
                        .out_of_order_tags
                        .as_ref()
                        .and_then(super::exprs::parse_int_literal_expr)
                        .ok_or_else(|| {
                            LowerError::Invalid(format!(
                                "bus `{}` tlm_method `{}` requires a literal positive `tags` count",
                                decl.name.name, m.name.name
                            ))
                        })?;
                    if tags == 0 {
                        return Err(LowerError::Invalid(format!(
                            "bus `{}` tlm_method `{}` requires a positive `tags` count",
                            decl.name.name, m.name.name
                        )));
                    }
                    ir::TlmMethodMode::OutOfOrder { tags }
                }
                other => {
                    return Err(LowerError::Invalid(format!(
                        "bus `{}` tlm_method `{}` has unsupported mode `{other}`",
                        decl.name.name, m.name.name
                    )))
                }
            };
            Ok(ir::TlmMethodSchema {
                name: m.name.name.clone(),
                args: m.args.iter().map(|(n, _)| n.name.clone()).collect(),
                arg_types: m
                    .args
                    .iter()
                    .map(|(_, ty)| super::helpers::slot_ir_type(Some(ty), record_ids))
                    .collect(),
                has_ret: m.ret.is_some(),
                ret_type: m
                    .ret
                    .as_ref()
                    .map(|ty| super::helpers::slot_ir_type(Some(ty), record_ids)),
                mode,
            })
        })
        .collect::<Result<Vec<_>, LowerError>>()?;
    Ok((
        ir::BusBindingSchema {
            id,
            field: bind.clone(),
            bus: decl.name.name.clone(),
            methods,
            remap,
        },
        decl.clone(),
    ))
}

impl FuncBuilder<'_> {
    /// PortRef for a `<bind>.<channel>.<signal>` handshake access. The IR
    /// retains this logical path; the selected test's typed binding applies
    /// any concrete DUT-port remap during emission.
    fn bus_channel_port(&self, bind: &str, ch: &str, sig: &str) -> PortRef {
        let channel = self.ctx.bus_bindings.get(bind).and_then(|bus| {
            bus.handshakes
                .iter()
                .find(|channel| channel.name.name == ch)
        });
        let value_type = channel
            .and_then(|channel| {
                if matches!(sig, "valid" | "ready") {
                    Some(IrType::Bool)
                } else {
                    channel
                        .payload
                        .iter()
                        .find(|signal| signal.name.name == sig)
                        .map(|signal| super::helpers::ir_type_of(Some(&signal.ty)))
                }
            })
            .unwrap_or(IrType::Unknown);
        let direction = channel.map(|channel| match (channel.role, sig) {
            (crate::ast::HandshakeRole::Send, "ready")
            | (crate::ast::HandshakeRole::Receive, "valid") => ir::PortDirection::Out,
            (crate::ast::HandshakeRole::Send, _)
            | (crate::ast::HandshakeRole::Receive, "ready") => ir::PortDirection::In,
            (crate::ast::HandshakeRole::Receive, _) => ir::PortDirection::Out,
        });
        bus_port(
            bind,
            &[ch, sig],
            self.bus_origin(bind),
            value_type,
            direction,
        )
    }

    fn bus_origin(&self, bind: &str) -> crate::ir::PortOrigin {
        self.ctx
            .bus_origins
            .get(bind)
            .cloned()
            .unwrap_or(crate::ir::PortOrigin::BoundBus)
    }

    /// `Some(PortRef)` when `e` is a dotted access rooted at a bus
    /// binding: `<bind>.<sig>` (plain signal or pre-flattened
    /// `<ch>_<sig>`), or `<bind>.<ch>.<sig>` (handshake channel signal
    /// incl. the implicit `valid`/`ready`). Flat-name convention
    /// mirrors arch-com §19.6 / v1's `bus_signal_name`:
    /// `<bind>_<ch>_<sig>`. Unknown signals/channels are hard errors
    /// with v1's diagnostic text — v1 surfaces them as codegen errors;
    /// the IR rejects at lowering.
    pub(crate) fn as_bus_port_ref(&self, e: &AstExpr) -> Result<Option<PortRef>, LowerError> {
        let ExprKind::Field { target, name } = &*e.kind else {
            return Ok(None);
        };
        // <bind>.<name>
        if let ExprKind::Ident(id) = &*target.kind {
            let Some(bus) = self.ctx.bus_bindings.get(&id.name) else {
                return Ok(None);
            };
            if bus.signals.iter().any(|s| s.name.name == name.name) {
                let signal = bus
                    .signals
                    .iter()
                    .find(|signal| signal.name.name == name.name)
                    .expect("checked above");
                let value_type = super::helpers::ir_type_of(Some(&signal.ty));
                let direction = match signal.direction {
                    crate::ast::Direction::In => Some(ir::PortDirection::Out),
                    crate::ast::Direction::Out => Some(ir::PortDirection::In),
                    crate::ast::Direction::InOut => None,
                };
                return Ok(Some(bus_port(
                    &id.name,
                    &[&name.name],
                    self.bus_origin(&id.name),
                    value_type,
                    direction,
                )));
            }
            // Already-flattened `<ch>_<sig>` form.
            for h in &bus.handshakes {
                let chprefix = format!("{}_", h.name.name);
                if let Some(tail) = name.name.strip_prefix(&chprefix) {
                    if tail == "valid"
                        || tail == "ready"
                        || h.payload.iter().any(|s| s.name.name == tail)
                    {
                        let value_type = if matches!(tail, "valid" | "ready") {
                            IrType::Bool
                        } else {
                            h.payload
                                .iter()
                                .find(|signal| signal.name.name == tail)
                                .map(|signal| super::helpers::ir_type_of(Some(&signal.ty)))
                                .unwrap_or(IrType::Unknown)
                        };
                        let direction = Some(match (h.role, tail) {
                            (crate::ast::HandshakeRole::Send, "ready")
                            | (crate::ast::HandshakeRole::Receive, "valid") => {
                                ir::PortDirection::Out
                            }
                            (crate::ast::HandshakeRole::Send, _)
                            | (crate::ast::HandshakeRole::Receive, "ready") => {
                                ir::PortDirection::In
                            }
                            (crate::ast::HandshakeRole::Receive, _) => ir::PortDirection::Out,
                        });
                        return Ok(Some(bus_port(
                            &id.name,
                            &[&name.name],
                            self.bus_origin(&id.name),
                            value_type,
                            direction,
                        )));
                    }
                }
            }
            if bus.handshakes.iter().any(|h| h.name.name == name.name) {
                return Err(LowerError::Invalid(format!(
                    "bus `{}` (binding `{}`): channel `{}` cannot be used as a value — \
                     access a signal (`{}.{}.valid`, ...)",
                    bus.name.name, id.name, name.name, id.name, name.name
                )));
            }
            return Err(LowerError::Invalid(format!(
                "bus `{}` (binding `{}`) has no signal or channel named `{}`",
                bus.name.name, id.name, name.name
            )));
        }
        // <bind>.<ch>.<sig>
        if let ExprKind::Field {
            target: outer,
            name: ch,
        } = &*target.kind
        {
            let ExprKind::Ident(id) = &*outer.kind else {
                return Ok(None);
            };
            let Some(bus) = self.ctx.bus_bindings.get(&id.name) else {
                return Ok(None);
            };
            let Some(h) = bus.handshakes.iter().find(|h| h.name.name == ch.name) else {
                return Err(LowerError::Invalid(format!(
                    "bus `{}` (binding `{}`) has no channel `{}`",
                    bus.name.name, id.name, ch.name
                )));
            };
            if name.name == "valid"
                || name.name == "ready"
                || h.payload.iter().any(|s| s.name.name == name.name)
            {
                return Ok(Some(self.bus_channel_port(&id.name, &ch.name, &name.name)));
            }
            let valid_options: Vec<&str> = ["valid", "ready"]
                .into_iter()
                .chain(h.payload.iter().map(|s| s.name.name.as_str()))
                .collect();
            return Err(LowerError::Invalid(format!(
                "bus `{}` channel `{}` has no signal `{}` (valid: {})",
                bus.name.name,
                ch.name,
                name.name,
                valid_options.join(", ")
            )));
        }
        Ok(None)
    }

    /// Binding name at the root of a call's dotted callee, if the
    /// callee has an actual bus-call shape — `<bind>.<method>` or
    /// `<bind>.<ch>.<send|recv>` (1–2 dotted levels). Used by
    /// expression lowering to reject nested bus calls with a precise
    /// message instead of the generic method-call one; deeper chains
    /// (`<bind>.<ch>.<sig>.trunc(...)`) are not bus calls and fall
    /// through to the generic rejection.
    pub(crate) fn bus_call_root(&self, callee: &AstExpr) -> Option<String> {
        let mut depth = 0usize;
        let mut cur = callee;
        loop {
            match &*cur.kind {
                ExprKind::Field { target, .. } => {
                    depth += 1;
                    if depth > 2 {
                        return None;
                    }
                    cur = target;
                }
                ExprKind::Paren(inner) => cur = inner,
                ExprKind::Ident(id) => {
                    return (depth >= 1 && self.ctx.bus_bindings.contains_key(&id.name))
                        .then(|| id.name.clone());
                }
                _ => return None,
            }
        }
    }

    /// Statement-position bus call dispatch. Returns `Ok(true)` when
    /// `e` was a recognized bus call (`<bind>.<ch>.send/recv` or
    /// `<bind>.<method>(...)`) and was lowered into the CFG / a call
    /// edge; `Ok(false)` when `e` is not a bus call at all.
    pub(crate) fn try_lower_bus_call(
        &mut self,
        e: &AstExpr,
        dest: BusCallDest<'_>,
    ) -> Result<bool, LowerError> {
        let ExprKind::Call { callee, args } = &*e.kind else {
            return Ok(false);
        };
        let ExprKind::Field {
            target,
            name: method,
        } = &*callee.kind
        else {
            return Ok(false);
        };
        // <bind>.<ch>.send(...) / <bind>.<ch>.recv()
        if let ExprKind::Field {
            target: outer,
            name: ch,
        } = &*target.kind
        {
            if let ExprKind::Ident(id) = &*outer.kind {
                if let Some(bus) = self.ctx.bus_bindings.get(&id.name) {
                    let Some(h) = bus
                        .handshakes
                        .iter()
                        .find(|h| h.name.name == ch.name)
                        .cloned()
                    else {
                        return Err(LowerError::Invalid(format!(
                            "bus `{}` (binding `{}`) has no channel `{}`",
                            bus.name.name, id.name, ch.name
                        )));
                    };
                    let bind = id.name.clone();
                    return match method.name.as_str() {
                        "send" => {
                            self.lower_handshake_send(&bind, &h, args, dest)?;
                            Ok(true)
                        }
                        "recv" => {
                            self.lower_handshake_recv(&bind, &h, args, dest)?;
                            Ok(true)
                        }
                        // v1 splits on whether the method name happens to
                        // be a channel SIGNAL. `strm.s.poke()` is not, so
                        // it reports "channel `s` has no signal `poke`" —
                        // but `strm.s.data()` IS, and v1 emits it as a
                        // signal read with the call parens left on:
                        // `auto d = harc_rt::harc_read(dut->strm_s_data)();`.
                        // `harc_read` returns a VALUE (`_harc_u128` /
                        // `HarcWide<N>`), so that is "expression cannot be
                        // used as a function" — the worse of the two
                        // outcomes, and the one that sets the status.
                        // Probed by adding a channel call to
                        // `stream_burst_mon_test`; `.recv()` is itself a
                        // control (both backends emit).
                        other => Err(not_implemented(
                            &format!("bus channel method `.{other}(...)`"),
                            "supported: send, recv",
                            V1Status::EmitsUncompilable,
                        )),
                    };
                }
            }
        }
        // <bind>.<method>(...) — tlm_method call edge.
        if let ExprKind::Ident(id) = &*target.kind {
            if let Some(bus) = self.ctx.bus_bindings.get(&id.name) {
                let Some(m) = bus
                    .tlm_methods
                    .iter()
                    .find(|m| m.name.name == method.name)
                    .cloned()
                else {
                    return Err(LowerError::Invalid(format!(
                        "bus `{}` has no tlm_method `{}`",
                        bus.name.name, method.name
                    )));
                };
                let bind = id.name.clone();
                self.lower_tlm_method_call(e, &bind, &m, args, dest)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `<bind>.<ch>.send(payload...)` — v1's auto-handshake, CFG-
    /// inlined: drive payload + valid, budget-wait for ready, tick,
    /// drop valid (cpp_tb `try_emit_bus_handshake`, coroutine path).
    fn lower_handshake_send(
        &mut self,
        bind: &str,
        h: &HandshakeChannel,
        args: &[CallArg],
        dest: BusCallDest<'_>,
    ) -> Result<(), LowerError> {
        if !matches!(dest, BusCallDest::Discard) {
            return Err(LowerError::Invalid(format!(
                "bus.{}.send returns no value; use it as a statement",
                h.name.name
            )));
        }
        if args.len() != h.payload.len() {
            return Err(LowerError::Invalid(format!(
                "bus.{}.send: expected {} payload arg(s), got {}",
                h.name.name,
                h.payload.len(),
                args.len()
            )));
        }
        let declared: Vec<String> = h.payload.iter().map(|p| p.name.name.clone()).collect();
        super::reject_misplaced_named_args(
            args,
            &declared,
            &format!("a `bus.{}.send(...)` payload", h.name.name),
        )?;
        for (sig, arg) in h.payload.iter().zip(args.iter()) {
            let value = self.lower_expr(call_arg_expr(arg))?; // ports OK in DutWrite values
            let value = self.hoist_transactor_calls(value);
            self.push(Stmt::DutWrite(
                self.bus_channel_port(bind, &h.name.name, &sig.name.name),
                value,
            ));
        }
        self.push(Stmt::DutWrite(
            self.bus_channel_port(bind, &h.name.name, "valid"),
            lit(1),
        ));
        self.lower_budget_wait_low(self.bus_channel_port(bind, &h.name.name, "ready"));
        self.wait_one_cycle();
        self.push(Stmt::DutWrite(
            self.bus_channel_port(bind, &h.name.name, "valid"),
            lit(0),
        ));
        Ok(())
    }

    /// `<bind>.<ch>.recv()` — drive ready, budget-wait for valid,
    /// capture, tick, drop ready.
    ///
    /// Documented divergence (docs/tbir-mvp.md): v1 captures the FULL
    /// payload into a generated `<Bus>_<ch>_payload` struct with an
    /// implicit conversion to the first field; the IR's scalar local
    /// model captures the first payload signal only. Observably
    /// equivalent for every use the IR can express — scalar reads see
    /// the first field either way, and named payload-field access
    /// (`v.resp`) is rejected at lowering ("field access on a non-DUT
    /// value"), never mis-lowered.
    fn lower_handshake_recv(
        &mut self,
        bind: &str,
        h: &HandshakeChannel,
        args: &[CallArg],
        dest: BusCallDest<'_>,
    ) -> Result<(), LowerError> {
        if !args.is_empty() {
            return Err(LowerError::Invalid(format!(
                "bus.{}.recv: expected 0 args, got {}",
                h.name.name,
                args.len()
            )));
        }
        if h.payload.is_empty() {
            return Err(LowerError::Invalid(format!(
                "bus.{}.recv: channel has no payload signals to receive",
                h.name.name
            )));
        }
        self.push(Stmt::DutWrite(
            self.bus_channel_port(bind, &h.name.name, "ready"),
            lit(1),
        ));
        self.lower_budget_wait_low(self.bus_channel_port(bind, &h.name.name, "valid"));
        // Capture BEFORE the trailing tick — payload is valid in the
        // same cycle `valid` is high (mirrors v1).
        //
        // v1 captures the whole `<Bus>_<ch>_payload` struct, with the
        // bare local convertible to the first field. The IR's scalar
        // local model captures the FIRST payload signal into the bound
        // local (so a bare scalar read `let v = bus.r.recv()` sees
        // `data`), AND — to support v1's named field access
        // `let r = bus.r.recv(); ... r.data` — captures every payload
        // signal into a per-field local, recorded in `recv_payloads` so
        // a later `r.<field>` read resolves to the matching local.
        match dest {
            BusCallDest::Declare(name) => {
                let id = self.declare(name);
                let first_port = self.bus_channel_port(bind, &h.name.name, &h.payload[0].name.name);
                self.push(Stmt::DutRead(id, first_port));
                let mut fields = Vec::with_capacity(h.payload.len());
                // The first field aliases the bound local itself.
                fields.push((h.payload[0].name.name.clone(), id));
                for sig in &h.payload[1..] {
                    let fid = self.declare(&format!("{name}__{}", sig.name.name));
                    let port = self.bus_channel_port(bind, &h.name.name, &sig.name.name);
                    self.push(Stmt::DutRead(fid, port));
                    fields.push((sig.name.name.clone(), fid));
                }
                self.recv_payloads.insert(id, fields);
            }
            BusCallDest::Discard => {}
        }
        self.wait_one_cycle();
        self.push(Stmt::DutWrite(
            self.bus_channel_port(bind, &h.name.name, "ready"),
            lit(0),
        ));
        Ok(())
    }

    /// Blocking `tlm_method` call → `Assign(dest, Call(TransactorMethod))`
    /// call edge. The protocol expansion (req/rsp wires, budget loops,
    /// trace events) is backend-owned; the IR carries only the edge.
    fn lower_tlm_method_call(
        &mut self,
        call_expr: &AstExpr,
        bind: &str,
        m: &TlmMethod,
        args: &[CallArg],
        dest: BusCallDest<'_>,
    ) -> Result<(), LowerError> {
        // A direct OOO call is synchronous at the source level: issue one
        // tagged request and immediately drain that same response. Reuse the
        // fork/join lowering so the request handshake, tag routing, return
        // typing, verifier seam, and backend protocol remain one contract.
        if m.mode.name == "out_of_order" {
            if self.concurrent_target_ooo_lanes {
                return Err(not_implemented(
                    &format!(
                        "direct out_of_order call `{bind}.{}` inside a concurrently instanced \
                         out_of_order target responder body",
                        m.name.name
                    ),
                    "use an explicit downstream fork/join design with runtime-safe tag ownership, \
                     or serve the front-side method as blocking",
                    V1Status::Rejects,
                ));
            }
            if self
                .pending_tlm_forks
                .iter()
                .any(|pending| pending.bus_field == bind && pending.method == m.name.name)
            {
                return Err(LowerError::Invalid(format!(
                    "direct call `{bind}.{}` cannot share an out_of_order method with pending forks; \
                     join_all before the direct call",
                    m.name.name
                )));
            }

            // No request on this method remains outstanding, so tag 0 is
            // reusable even after earlier direct calls. This also prevents a
            // sequence of synchronous calls from walking beyond `tags N`.
            let tag_key = (bind.to_string(), m.name.name.clone());
            let saved_next_tag = self.next_tlm_fork_tag.insert(tag_key.clone(), 0);
            let fork_expr = AstExpr::new(
                ExprKind::ForkCall {
                    call: call_expr.clone(),
                },
                call_expr.span,
            );
            let prior_pending = self.pending_tlm_forks.len();
            let result = self.try_lower_tlm_fork_impl(&fork_expr, dest, false);
            if let Some(next) = saved_next_tag {
                self.next_tlm_fork_tag.insert(tag_key, next);
            } else {
                self.next_tlm_fork_tag.remove(&tag_key);
            }
            let lowered = result?;
            debug_assert!(lowered, "synthetic direct OOO fork must resolve");
            let direct = self
                .pending_tlm_forks
                .pop()
                .expect("direct OOO lowering appended its descriptor");
            debug_assert_eq!(self.pending_tlm_forks.len(), prior_pending);
            self.push(Stmt::TlmJoinAll(vec![direct]));
            return Ok(());
        }
        if m.mode.name != "blocking" {
            return Err(not_implemented(
                &format!("`{}` tlm_method calls", m.mode.name),
                format!(
                    "`{bind}.{}` — only `blocking` methods are lowered",
                    m.name.name
                ),
                V1Status::Rejects,
            ));
        }
        if args.len() != m.args.len() {
            return Err(LowerError::Invalid(format!(
                "bus.{}: expected {} arg(s), got {}",
                m.name.name,
                m.args.len(),
                args.len()
            )));
        }
        if m.ret.is_none() && !matches!(dest, BusCallDest::Discard) {
            return Err(LowerError::Invalid(format!(
                "bus.{} returns no value; use it as a statement",
                m.name.name
            )));
        }
        // Args evaluate before any protocol activity; DUT reads hoist
        // into the current block (same cycle, same values as v1's
        // inline arg emission — no tick in between). The declared method
        // arg type is passed as a width hint so a bare wide DUT-port read
        // (`wide.send(dut.payload)` over a `uint<1024>` arg) hoists into a
        // wide temp instead of truncating to u64 (`port_temp_type` only
        // honors the hint for >64-bit widths, so narrow args are
        // unaffected).
        let declared: Vec<String> = m.args.iter().map(|(n, _)| n.name.clone()).collect();
        super::reject_misplaced_named_args(
            args,
            &declared,
            &format!("a `bus.{}(...)` call", m.name.name),
        )?;
        let arg_effects: Vec<bool> = args
            .iter()
            .map(|arg| self.expr_has_effectful_value_prelude(call_arg_expr(arg)))
            .collect();
        let mut lowered = Vec::with_capacity(args.len());
        for (index, (a, (aname, decl_ty))) in args.iter().zip(m.args.iter()).enumerate() {
            let hint = super::helpers::ir_type_of(Some(decl_ty));
            let mut v = self.lower_expr_no_ports_hinted(call_arg_expr(a), Some(hint))?;
            if arg_effects[index + 1..].iter().any(|effect| *effect) {
                v = self.materialize_ordered_value(v);
            }
            // The bus spelling of the parameter slot rule. This path had
            // the declared type in hand already — as a WIDTH hint one
            // line up — and never asked whether the argument belonged in
            // the slot at all. Measured: `mem.read(b)` on
            // `read(addr: uint<8>)` lowered, verified and EMITTED
            // `harc_rt::harc_assign(dut->mem_read_addr, b);` from both
            // backends, and both fail g++ identically ("invalid
            // 'static_cast' from type 'Beat'"). No backend runs it, so
            // it is `Invalid` like the rest of the family.
            let want = super::helpers::slot_ir_type(Some(decl_ty), &self.ctx.record_ids);
            if let crate::ir::IrType::Record(record) = want {
                self.ctx.reject_dynamic_list_record_wire(
                    record,
                    &format!(
                        "record parameter `{}` of bus method `{}` crossing a TLM request wire",
                        aname.name, m.name.name
                    ),
                )?;
            }
            if matches!(want, IrType::String) {
                return Err(not_implemented(
                    &format!(
                        "String parameter `{}` of bus method `{}` crossing a TLM request wire",
                        aname.name, m.name.name
                    ),
                    "String values are host-side callable arguments; TLM request wires carry packed numeric or record values",
                    V1Status::EmitsUncompilable,
                ));
            }
            self.check_slot_ir(
                &v,
                &want,
                &format!("parameter `{}` of bus method `{}`", aname.name, m.name.name),
            )?;
            lowered.push(v);
        }
        let logical_bus = self
            .ctx
            .bus_bindings
            .get(bind)
            .map(|bus| bus.name.name.clone())
            .ok_or_else(|| {
                LowerError::Invalid(format!(
                    "bus method `{}.{}` lost its declared bus type",
                    bind, m.name.name
                ))
            })?;
        let call = Expr::Call(
            crate::ir::CallTarget::TransactorMethod {
                bus_field: bind.to_string(),
                method: m.name.name.clone(),
                target: match self.bus_origin(bind) {
                    crate::ir::PortOrigin::BusBinding { binding, field } => self
                        .testbench_method_owner
                        .map(
                            |testbench| crate::ir::TransactorMethodTarget::TestbenchBusField {
                                testbench,
                                field: field.clone(),
                                bus: logical_bus.clone(),
                            },
                        )
                        .unwrap_or(crate::ir::TransactorMethodTarget::ConcreteBusBinding {
                            binding,
                            field,
                        }),
                    crate::ir::PortOrigin::BoundBus => crate::ir::TransactorMethodTarget::BoundBus,
                    crate::ir::PortOrigin::Dut => {
                        return Err(LowerError::Invalid(format!(
                            "bus method `{}.{}` lost its typed binding provenance",
                            bind, m.name.name
                        )))
                    }
                },
            },
            lowered,
        );
        // A record-typed return (`-> SomeStruct`) makes the dest a
        // record local, so `dest.field` / `dest.vecfield[i]` reads
        // resolve and the backend captures via `harc_unpack_<R>`. A
        // scalar return carries its declared `IrType` onto the dest so a
        // wide (>64-bit) return (`-> uint<1024>`) declares the loop-switch
        // local at the right width (`_harc_u128` / `HarcWide<N>`) instead
        // of truncating to the default u64.
        let ret_record = m.ret.as_ref().and_then(|t| self.tlm_ret_record_id(t));
        if let Some(record) = ret_record {
            self.ctx.reject_dynamic_list_record_wire(
                record,
                &format!(
                    "record return from bus method `{}` crossing a TLM response wire",
                    m.name.name
                ),
            )?;
        }
        if m.ret
            .as_ref()
            .is_some_and(|ret| matches!(super::helpers::ir_type_of(Some(ret)), IrType::String))
        {
            return Err(not_implemented(
                &format!(
                    "String return from bus method `{}` crossing a TLM response wire",
                    m.name.name
                ),
                "String values are host-side callable results; TLM response wires carry packed numeric or record values",
                V1Status::EmitsUncompilable,
            ));
        }
        match dest {
            BusCallDest::Declare(name) => {
                let id = self.declare(name);
                if let Some(rid) = ret_record {
                    self.set_local_type(id, crate::ir::IrType::Record(rid));
                } else if let Some(ret) = m.ret.as_ref() {
                    self.set_local_type(id, super::helpers::ir_type_of(Some(ret)));
                }
                self.push(Stmt::Assign(id, call));
            }
            BusCallDest::Discard => {
                let t = self.fresh_temp();
                self.push(Stmt::Assign(t, call));
            }
        }
        Ok(())
    }

    /// Resolve a `tlm_method` return `TypeExpr` to a `RecordId` when it
    /// names a lowered record (`struct`/`transaction`); `None` for a
    /// scalar return type or an unknown name.
    fn tlm_ret_record_id(&self, ret: &TypeExpr) -> Option<crate::ir::RecordId> {
        let TypeExpr::Named { name, .. } = ret else {
            return None;
        };
        let simple = name.segments.last().map(|s| s.name.as_str())?;
        self.ctx.record_ids.get(simple).copied()
    }

    /// `let x = fork <bind>.<method>(args)` / `fork <bind>.<method>(args)`
    /// — the non-blocking REQUEST issue of a bus-bound `tlm_method`. The
    /// response capture defers to the next `join_all` (`lower_tlm_join_all`
    /// drains the accumulated descriptors). Mirrors v1's
    /// `try_emit_bus_tlm_fork`: `blocking` methods get no tag (issue-order
    /// FIFO drain), `out_of_order tags N` methods get a per-(field,method)
    /// monotonically allocated request tag. Returns `Ok(true)` when `e`
    /// was a `fork bus.<method>(...)` and was lowered.
    pub(crate) fn try_lower_tlm_fork(
        &mut self,
        e: &AstExpr,
        dest: BusCallDest<'_>,
    ) -> Result<bool, LowerError> {
        self.try_lower_tlm_fork_impl(e, dest, true)
    }

    /// Shared descriptor construction for a source `fork bus.m(...)` and the
    /// synthetic issue half of a direct OOO call. `source_is_fork` affects
    /// diagnostics only; both shapes intentionally share protocol lowering.
    fn try_lower_tlm_fork_impl(
        &mut self,
        e: &AstExpr,
        dest: BusCallDest<'_>,
        source_is_fork: bool,
    ) -> Result<bool, LowerError> {
        let ExprKind::ForkCall { call } = &*e.kind else {
            return Ok(false);
        };
        // The next three arms mirror v1's `try_emit_bus_tlm_fork` shape
        // checks one-for-one, and v1 rejects each of them: `fork 9` and
        // `fork read_ooo(9)` both give "`fork` RHS currently requires a
        // direct bus tlm_method call", `fork mem.inner.read_ooo(9)` gives
        // "`fork` RHS currently requires `bus.method(args)`". Verified by
        // mutating `tlm_method_bus_test` one token at a time; the control
        // lowers in both backends.
        let ExprKind::Call { callee, args } = &*call.kind else {
            return Err(not_implemented(
                "`fork` RHS that is not a direct bus tlm_method call",
                "only `fork <bus>.<method>(args)` is lowered",
                V1Status::Rejects,
            ));
        };
        let ExprKind::Field {
            target,
            name: method,
        } = &*callee.kind
        else {
            return Err(not_implemented(
                "`fork` RHS that is not `<bus>.<method>(args)`",
                "",
                V1Status::Rejects,
            ));
        };
        let ExprKind::Ident(id) = &*target.kind else {
            return Err(not_implemented(
                "`fork` RHS that is not rooted at a bus binding",
                "",
                V1Status::Rejects,
            ));
        };
        let Some(bus) = self.ctx.bus_bindings.get(&id.name) else {
            return Ok(false);
        };
        let Some(m) = bus
            .tlm_methods
            .iter()
            .find(|m| m.name.name == method.name)
            .cloned()
        else {
            return Err(LowerError::Invalid(format!(
                "bus `{}` has no tlm_method `{}`",
                bus.name.name, method.name
            )));
        };
        let bind = id.name.clone();
        // The RHS-fork lowering supports `blocking` (issue-order) and
        // `out_of_order tags N` (tagged-lane) methods, matching v1.
        let tag = match m.mode.name.as_str() {
            "blocking" => None,
            "out_of_order" => {
                let tags = m
                    .out_of_order_tags
                    .as_ref()
                    .and_then(super::exprs::parse_int_literal_expr)
                    .ok_or_else(|| {
                        LowerError::Invalid(format!(
                            "`{bind}.{}` requires a literal positive `out_of_order tags N` count",
                            m.name.name
                        ))
                    })?;
                let key = (bind.clone(), m.name.name.clone());
                let next = self.next_tlm_fork_tag.entry(key).or_insert(0);
                if *next >= tags {
                    return Err(LowerError::Invalid(format!(
                        "`{bind}.{}` exceeds its {tags} outstanding tag slot(s) before `join_all`",
                        m.name.name
                    )));
                }
                let tag = *next;
                *next += 1;
                Some(tag)
            }
            // Unreachable today: `parse_tlm_method_decl` admits only
            // `blocking` and `out_of_order`, and both are handled above.
            // Kept as a defensive arm, classified to match v1 — its
            // `try_emit_bus_tlm_fork` carries the identical unreachable
            // arm and pushes an error ("HARC RHS-fork lowering supports
            // `blocking` and `out_of_order tags N`, not `{mode}`"), so a
            // third mode added to the shared parser would be rejected by
            // both backends.
            other => {
                return Err(not_implemented(
                    &format!("`fork {bind}.{}` on a `{other}` tlm_method", m.name.name),
                    "fork supports `blocking` and `out_of_order tags N` methods",
                    V1Status::Rejects,
                ));
            }
        };
        if args.len() != m.args.len() {
            return Err(LowerError::Invalid(format!(
                "bus.{}: expected {} arg(s), got {}",
                m.name.name,
                m.args.len(),
                args.len()
            )));
        }
        if m.ret.is_none() && !matches!(dest, BusCallDest::Discard) {
            return Err(LowerError::Invalid(format!(
                "bus.{} returns no value; {}",
                m.name.name,
                if source_is_fork {
                    "`fork` it as a statement"
                } else {
                    "use it as a statement"
                }
            )));
        }
        // Request payload evaluates now (same cycle as the request, no
        // tick between args and req_valid — v1's inline arg emission).
        let declared: Vec<String> = m.args.iter().map(|(n, _)| n.name.clone()).collect();
        super::reject_misplaced_named_args(
            args,
            &declared,
            &if source_is_fork {
                format!("a `fork bus.{}(...)` call", m.name.name)
            } else {
                format!("a `bus.{}(...)` call", m.name.name)
            },
        )?;
        let arg_effects: Vec<bool> = args
            .iter()
            .map(|arg| self.expr_has_effectful_value_prelude(call_arg_expr(arg)))
            .collect();
        let mut lowered = Vec::with_capacity(args.len());
        for (index, (a, (aname, decl_ty))) in args.iter().zip(m.args.iter()).enumerate() {
            let mut v = self.lower_expr_no_ports(call_arg_expr(a))?;
            if arg_effects[index + 1..].iter().any(|effect| *effect) {
                v = self.materialize_ordered_value(v);
            }
            // The FORK spelling of the parameter check one screen up.
            // Same declared types, same rule; the blocking and fork
            // request sides emit the same `harc_assign` into the same
            // request port, so a slot that is wrong in one is wrong in
            // the other.
            let want = super::helpers::slot_ir_type(Some(decl_ty), &self.ctx.record_ids);
            if let crate::ir::IrType::Record(record) = want {
                self.ctx.reject_dynamic_list_record_wire(
                    record,
                    &format!(
                        "record parameter `{}` of {} bus method `{}` crossing a TLM request wire",
                        aname.name,
                        if source_is_fork { "forked" } else { "direct" },
                        m.name.name
                    ),
                )?;
            }
            if matches!(want, IrType::String) {
                return Err(not_implemented(
                    &format!(
                        "String parameter `{}` of forked bus method `{}` crossing a TLM request wire",
                        aname.name, m.name.name
                    ),
                    "String values are host-side callable arguments; TLM request wires carry packed numeric or record values",
                    V1Status::EmitsUncompilable,
                ));
            }
            self.check_slot_ir(
                &v,
                &want,
                &format!(
                    "parameter `{}` of {} bus method `{}`",
                    aname.name,
                    if source_is_fork { "forked" } else { "direct" },
                    m.name.name
                ),
            )?;
            lowered.push(v);
        }
        // The response destination is declared + zero-init at the fork
        // site (v1 emits `T x = {};`), so reads between fork and join_all
        // see a defined-but-zero local. `Discard` carries no dest.
        let ret_record = m.ret.as_ref().and_then(|t| self.tlm_ret_record_id(t));
        if let Some(record) = ret_record {
            self.ctx.reject_dynamic_list_record_wire(
                record,
                &format!(
                    "record return from {} bus method `{}` crossing a TLM response wire",
                    if source_is_fork { "forked" } else { "direct" },
                    m.name.name
                ),
            )?;
        }
        if m.ret
            .as_ref()
            .is_some_and(|ret| matches!(super::helpers::ir_type_of(Some(ret)), IrType::String))
        {
            return Err(not_implemented(
                &format!(
                    "String return from forked bus method `{}` crossing a TLM response wire",
                    m.name.name
                ),
                "String values are host-side callable results; TLM response wires carry packed numeric or record values",
                V1Status::EmitsUncompilable,
            ));
        }
        let dest_local = match dest {
            BusCallDest::Declare(name) => {
                let id = self.declare(name);
                // The same typing the NON-fork path one screen up does.
                // Dropping it left a `let r = fork mem.read(5)` on a
                // record-returning `tlm_method` untyped, and that is not
                // merely a missing annotation: tbir emitted
                // `uint64_t r = 0; r = harc_read(dut->mem_read_rsp_data);`
                // where v1 emits `Resp r = {}; r = harc_unpack_Resp(...)`.
                // BOTH compile, so for a multi-field struct the two
                // backends silently computed DIFFERENT values — worse
                // than the uncompilable emissions elsewhere in this
                // family, because nothing fails.
                if let Some(rid) = ret_record {
                    self.set_local_type(id, crate::ir::IrType::Record(rid));
                } else if let Some(ret) = m.ret.as_ref() {
                    self.set_local_type(id, super::helpers::ir_type_of(Some(ret)));
                }
                Some(id)
            }
            BusCallDest::Discard => None,
        };
        let logical_bus = self
            .ctx
            .bus_bindings
            .get(&bind)
            .map(|bus| bus.name.name.clone())
            .ok_or_else(|| {
                LowerError::Invalid(format!(
                    "forked bus method `{}.{}` lost its declared bus type",
                    bind, m.name.name
                ))
            })?;
        let target = match self.bus_origin(&bind) {
            crate::ir::PortOrigin::BusBinding { binding, field } => self
                .testbench_method_owner
                .map(
                    |testbench| crate::ir::TransactorMethodTarget::TestbenchBusField {
                        testbench,
                        field: field.clone(),
                        bus: logical_bus.clone(),
                    },
                )
                .unwrap_or(crate::ir::TransactorMethodTarget::ConcreteBusBinding {
                    binding,
                    field,
                }),
            crate::ir::PortOrigin::BoundBus => crate::ir::TransactorMethodTarget::BoundBus,
            crate::ir::PortOrigin::Dut => {
                return Err(LowerError::Invalid(format!(
                    "forked bus method `{}.{}` lost its typed binding provenance",
                    bind, m.name.name
                )))
            }
        };
        let desc = crate::ir::TlmForkDesc {
            bus_field: bind,
            target,
            method: m.name.name.clone(),
            args: lowered,
            dest: dest_local,
            has_ret: m.ret.is_some(),
            tag,
        };
        self.push(Stmt::TlmFork(desc.clone()));
        self.pending_tlm_forks.push(desc);
        Ok(true)
    }

    /// `join_all` — drain every pending `fork` issued since the last
    /// join_all into a single `Stmt::TlmJoinAll`. Mixing tagged
    /// (`out_of_order`) and untagged (`blocking`) forks before one
    /// join_all is rejected — v1 reports it at emission, the IR rejects at
    /// lowering. An empty pending set lowers to an empty `TlmJoinAll`
    /// (a no-op, matching v1's "no pending forks" comment).
    pub(crate) fn lower_tlm_join_all(&mut self) -> Result<(), LowerError> {
        let pending = std::mem::take(&mut self.pending_tlm_forks);
        let tagged = pending.iter().filter(|p| p.tag.is_some()).count();
        if tagged != 0 && tagged != pending.len() {
            return Err(LowerError::Invalid(
                "cannot mix tagged (`out_of_order`) and untagged (`blocking`) \
                 fork TLM calls before one `join_all`"
                    .to_string(),
            ));
        }
        // Tags name outstanding requests, not source locations. Once the
        // barrier drains a tagged group, release its per-method allocator so
        // the next group starts again within `0..tags N`.
        for completed in pending.iter().filter(|call| call.tag.is_some()) {
            self.next_tlm_fork_tag
                .remove(&(completed.bus_field.clone(), completed.method.clone()));
        }
        self.push(Stmt::TlmJoinAll(pending));
        Ok(())
    }

    /// v1's bounded handshake wait, CFG shape of
    /// `{ int _b = 16; while (!<sig> && _b > 0) { co_await wait_cycles(1); _b--; } }`:
    ///
    /// ```text
    /// current: Assign(_b, 16); Jump(cond)
    /// cond:    DutRead(t, sig); Branch(!t && _b > 0, wait, after)
    /// wait:    -> WaitCycles(1, decr)
    /// decr:    Assign(_b, _b - 1); Jump(cond)
    /// after:   (becomes the current block)
    /// ```
    fn lower_budget_wait_low(&mut self, sig: PortRef) {
        let b = self.fresh_temp();
        self.push(Stmt::Assign(b, lit(16)));
        let cond = self.new_block();
        let wait = self.new_block();
        let decr = self.new_block();
        let after = self.new_block();
        self.terminate(Terminator::Jump(cond));

        self.start_block(cond);
        let t = self.fresh_temp();
        self.push(Stmt::DutRead(t, sig));
        let pred = Expr::Binary(
            BinOp::And,
            Box::new(Expr::Unary(UnOp::Not, Box::new(Expr::Local(t)))),
            Box::new(Expr::Binary(
                BinOp::Gt,
                Box::new(Expr::Local(b)),
                Box::new(lit(0)),
            )),
        );
        self.terminate(Terminator::Branch(pred, wait, after));

        self.start_block(wait);
        self.terminate(Terminator::WaitCycles(lit(1), None, decr));

        self.start_block(decr);
        self.push(Stmt::Assign(
            b,
            Expr::Binary(BinOp::Sub, Box::new(Expr::Local(b)), Box::new(lit(1))),
        ));
        self.terminate(Terminator::Jump(cond));

        self.start_block(after);
    }

    /// One scheduler tick (`co_await wait_cycles(_slot, 1)` in v1).
    fn wait_one_cycle(&mut self) {
        let next = self.new_block();
        self.terminate(Terminator::WaitCycles(lit(1), None, next));
        self.start_block(next);
    }
}

fn call_arg_expr(a: &CallArg) -> &AstExpr {
    match a {
        CallArg::Expr(e) => e,
        CallArg::Named { value, .. } => value,
    }
}

fn lit(v: u64) -> Expr {
    Expr::Literal {
        value: v,
        ty: IrType::Unknown,
    }
}

/// PortRef for a bus-bound flat signal: `bus_port("axil", ["aw",
/// "valid"])` → path `["axil", "aw", "valid"]`, which backends join
/// with `_` into the arch-com §19.6 flat name `axil_aw_valid`.
pub(super) fn bus_port(
    bind: &str,
    tail: &[&str],
    origin: crate::ir::PortOrigin,
    value_type: IrType,
    direction: Option<ir::PortDirection>,
) -> PortRef {
    let mut port_path = Vec::with_capacity(1 + tail.len());
    port_path.push(bind.to_string());
    port_path.extend(tail.iter().map(|s| s.to_string()));
    PortRef {
        testbench_field: "dut".to_string(),
        origin,
        port_path,
        aggregate_path: false,
        deferred_bus_binding: (bind == super::transactors::INITIATOR_BUS_PLACEHOLDER)
            .then_some(crate::ir::DeferredBusBinding::Unresolved),
        direction,
        width: match &value_type {
            IrType::Bool => Some(1),
            IrType::UInt(width) | IrType::SInt(width) => *width,
            _ => None,
        },
        value_type: Some(value_type),
        access: PortAccess::Port,
        probe: None,
        lane: None,
    }
}
