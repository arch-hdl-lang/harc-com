//! Native C++ initiator-side TLM call and join emission.

use super::*;

impl Emitter {
    /// Emit a scoreboard declaration as a C++ struct. Only the field list
    /// is lowered for v0 — `connect` clauses and `on` handlers in the
    /// scoreboard body would need event registration that's not in scope.
    /// Each field default-initialises (queues to empty, ints to 0). The
    /// user drives push/pop/etc. directly from test code.
    /// Resolve a bus-bound field access. Returns `Some(c++_string)`
    /// when `target.name` (or `target.ch.name`) names a signal under
    /// a known bus binding; `None` otherwise (caller falls back to
    /// generic field-access lowering).
    ///
    /// Naming convention mirrors arch-com §19.6: each plain bus
    /// signal flattens to `<binding>_<signal>` on the DUT pointer,
    /// each handshake_channel signal flattens to
    /// `<binding>_<channel>_<signal>` (and the implicit `valid` /
    /// `ready` signals flatten the same way).
    /// Detect `<bus>.<method>(args)` for a `tlm_method` declared on a bus
    /// binding and emit ARCH-compatible blocking req/rsp wire protocol:
    /// `<prefix>_<method>_req_valid`, `<prefix>_<method>_<arg>`,
    /// `<prefix>_<method>_req_ready`, `<prefix>_<method>_rsp_valid`,
    /// optional `<prefix>_<method>_rsp_data`, and
    /// `<prefix>_<method>_rsp_ready`.
    pub(super) fn try_emit_bus_tlm_method(
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
            name: method_name,
        } = &*callee.kind
        else {
            return false;
        };
        let ExprKind::Ident(id) = &*target.kind else {
            return false;
        };
        let Some((bus, root, sig_prefix)) = self.bus_bindings.get(&id.name).cloned() else {
            return false;
        };
        let Some(method) = bus
            .tlm_methods
            .iter()
            .find(|m| m.name.name == method_name.name)
            .cloned()
        else {
            return false;
        };
        if method.mode.name != "blocking" {
            self.errors.push(format!(
                "bus.{}: HARC direct-Verilator lowering currently supports only `blocking` tlm_method calls; `{}` is parsed for ARCH compatibility but not lowered here",
                method.name.name, method.mode.name
            ));
            return true;
        }
        if args.len() != method.args.len() {
            self.errors.push(format!(
                "bus.{}: expected {} arg(s), got {}",
                method.name.name,
                method.args.len(),
                args.len(),
            ));
            return true;
        }
        if let_name.is_some() && method.ret.is_none() {
            self.errors.push(format!(
                "bus.{} returns no value; use it as a statement",
                method.name.name,
            ));
            return true;
        }

        let req_valid = self.bus_signal_name(&sig_prefix, &method.name.name, "req_valid");
        let req_ready = self.bus_signal_name(&sig_prefix, &method.name.name, "req_ready");
        let rsp_valid = self.bus_signal_name(&sig_prefix, &method.name.name, "rsp_valid");
        let rsp_ready = self.bus_signal_name(&sig_prefix, &method.name.name, "rsp_ready");
        let rsp_data = self.bus_signal_name(&sig_prefix, &method.name.name, "rsp_data");
        let component = self.trace_component_context();

        self.pad(depth);
        writeln!(self.out, "// bus.{} tlm_method", method.name.name).ok();
        for ((arg_name, _), arg) in method.args.iter().zip(args.iter()) {
            let sig_port = self.bus_signal_name(&sig_prefix, &method.name.name, &arg_name.name);
            self.pad(depth);
            write!(self.out, "harc_rt::harc_assign({root}->{sig_port}, ").ok();
            match arg {
                CallArg::Expr(e) => self.emit_expr(e),
                CallArg::Named { value, .. } => self.emit_expr(value),
            }
            writeln!(self.out, ");").ok();
        }
        self.emit_tlm_call_trace_event(
            &component,
            &id.name,
            &method.name.name,
            "request",
            "initiator",
            None,
            depth,
        );
        self.pad(depth);
        writeln!(self.out, "{root}->{req_valid} = 1;").ok();
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
                &format!("{root}->{req_ready}"),
                crate::codegen::TLM_WAIT_BOUND,
                advance,
                &format!("TLM {}.{} request", id.name, method.name.name),
            )
        )
        .ok();
        self.pad(depth);
        writeln!(self.out, "{advance};").ok();
        self.pad(depth);
        writeln!(self.out, "{root}->{req_valid} = 0;").ok();

        self.pad(depth);
        writeln!(self.out, "{root}->{rsp_ready} = 1;").ok();
        if let Some(name) = let_name {
            if let Some(ret) = &method.ret {
                self.pad(depth);
                let cty = self.record_field_c_type(ret);
                writeln!(self.out, "{cty} {name} = {{}};").ok();
            }
        }
        self.pad(depth);
        writeln!(self.out, "{{").ok();
        self.pad(depth + 1);
        writeln!(self.out, "bool _rsp_ok = true;").ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "{}",
            crate::codegen::bounded_handshake_wait_into(
                &format!("{root}->{rsp_valid}"),
                crate::codegen::TLM_WAIT_BOUND,
                advance,
                &format!("TLM {}.{} response", id.name, method.name.name),
                "_rsp_ok",
            )
        )
        .ok();
        self.pad(depth + 1);
        writeln!(self.out, "if (_rsp_ok) {{").ok();
        self.emit_tlm_call_trace_event(
            &component,
            &id.name,
            &method.name.name,
            "response",
            "initiator",
            None,
            depth + 2,
        );
        if let Some(name) = let_name {
            if let Some(ret) = &method.ret {
                self.pad(depth + 2);
                let read_expr = self.record_unpack_expr(ret, &format!("{root}->{rsp_data}"));
                writeln!(self.out, "{name} = {read_expr};").ok();
            }
        }
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        self.pad(depth);
        writeln!(self.out, "}}").ok();
        self.pad(depth);
        if self.in_coroutine {
            writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
        } else {
            writeln!(self.out, "tick();").ok();
        }
        self.pad(depth);
        writeln!(self.out, "{root}->{rsp_ready} = 0;").ok();

        if let Some(inst) = self.current_component_instance.clone() {
            self.pad(depth);
            writeln!(self.out, "{inst}._last_out_cycle = (uint64_t)cycle_count;").ok();
            self.pad(depth);
            writeln!(self.out, "{inst}._last_in_cycle = (uint64_t)cycle_count;").ok();
        }
        true
    }

    /// Detect `fork bus.<method>(args)` and emit only the request side of a
    /// bus-level `tlm_method` call. A later `join_all` drains the response
    /// side and assigns any captured return values.
    pub(super) fn try_emit_bus_tlm_fork(
        &mut self,
        e: &Expr,
        let_name: Option<&str>,
        depth: usize,
    ) -> bool {
        let ExprKind::ForkCall { call } = &*e.kind else {
            return false;
        };
        let ExprKind::Call { callee, args } = &*call.kind else {
            self.errors
                .push("`fork` RHS currently requires a direct bus tlm_method call".into());
            return true;
        };
        let ExprKind::Field {
            target,
            name: method_name,
        } = &*callee.kind
        else {
            self.errors
                .push("`fork` RHS currently requires a direct bus tlm_method call".into());
            return true;
        };
        let ExprKind::Ident(id) = &*target.kind else {
            self.errors
                .push("`fork` RHS currently requires `bus.method(args)`".into());
            return true;
        };
        let Some((bus, root, sig_prefix)) = self.bus_bindings.get(&id.name).cloned() else {
            self.errors.push(format!(
                "`fork {}.{}(...)`: `{}` is not a known bus binding",
                id.name, method_name.name, id.name
            ));
            return true;
        };
        let Some(method) = bus
            .tlm_methods
            .iter()
            .find(|m| m.name.name == method_name.name)
            .cloned()
        else {
            self.errors.push(format!(
                "bus `{}` has no tlm_method `{}`",
                bus.name.name, method_name.name
            ));
            return true;
        };
        if args.len() != method.args.len() {
            self.errors.push(format!(
                "bus.{}: expected {} arg(s), got {}",
                method.name.name,
                method.args.len(),
                args.len(),
            ));
            return true;
        }

        let ret_type = method.ret.as_ref().map(|t| self.record_field_c_type(t));
        let component = self.trace_component_context();
        if let Some(name) = let_name {
            let cty = ret_type.clone().unwrap_or_else(|| "uint64_t".into());
            self.pad(depth);
            writeln!(self.out, "{cty} {name} = {{}};").ok();
        } else if method.ret.is_some() {
            self.pad(depth);
            writeln!(
                self.out,
                "// fork bus.{} result intentionally discarded",
                method.name.name
            )
            .ok();
        }

        let req_valid = self.bus_signal_name(&sig_prefix, &method.name.name, "req_valid");
        let req_ready = self.bus_signal_name(&sig_prefix, &method.name.name, "req_ready");
        let req_tag = self.bus_signal_name(&sig_prefix, &method.name.name, "req_tag");
        let tag = if method.mode.name == "out_of_order" {
            let Some(tags) = method.out_of_order_tags.as_ref().and_then(fold_int_literal) else {
                self.errors.push(format!(
                    "`{}.{}` requires a literal positive `out_of_order tags N` count",
                    id.name, method.name.name
                ));
                return true;
            };
            let key = (sig_prefix.clone(), method.name.name.clone());
            let next = self.next_tlm_fork_tag.entry(key).or_insert(0);
            if *next >= tags {
                self.errors.push(format!(
                    "`{}.{}` exceeds its {} outstanding tag slot(s) before `join_all`",
                    id.name, method.name.name, tags
                ));
                return true;
            }
            let tag = *next;
            *next += 1;
            Some(tag)
        } else if method.mode.name == "blocking" {
            None
        } else {
            self.errors.push(format!(
                "bus.{}: HARC RHS-fork lowering supports `blocking` and `out_of_order tags N`, not `{}`",
                method.name.name, method.mode.name
            ));
            return true;
        };

        self.pad(depth);
        writeln!(
            self.out,
            "// fork bus.{} tlm_method issue",
            method.name.name
        )
        .ok();
        for ((arg_name, _), arg) in method.args.iter().zip(args.iter()) {
            let sig_port = self.bus_signal_name(&sig_prefix, &method.name.name, &arg_name.name);
            self.pad(depth);
            write!(self.out, "harc_rt::harc_assign({root}->{sig_port}, ").ok();
            match arg {
                CallArg::Expr(e) => self.emit_expr(e),
                CallArg::Named { value, .. } => self.emit_expr(value),
            }
            writeln!(self.out, ");").ok();
        }
        if let Some(tag) = tag {
            self.pad(depth);
            writeln!(self.out, "{root}->{req_tag} = {tag};").ok();
        }
        let req_tag_expr = tag.map(|_| format!("{root}->{req_tag}"));
        self.emit_tlm_call_trace_event(
            &component,
            &id.name,
            &method.name.name,
            "request",
            "initiator",
            req_tag_expr.as_deref(),
            depth,
        );
        self.pad(depth);
        writeln!(self.out, "{root}->{req_valid} = 1;").ok();
        self.pad(depth);
        if tag.is_some() {
            // Present the request (valid + tag + payload) for exactly the
            // acceptance cycle, then deassert. An out-of-order target accepts
            // combinationally — `req_ready` is high while the addressed tag's
            // slot is idle and drops the same posedge the request is latched.
            // We must NOT spin on `req_ready` before advancing: at this point
            // the DUT mirror has not re-evaluated with the just-written
            // `req_tag`, so `req_ready` still reflects the *previous* tag
            // (whose slot may now be busy) — a stale 0 that would send the
            // initiator into a multi-cycle hold, leaving `req_valid` asserted
            // through a `valid && !ready` window and dropping it while the
            // slot is busy. That trips the DUT's `_auto_tlm_*_req_stable`
            // handshake assertion (a real protocol violation, not a false
            // positive). Advancing exactly one cycle spans the accept edge
            // with the new tag/payload held stable; deasserting the next
            // cycle keeps the request out of any stalled window. (The
            // response is collected later at join_all.)
            if self.in_coroutine {
                writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
            } else {
                writeln!(self.out, "tick();").ok();
            }
        } else {
            // Blocking forks still need the legacy ready-wait: unlike tagged
            // OOO lanes, a blocking target may legitimately hold req_ready low
            // for multiple cycles before accepting the request.
            let advance = if self.in_coroutine {
                "co_await harc_rt::wait_cycles(_slot, 1)"
            } else {
                "tick()"
            };
            // `self.pad(depth)` already emitted above (shared with the OOO
            // branch), so the loop line is not re-padded.
            writeln!(
                self.out,
                "{}",
                crate::codegen::bounded_handshake_wait(
                    &format!("{root}->{req_ready}"),
                    crate::codegen::TLM_WAIT_BOUND,
                    advance,
                    &format!("TLM {}.{} fork request", id.name, method.name.name),
                )
            )
            .ok();
            self.pad(depth);
            writeln!(self.out, "{advance};").ok();
        }
        self.pad(depth);
        writeln!(self.out, "{root}->{req_valid} = 0;").ok();

        self.pending_tlm_forks.push(PendingTlmFork {
            root,
            component,
            bus: id.name.clone(),
            method: method.name.name,
            sig_prefix,
            ret_var: let_name.map(|s| s.to_string()),
            ret_ty: method.ret.clone(),
            tag,
        });
        true
    }

    pub(super) fn emit_tlm_join_all(&mut self, depth: usize) {
        if self.pending_tlm_forks.is_empty() {
            self.pad(depth);
            writeln!(self.out, "// join_all: no pending forked TLM calls").ok();
            return;
        }
        let pending = std::mem::take(&mut self.pending_tlm_forks);
        let tagged = pending.iter().any(|p| p.tag.is_some());
        if tagged {
            self.emit_tagged_tlm_join_all(&pending, depth);
        } else {
            self.emit_ordered_tlm_join_all(&pending, depth);
        }
        for request in pending.iter().filter(|request| request.tag.is_some()) {
            self.next_tlm_fork_tag
                .remove(&(request.sig_prefix.clone(), request.method.clone()));
        }
    }

    fn emit_ordered_tlm_join_all(&mut self, pending: &[PendingTlmFork], depth: usize) {
        for p in pending {
            let rsp_valid = self.bus_signal_name(&p.sig_prefix, &p.method, "rsp_valid");
            let rsp_ready = self.bus_signal_name(&p.sig_prefix, &p.method, "rsp_ready");
            let rsp_data = self.bus_signal_name(&p.sig_prefix, &p.method, "rsp_data");
            self.pad(depth);
            writeln!(self.out, "// join_all bus.{} response", p.method).ok();
            self.pad(depth);
            writeln!(self.out, "{}->{} = 1;", p.root, rsp_ready).ok();
            self.pad(depth);
            writeln!(self.out, "{{").ok();
            self.pad(depth + 1);
            writeln!(self.out, "bool _rsp_ok = true;").ok();
            self.pad(depth + 1);
            writeln!(
                self.out,
                "{}",
                crate::codegen::bounded_handshake_wait_into(
                    &format!("{}->{}", p.root, rsp_valid),
                    crate::codegen::TLM_JOIN_DRAIN_BOUND,
                    if self.in_coroutine {
                        "co_await harc_rt::wait_cycles(_slot, 1)"
                    } else {
                        "tick()"
                    },
                    &format!("TLM {}.{} fork response", p.bus, p.method),
                    "_rsp_ok",
                )
            )
            .ok();
            self.pad(depth + 1);
            writeln!(self.out, "if (_rsp_ok) {{").ok();
            if let Some(var) = &p.ret_var {
                let read_expr = p
                    .ret_ty
                    .as_ref()
                    .map(|ty| self.record_unpack_expr(ty, &format!("{}->{}", p.root, rsp_data)))
                    .unwrap_or_else(|| format!("harc_rt::harc_read({}->{})", p.root, rsp_data));
                self.pad(depth + 2);
                writeln!(self.out, "{var} = {read_expr};").ok();
            }
            self.emit_tlm_call_trace_event(
                &p.component,
                &p.bus,
                &p.method,
                "response",
                "initiator",
                None,
                depth + 2,
            );
            self.pad(depth + 1);
            writeln!(self.out, "}}").ok();
            self.pad(depth);
            writeln!(self.out, "}}").ok();
            self.pad(depth);
            if self.in_coroutine {
                writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
            } else {
                writeln!(self.out, "tick();").ok();
            }
            self.pad(depth);
            writeln!(self.out, "{}->{} = 0;", p.root, rsp_ready).ok();
        }
    }

    fn emit_tagged_tlm_join_all(&mut self, pending: &[PendingTlmFork], depth: usize) {
        self.pad(depth);
        writeln!(self.out, "{{").ok();
        self.pad(depth + 1);
        writeln!(self.out, "int _tlm_pending = {};", pending.len()).ok();
        for (idx, _) in pending.iter().enumerate() {
            self.pad(depth + 1);
            writeln!(self.out, "bool _tlm_seen_{idx} = false;").ok();
        }
        self.pad(depth + 1);
        writeln!(self.out, "int _tlm_budget = 256;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "while (_tlm_pending > 0 && _tlm_budget > 0) {{").ok();
        for p in pending {
            let rsp_ready = self.bus_signal_name(&p.sig_prefix, &p.method, "rsp_ready");
            self.pad(depth + 2);
            writeln!(self.out, "{}->{} = 0;", p.root, rsp_ready).ok();
        }
        self.pad(depth + 2);
        writeln!(self.out, "bool _tlm_accept = false;").ok();
        for (idx, p) in pending.iter().enumerate() {
            let Some(tag) = p.tag else {
                self.errors.push(
                    "cannot mix tagged and untagged RHS-fork TLM calls before one join_all".into(),
                );
                continue;
            };
            let rsp_valid = self.bus_signal_name(&p.sig_prefix, &p.method, "rsp_valid");
            let rsp_ready = self.bus_signal_name(&p.sig_prefix, &p.method, "rsp_ready");
            let rsp_data = self.bus_signal_name(&p.sig_prefix, &p.method, "rsp_data");
            let rsp_tag = self.bus_signal_name(&p.sig_prefix, &p.method, "rsp_tag");
            self.pad(depth + 2);
            writeln!(
                self.out,
                "if (!_tlm_seen_{idx} && {root}->{rsp_valid} && {root}->{rsp_tag} == {tag}) {{",
                root = p.root
            )
            .ok();
            if let Some(var) = &p.ret_var {
                let read_expr = p
                    .ret_ty
                    .as_ref()
                    .map(|ty| self.record_unpack_expr(ty, &format!("{}->{}", p.root, rsp_data)))
                    .unwrap_or_else(|| format!("harc_rt::harc_read({}->{})", p.root, rsp_data));
                self.pad(depth + 3);
                writeln!(self.out, "{var} = {read_expr};").ok();
            }
            let tag_expr = tag.to_string();
            self.emit_tlm_call_trace_event(
                &p.component,
                &p.bus,
                &p.method,
                "response",
                "initiator",
                Some(&tag_expr),
                depth + 3,
            );
            self.pad(depth + 3);
            writeln!(self.out, "{}->{} = 1;", p.root, rsp_ready).ok();
            self.pad(depth + 3);
            writeln!(self.out, "_tlm_seen_{idx} = true;").ok();
            self.pad(depth + 3);
            writeln!(self.out, "_tlm_pending--;").ok();
            self.pad(depth + 3);
            writeln!(self.out, "_tlm_accept = true;").ok();
            self.pad(depth + 2);
            writeln!(self.out, "}}").ok();
        }
        self.pad(depth + 2);
        if self.in_coroutine {
            writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
        } else {
            writeln!(self.out, "tick();").ok();
        }
        self.pad(depth + 2);
        writeln!(self.out, "if (!_tlm_accept) _tlm_budget--;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        self.pad(depth + 1);
        writeln!(self.out, "if (_tlm_pending > 0) {{").ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "sim_log_line(\"FAIL\", \"TLM fork join_all timed out after 256 cycles waiting for a matching response tag\");"
        )
        .ok();
        self.pad(depth + 2);
        let runtime_context = self.randomize_context_ref().to_string();
        writeln!(self.out, "{runtime_context}.errors++;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        for p in pending {
            let rsp_ready = self.bus_signal_name(&p.sig_prefix, &p.method, "rsp_ready");
            self.pad(depth + 1);
            writeln!(self.out, "{}->{} = 0;", p.root, rsp_ready).ok();
        }
        self.pad(depth);
        writeln!(self.out, "}}").ok();
    }
}
