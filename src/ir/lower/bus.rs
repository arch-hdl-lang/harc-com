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
//! * **`tlm_method` calls stay call edges.** A blocking method call
//!   lowers to `Assign(dest, Expr::Call(CallTarget::TransactorMethod
//!   { bus_field, method }, args))` and is NEVER inlined at the IR
//!   level — the sequence→transactor boundary is the Tier-1/Tier-0
//!   placement cut (docs/tb-ir-design.md §CallTarget), and the
//!   verifier enforces the seam (top-of-Assign-RHS position only,
//!   Run/Check functions only). Backends expand the edge themselves;
//!   the tbir backend mirrors v1's req/rsp wire protocol.

use super::{FuncBuilder, LowerError, unsupported};
use crate::ast::{
    BusDecl, CallArg, Expr as AstExpr, ExprKind, HandshakeChannel, LetStmt, TlmMethod, TypeExpr,
};
use crate::ir::{
    self, BinOp, Expr, IrType, PortAccess, PortRef, Stmt, Terminator, UnOp,
};

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
/// information the IR pipeline does not model yet (bind remaps, bind-
/// site generics, `generate_if`-gated signals — the latter two need
/// the DUT-port param-override layering from `EmitOpts`) are rejected
/// with precise messages instead of silently mis-lowering.
pub(super) fn lower_bus_binding(
    l: &LetStmt,
    decl: &BusDecl,
) -> Result<(ir::BusBindingSchema, BusDecl), LowerError> {
    let bind = &l.name.name;
    if !l.probes.is_empty() {
        return Err(unsupported("probe declarations on a bus binding", ""));
    }
    // `bind ... with { ch.sig: "port", ... }` signal remaps. Each path
    // must be exactly `<channel>.<signal>` (2 segments), mirroring v1's
    // `bind_remap` → `bus_remap` translation; the resulting
    // `(channel, signal) → port` table overrides the
    // `<field>_<channel>_<signal>` flat-name convention at wire
    // emission. Sorted by key for deterministic dump-ir output.
    //
    // Subset: only `tlm_method` wire emission (`emit_transactor_call` /
    // `emit_target_actor`) routes through the remap's `wire_name`. A
    // handshake-channel access (`bus.<ch>.send`/`recv`/`<ch>.<sig>`)
    // lowers to a `PortRef` whose flat name is joined by the backend
    // with `_`, bypassing the remap — so a remap on a bus that declares
    // handshake channels would silently mis-name those wires. Reject it
    // precisely rather than mis-lower (the handshake-remap path is a
    // follow-up slice).
    if !l.bind_remap.is_empty() && !decl.handshakes.is_empty() {
        return Err(unsupported(
            &format!(
                "bus bind signal remaps (`bind ... with {{ ... }}`) on bus `{}` \
                 with handshake channels",
                decl.name.name
            ),
            "remaps are honored for `tlm_method`-only buses; handshake-channel \
             signal remaps are a follow-up slice",
        ));
    }
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
    if let Some(TypeExpr::Named { generics, .. }) = l.ty.as_ref() {
        if !generics.is_empty() {
            return Err(unsupported(
                "bus bind-site generic overrides (`Bus#(P=...)`)",
                "",
            ));
        }
    }
    match l.value.as_ref().map(|v| &*v.kind) {
        Some(ExprKind::Ident(id)) if id.name == "dut" => {}
        _ => {
            return Err(unsupported(
                &format!("bus binding `{bind}` to a non-DUT target"),
                "only `= bind dut` is lowered",
            ));
        }
    }
    // Param-gated signals need the effective param env (bus defaults +
    // bind generics + DUT-port override) that only the emission-side
    // `EmitOpts` carries; reject rather than evaluate gates against
    // defaults alone and silently diverge from `arch build`'s port set.
    let gated = decl
        .signals
        .iter()
        .chain(decl.handshakes.iter().flat_map(|h| h.payload.iter()))
        .any(|s| s.gate.is_some());
    if gated {
        return Err(unsupported(
            &format!(
                "binding bus `{}` with `generate_if`-gated signals",
                decl.name.name
            ),
            "",
        ));
    }

    let methods = decl
        .tlm_methods
        .iter()
        .map(|m| ir::TlmMethodSchema {
            name: m.name.name.clone(),
            args: m.args.iter().map(|(n, _)| n.name.clone()).collect(),
            has_ret: m.ret.is_some(),
        })
        .collect();
    Ok((
        ir::BusBindingSchema {
            field: bind.clone(),
            bus: decl.name.name.clone(),
            methods,
            remap,
        },
        decl.clone(),
    ))
}

impl FuncBuilder<'_> {
    /// `Some(PortRef)` when `e` is a dotted access rooted at a bus
    /// binding: `<bind>.<sig>` (plain signal or pre-flattened
    /// `<ch>_<sig>`), or `<bind>.<ch>.<sig>` (handshake channel signal
    /// incl. the implicit `valid`/`ready`). Flat-name convention
    /// mirrors arch-com §19.6 / v1's `bus_signal_name`:
    /// `<bind>_<ch>_<sig>`. Unknown signals/channels are hard errors
    /// with v1's diagnostic text — v1 surfaces them as codegen errors;
    /// the IR rejects at lowering.
    pub(crate) fn as_bus_port_ref(
        &self,
        e: &AstExpr,
    ) -> Result<Option<PortRef>, LowerError> {
        let ExprKind::Field { target, name } = &*e.kind else {
            return Ok(None);
        };
        // <bind>.<name>
        if let ExprKind::Ident(id) = &*target.kind {
            let Some(bus) = self.ctx.bus_bindings.get(&id.name) else {
                return Ok(None);
            };
            if bus.signals.iter().any(|s| s.name.name == name.name) {
                return Ok(Some(bus_port(&id.name, &[&name.name])));
            }
            // Already-flattened `<ch>_<sig>` form.
            for h in &bus.handshakes {
                let chprefix = format!("{}_", h.name.name);
                if let Some(tail) = name.name.strip_prefix(&chprefix) {
                    if tail == "valid"
                        || tail == "ready"
                        || h.payload.iter().any(|s| s.name.name == tail)
                    {
                        return Ok(Some(bus_port(&id.name, &[&name.name])));
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
                return Ok(Some(bus_port(&id.name, &[&ch.name, &name.name])));
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
                        other => Err(unsupported(
                            &format!("bus channel method `.{other}(...)`"),
                            "supported: send, recv",
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
                self.lower_tlm_method_call(&bind, &m, args, dest)?;
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
        for (sig, arg) in h.payload.iter().zip(args.iter()) {
            let value = self.lower_expr(call_arg_expr(arg))?; // ports OK in DutWrite values
            let value = self.hoist_transactor_calls(value);
            self.push(Stmt::DutWrite(
                bus_port(bind, &[&h.name.name, &sig.name.name]),
                value,
            ));
        }
        self.push(Stmt::DutWrite(
            bus_port(bind, &[&h.name.name, "valid"]),
            lit(1),
        ));
        self.lower_budget_wait_low(bus_port(bind, &[&h.name.name, "ready"]));
        self.wait_one_cycle();
        self.push(Stmt::DutWrite(
            bus_port(bind, &[&h.name.name, "valid"]),
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
            bus_port(bind, &[&h.name.name, "ready"]),
            lit(1),
        ));
        self.lower_budget_wait_low(bus_port(bind, &[&h.name.name, "valid"]));
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
                let first_port =
                    bus_port(bind, &[&h.name.name, &h.payload[0].name.name]);
                self.push(Stmt::DutRead(id, first_port));
                let mut fields = Vec::with_capacity(h.payload.len());
                // The first field aliases the bound local itself.
                fields.push((h.payload[0].name.name.clone(), id));
                for sig in &h.payload[1..] {
                    let fid = self.declare(&format!("{name}__{}", sig.name.name));
                    let port = bus_port(bind, &[&h.name.name, &sig.name.name]);
                    self.push(Stmt::DutRead(fid, port));
                    fields.push((sig.name.name.clone(), fid));
                }
                self.recv_payloads.insert(id, fields);
            }
            BusCallDest::Discard => {}
        }
        self.wait_one_cycle();
        self.push(Stmt::DutWrite(
            bus_port(bind, &[&h.name.name, "ready"]),
            lit(0),
        ));
        Ok(())
    }

    /// Blocking `tlm_method` call → `Assign(dest, Call(TransactorMethod))`
    /// call edge. The protocol expansion (req/rsp wires, budget loops,
    /// trace events) is backend-owned; the IR carries only the edge.
    fn lower_tlm_method_call(
        &mut self,
        bind: &str,
        m: &TlmMethod,
        args: &[CallArg],
        dest: BusCallDest<'_>,
    ) -> Result<(), LowerError> {
        if m.mode.name != "blocking" {
            return Err(unsupported(
                &format!("`{}` tlm_method calls", m.mode.name),
                format!(
                    "`{bind}.{}` — only `blocking` methods are lowered",
                    m.name.name
                ),
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
        // inline arg emission — no tick in between).
        let mut lowered = Vec::with_capacity(args.len());
        for a in args {
            lowered.push(self.lower_expr_no_ports(call_arg_expr(a))?);
        }
        let call = Expr::Call(
            crate::ir::CallTarget::TransactorMethod {
                bus_field: bind.to_string(),
                method: m.name.name.clone(),
            },
            lowered,
        );
        match dest {
            BusCallDest::Declare(name) => {
                let id = self.declare(name);
                self.push(Stmt::Assign(id, call));
            }
            BusCallDest::Discard => {
                let t = self.fresh_temp();
                self.push(Stmt::Assign(t, call));
            }
        }
        Ok(())
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
        let ExprKind::ForkCall { call } = &*e.kind else {
            return Ok(false);
        };
        let ExprKind::Call { callee, args } = &*call.kind else {
            return Err(unsupported(
                "`fork` RHS that is not a direct bus tlm_method call",
                "only `fork <bus>.<method>(args)` is lowered",
            ));
        };
        let ExprKind::Field { target, name: method } = &*callee.kind else {
            return Err(unsupported(
                "`fork` RHS that is not `<bus>.<method>(args)`",
                "",
            ));
        };
        let ExprKind::Ident(id) = &*target.kind else {
            return Err(unsupported(
                "`fork` RHS that is not rooted at a bus binding",
                "",
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
                let key = (bind.clone(), m.name.name.clone());
                let next = self.next_tlm_fork_tag.entry(key).or_insert(0);
                let tag = *next;
                *next += 1;
                Some(tag)
            }
            other => {
                return Err(unsupported(
                    &format!("`fork {bind}.{}` on a `{other}` tlm_method", m.name.name),
                    "fork supports `blocking` and `out_of_order tags N` methods",
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
                "bus.{} returns no value; `fork` it as a statement",
                m.name.name
            )));
        }
        // Request payload evaluates now (same cycle as the request, no
        // tick between args and req_valid — v1's inline arg emission).
        let mut lowered = Vec::with_capacity(args.len());
        for a in args {
            lowered.push(self.lower_expr_no_ports(call_arg_expr(a))?);
        }
        // The response destination is declared + zero-init at the fork
        // site (v1 emits `T x = {};`), so reads between fork and join_all
        // see a defined-but-zero local. `Discard` carries no dest.
        let dest_local = match dest {
            BusCallDest::Declare(name) => Some(self.declare(name)),
            BusCallDest::Discard => None,
        };
        let desc = crate::ir::TlmForkDesc {
            bus_field: bind,
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
fn bus_port(bind: &str, tail: &[&str]) -> PortRef {
    let mut port_path = Vec::with_capacity(1 + tail.len());
    port_path.push(bind.to_string());
    port_path.extend(tail.iter().map(|s| s.to_string()));
    PortRef {
        testbench_field: "dut".to_string(),
        port_path,
        direction: None,
        width: None,
        access: PortAccess::Port,
        lane: None,
    }
}
