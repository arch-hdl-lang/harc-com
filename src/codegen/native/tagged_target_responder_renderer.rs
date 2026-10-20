//! Native C++ tagged/out-of-order target-side TLM responder emission.

use super::*;

impl Emitter {
    pub(super) fn emit_bound_tagged_tlm_target_actors(
        &mut self,
        t: &TargetTlmThread,
        method: &TlmMethod,
        instance: &str,
        depth: usize,
        binding: &(BusDecl, String, String),
        subs: &std::collections::HashMap<String, String>,
        local_event_types: &[(String, String)],
        local_field_types: &[(String, String)],
    ) {
        let Some(tags_expr) = &method.out_of_order_tags else {
            self.errors.push(format!(
                "target TLM thread bus.{} is out_of_order but has no tag count",
                method.name.name
            ));
            return;
        };
        let Some(tag_count_u64) = fold_int_literal(tags_expr) else {
            self.errors.push(format!(
                "target TLM thread bus.{} requires a literal `out_of_order tags N` count for responder-lane lowering",
                method.name.name
            ));
            return;
        };
        if tag_count_u64 == 0 || tag_count_u64 > 64 {
            self.errors.push(format!(
                "target TLM thread bus.{} supports 1..64 out_of_order target tags, got {}",
                method.name.name, tag_count_u64
            ));
            return;
        }
        let tag_count = tag_count_u64 as usize;

        let (_, root, sig_prefix) = binding;
        let inst_tag = instance.replace('.', "_");
        let method_tag = method.name.name.replace('.', "_");
        let prefix = format!("_{inst_tag}_{method_tag}_target_ooo");
        let req_valid = self.bus_signal_name(sig_prefix, &method.name.name, "req_valid");
        let req_ready = self.bus_signal_name(sig_prefix, &method.name.name, "req_ready");
        let rsp_valid = self.bus_signal_name(sig_prefix, &method.name.name, "rsp_valid");
        let rsp_ready = self.bus_signal_name(sig_prefix, &method.name.name, "rsp_ready");
        let rsp_data = self.bus_signal_name(sig_prefix, &method.name.name, "rsp_data");
        let req_tag = self.bus_signal_name(sig_prefix, &method.name.name, "req_tag");
        let rsp_tag = self.bus_signal_name(sig_prefix, &method.name.name, "rsp_tag");

        let dispatcher_slot = format!("{prefix}_dispatcher_slot");
        let arbiter_slot = format!("{prefix}_arbiter_slot");
        let dispatcher_sched = format!("{prefix}_dispatcher_sched");
        let arbiter_sched = format!("{prefix}_arbiter_sched");
        let lane_busy = format!("{prefix}_lane_busy");
        let lane_req_valid = format!("{prefix}_lane_req_valid");
        let lane_rsp_valid = format!("{prefix}_lane_rsp_valid");
        let lane_rsp_data = format!("{prefix}_lane_rsp_data");

        self.pad(depth);
        writeln!(
            self.out,
            "std::array<std::atomic<bool>, {tag_count}> {lane_busy}{{}};"
        )
        .ok();
        self.pad(depth);
        writeln!(
            self.out,
            "std::array<std::atomic<bool>, {tag_count}> {lane_req_valid}{{}};"
        )
        .ok();
        self.pad(depth);
        writeln!(
            self.out,
            "std::array<std::atomic<bool>, {tag_count}> {lane_rsp_valid}{{}};"
        )
        .ok();
        for (arg_name, arg_ty) in &method.args {
            let cty = self.c_type_for_param(arg_ty);
            let arr = format!("{prefix}_arg_{}", arg_name.name);
            self.pad(depth);
            writeln!(self.out, "std::array<{cty}, {tag_count}> {arr}{{}};").ok();
        }
        if let Some(ret_ty) = &method.ret {
            let cty = self.c_type_for_param(ret_ty);
            self.pad(depth);
            writeln!(
                self.out,
                "std::array<{cty}, {tag_count}> {lane_rsp_data}{{}};"
            )
            .ok();
        }

        self.pad(depth);
        writeln!(self.out, "_post_eval_services.push_back([&]() {{").ok();
        self.pad(depth + 1);
        writeln!(self.out, "bool _tlm_ready = false;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "if ({root}->{req_valid}) {{").ok();
        self.pad(depth + 2);
        writeln!(self.out, "auto _tag = (size_t){root}->{req_tag};").ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "_tlm_ready = _tag < {tag_count} && !{lane_busy}[_tag].load() && !{lane_req_valid}[_tag].load();"
        )
        .ok();
        self.pad(depth + 1);
        writeln!(self.out, "}} else {{").ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "for (size_t i = 0; i < {tag_count}; ++i) if (!{lane_busy}[i].load() && !{lane_req_valid}[i].load()) {{ _tlm_ready = true; break; }}"
        )
        .ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        self.pad(depth + 1);
        writeln!(self.out, "{root}->{req_ready} = _tlm_ready;").ok();
        self.pad(depth);
        writeln!(self.out, "}});").ok();

        if self.mt {
            self.pad(depth);
            writeln!(self.out, "harc_rt::ThreadScheduler {dispatcher_sched};").ok();
        }
        self.pad(depth);
        writeln!(self.out, "harc_rt::ThreadSlot {dispatcher_slot};").ok();
        self.pad(depth);
        if self.mt {
            writeln!(
                self.out,
                "{dispatcher_sched}.slots.push_back(&{dispatcher_slot});"
            )
            .ok();
            self.actor_threads
                .push((dispatcher_sched.clone(), dispatcher_slot.clone()));
        } else {
            writeln!(self.out, "sched.slots.push_back(&{dispatcher_slot});").ok();
        }
        self.pad(depth);
        writeln!(
            self.out,
            "auto {dispatcher_slot}_lambda = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{",
        )
        .ok();
        self.pad(depth + 1);
        writeln!(self.out, "{root}->{req_ready} = 0;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "while (true) {{").ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "co_await harc_rt::wait_until(_slot, [&]{{ return {root}->{req_valid} && {root}->{req_ready}; }});"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "{{").ok();
        self.pad(depth + 3);
        writeln!(self.out, "auto _tag = (size_t){root}->{req_tag};").ok();
        for (arg_name, arg_ty) in &method.args {
            let arg_port = self.bus_signal_name(sig_prefix, &method.name.name, &arg_name.name);
            let arr = format!("{prefix}_arg_{}", arg_name.name);
            let read_expr = self.record_unpack_expr(arg_ty, &format!("{root}->{arg_port}"));
            self.pad(depth + 3);
            writeln!(self.out, "{arr}[_tag] = {read_expr};").ok();
        }
        self.pad(depth + 3);
        writeln!(self.out, "{lane_busy}[_tag].store(true);").ok();
        self.pad(depth + 3);
        writeln!(self.out, "{lane_req_valid}[_tag].store(true);").ok();
        self.pad(depth + 3);
        writeln!(
            self.out,
            "{instance}._last_in_cycle = (uint64_t)cycle_count;"
        )
        .ok();
        self.emit_tlm_call_trace_event(
            instance,
            "bus",
            &method.name.name,
            "request",
            "target",
            Some("_tag"),
            depth + 3,
        );
        self.pad(depth + 2);
        writeln!(self.out, "}}").ok();
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
            "{dispatcher_slot}.thread = {dispatcher_slot}_lambda(&{dispatcher_slot});"
        )
        .ok();

        for lane in 0..tag_count {
            let lane_slot = format!("{prefix}_lane{lane}_slot");
            let lane_sched = format!("{prefix}_lane{lane}_sched");
            if self.mt {
                self.pad(depth);
                writeln!(self.out, "harc_rt::ThreadScheduler {lane_sched};").ok();
            }
            self.pad(depth);
            writeln!(self.out, "harc_rt::ThreadSlot {lane_slot};").ok();
            self.pad(depth);
            if self.mt {
                writeln!(self.out, "{lane_sched}.slots.push_back(&{lane_slot});").ok();
                self.actor_threads
                    .push((lane_sched.clone(), lane_slot.clone()));
            } else {
                writeln!(self.out, "sched.slots.push_back(&{lane_slot});").ok();
            }
            self.pad(depth);
            writeln!(
                self.out,
                "auto {lane_slot}_lambda = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{",
            )
            .ok();
            self.pad(depth + 1);
            writeln!(self.out, "while (true) {{").ok();
            self.pad(depth + 2);
            writeln!(
                self.out,
                "co_await harc_rt::wait_until(_slot, [&]{{ return {lane_req_valid}[{lane}].load(); }});"
            )
            .ok();
            self.pad(depth + 2);
            writeln!(self.out, "{lane_req_valid}[{lane}].store(false);").ok();
            for (param, (arg_name, arg_ty)) in t.params.iter().zip(method.args.iter()) {
                let local_name = &param.name.name;
                let local_ty = param.ty.as_ref().unwrap_or(arg_ty);
                let cty = self.c_type_for_param(local_ty);
                let arr = format!("{prefix}_arg_{}", arg_name.name);
                self.pad(depth + 2);
                writeln!(self.out, "{cty} {local_name} = {arr}[{lane}];").ok();
            }

            let prev_subs = std::mem::replace(&mut self.field_subs, subs.clone());
            let mut saved_field_types = Vec::new();
            for (name, ty) in local_field_types {
                let prev = self.let_types.insert(name.clone(), ty.clone());
                saved_field_types.push((name.clone(), prev));
            }
            let mut saved_param_types = Vec::new();
            let mut saved_param_widths = Vec::new();
            for (param, (_, arg_ty)) in t.params.iter().zip(method.args.iter()) {
                let local_ty = param.ty.as_ref().unwrap_or(arg_ty);
                if let Some(name) = type_simple_name(Some(local_ty)) {
                    let prev = self
                        .let_types
                        .insert(param.name.name.clone(), name.to_string());
                    saved_param_types.push((param.name.name.clone(), prev));
                }
                if let Some(width) = scalar_let_width(local_ty) {
                    let prev = self.let_widths.insert(param.name.name.clone(), width);
                    saved_param_widths.push((param.name.name.clone(), prev));
                }
            }
            let mut added_events = Vec::new();
            for (name, ty) in local_event_types {
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
            if let Some(ret_ty) = &method.ret {
                let cty = self.c_type_for_param(ret_ty);
                self.pad(depth + 2);
                writeln!(self.out, "{cty} _tlm_rsp_value{{}};").ok();
            }
            self.pad(depth + 2);
            writeln!(self.out, "bool _tlm_returned = false;").ok();
            self.push_probe_shadow_scope();
            for param in &t.params {
                self.declare_probe_shadow(&param.name.name);
            }
            self.emit_target_tlm_body(
                &t.body,
                method.ret.as_ref().map(|_| "_tlm_rsp_value"),
                method.ret.as_ref(),
                "_tlm_returned",
                depth + 2,
            );
            self.pop_probe_shadow_scope();
            if method.ret.is_some() {
                let runtime_context = self.randomize_context_ref().to_string();
                self.pad(depth + 2);
                writeln!(
                    self.out,
                    "if (!_tlm_returned) {{ sim_log_line(\"FAIL\", \"target TLM thread bus.{} completed without return\"); {}.errors++; }}",
                    method.name.name,
                    runtime_context,
                )
                .ok();
                self.pad(depth + 2);
                writeln!(self.out, "{lane_rsp_data}[{lane}] = _tlm_rsp_value;").ok();
            }
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
            for (name, prev) in saved_param_widths {
                match prev {
                    Some(width) => {
                        self.let_widths.insert(name, width);
                    }
                    None => {
                        self.let_widths.remove(&name);
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

            self.pad(depth + 2);
            writeln!(self.out, "{lane_rsp_valid}[{lane}].store(true);").ok();
            self.pad(depth + 2);
            writeln!(
                self.out,
                "co_await harc_rt::wait_until(_slot, [&]{{ return !{lane_rsp_valid}[{lane}].load(); }});"
            )
            .ok();
            self.pad(depth + 2);
            writeln!(self.out, "{lane_busy}[{lane}].store(false);").ok();
            self.pad(depth + 1);
            writeln!(self.out, "}}").ok();
            self.pad(depth + 1);
            writeln!(self.out, "co_return;").ok();
            self.pad(depth);
            writeln!(self.out, "}};").ok();
            self.pad(depth);
            writeln!(
                self.out,
                "{lane_slot}.thread = {lane_slot}_lambda(&{lane_slot});"
            )
            .ok();
        }

        if self.mt {
            self.pad(depth);
            writeln!(self.out, "harc_rt::ThreadScheduler {arbiter_sched};").ok();
        }
        self.pad(depth);
        writeln!(self.out, "harc_rt::ThreadSlot {arbiter_slot};").ok();
        self.pad(depth);
        if self.mt {
            writeln!(
                self.out,
                "{arbiter_sched}.slots.push_back(&{arbiter_slot});"
            )
            .ok();
            self.actor_threads
                .push((arbiter_sched.clone(), arbiter_slot.clone()));
        } else {
            writeln!(self.out, "sched.slots.push_back(&{arbiter_slot});").ok();
        }
        self.pad(depth);
        writeln!(
            self.out,
            "auto {arbiter_slot}_lambda = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{",
        )
        .ok();
        self.pad(depth + 1);
        writeln!(self.out, "{root}->{rsp_valid} = 0;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "while (true) {{").ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "co_await harc_rt::wait_until(_slot, [&]{{ for (size_t i = 0; i < {tag_count}; ++i) if ({lane_rsp_valid}[i].load()) return true; return false; }});"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "int _sel = -1;").ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "for (int i = {tag_count} - 1; i >= 0; --i) if ({lane_rsp_valid}[(size_t)i].load()) {{ _sel = i; break; }}"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "if (_sel >= 0) {{").ok();
        if method.ret.is_some() {
            let rsp_stmt = method
                .ret
                .as_ref()
                .map(|ret| {
                    self.record_drive_stmt(
                        ret,
                        &format!("{root}->{rsp_data}"),
                        &format!("{lane_rsp_data}[(size_t)_sel]"),
                    )
                })
                .unwrap_or_else(|| {
                    format!(
                        "harc_rt::harc_assign({root}->{rsp_data}, {lane_rsp_data}[(size_t)_sel]);"
                    )
                });
            self.pad(depth + 3);
            writeln!(self.out, "{rsp_stmt}").ok();
        }
        self.pad(depth + 3);
        writeln!(self.out, "{root}->{rsp_tag} = _sel;").ok();
        self.emit_tlm_call_trace_event(
            instance,
            "bus",
            &method.name.name,
            "response",
            "target",
            Some("_sel"),
            depth + 3,
        );
        self.pad(depth + 3);
        writeln!(self.out, "{root}->{rsp_valid} = 1;").ok();
        self.pad(depth + 3);
        writeln!(
            self.out,
            "if (!{root}->{rsp_ready}) co_await harc_rt::wait_until(_slot, [&]{{ return {root}->{rsp_ready}; }});"
        )
        .ok();
        self.pad(depth + 3);
        writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
        self.pad(depth + 3);
        writeln!(self.out, "{root}->{rsp_valid} = 0;").ok();
        self.pad(depth + 3);
        writeln!(self.out, "{lane_rsp_valid}[(size_t)_sel].store(false);").ok();
        self.pad(depth + 3);
        writeln!(
            self.out,
            "{instance}._last_out_cycle = (uint64_t)cycle_count;"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "}}").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        self.pad(depth + 1);
        writeln!(self.out, "co_return;").ok();
        self.pad(depth);
        writeln!(self.out, "}};").ok();
        self.pad(depth);
        writeln!(
            self.out,
            "{arbiter_slot}.thread = {arbiter_slot}_lambda(&{arbiter_slot});"
        )
        .ok();
    }
}
