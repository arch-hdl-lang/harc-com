//! Native C++ blocking target-side TLM responder emission.

use super::*;

impl Emitter {
    /// Emit a target-side TLM body with source-level `return` lowered into a
    /// response temp plus a returned flag. This keeps responder actors in the
    /// same coroutine after nested/early returns: remaining statements are
    /// skipped, loops break, then the common rsp_valid/rsp_ready epilogue runs.
    pub(super) fn emit_target_tlm_body(
        &mut self,
        block: &Block,
        rsp_var: Option<&str>,
        rsp_ty: Option<&TypeExpr>,
        returned_var: &str,
        depth: usize,
    ) {
        self.push_probe_shadow_scope();
        // A `let` must remain visible to later siblings in this source block,
        // so guarding only its declaration would change lexical scope. Keep
        // one guard open from each declaration through the end of the block:
        // the declaration is skipped after an earlier return, while later
        // statements can still refer to it on the live path.
        let mut let_regions = 0usize;
        for (index, stmt) in block.stmts.iter().enumerate() {
            let prior_source_id = self.current_source_id;
            let source_id = block.stmt_source(index);
            if source_id.is_known() {
                self.current_source_id = source_id;
            }
            if matches!(stmt.kind, StmtKind::Let(_)) {
                let region_depth = depth + let_regions;
                self.pad(region_depth);
                writeln!(self.out, "if (!{returned_var}) {{").ok();
                self.emit_target_tlm_stmt(stmt, rsp_var, rsp_ty, returned_var, region_depth + 1);
                if let StmtKind::Let(let_stmt) = &stmt.kind {
                    self.declare_probe_shadow(&let_stmt.name.name);
                }
                let_regions += 1;
                self.current_source_id = prior_source_id;
                continue;
            }
            let stmt_depth = depth + let_regions;
            self.pad(stmt_depth);
            writeln!(self.out, "if (!{returned_var}) {{").ok();
            self.emit_target_tlm_stmt(stmt, rsp_var, rsp_ty, returned_var, stmt_depth + 1);
            self.pad(stmt_depth);
            writeln!(self.out, "}}").ok();
            self.current_source_id = prior_source_id;
        }
        for region in (0..let_regions).rev() {
            self.pad(depth + region);
            writeln!(self.out, "}}").ok();
        }
        self.pop_probe_shadow_scope();
    }

    fn emit_target_tlm_stmt(
        &mut self,
        stmt: &Stmt,
        rsp_var: Option<&str>,
        rsp_ty: Option<&TypeExpr>,
        returned_var: &str,
        depth: usize,
    ) {
        match &stmt.kind {
            StmtKind::Return(opt) => {
                self.pad(depth);
                match (rsp_var, opt) {
                    (Some(dst), Some(expr)) => {
                        write!(self.out, "harc_rt::harc_assign({dst}, ").ok();
                        if let Some(ty) = rsp_ty {
                            self.emit_value_for_destination(ty, expr);
                        } else {
                            self.emit_expr(expr);
                        }
                        writeln!(self.out, ");").ok();
                    }
                    (Some(_), None) => {
                        self.errors
                            .push("target TLM value-returning method uses bare `return`".into());
                    }
                    (None, Some(_)) => {
                        self.errors
                            .push("target TLM void method must not return a value".into());
                    }
                    (None, None) => {}
                }
                self.pad(depth);
                writeln!(self.out, "{returned_var} = true;").ok();
            }
            StmtKind::If(i) => {
                self.pad(depth);
                write!(self.out, "if (").ok();
                self.emit_expr(&i.cond);
                writeln!(self.out, ") {{").ok();
                self.emit_target_tlm_body(&i.then_block, rsp_var, rsp_ty, returned_var, depth + 1);
                for (cond, block) in &i.elsifs {
                    self.pad(depth);
                    write!(self.out, "}} else if (").ok();
                    self.emit_expr(cond);
                    writeln!(self.out, ") {{").ok();
                    self.emit_target_tlm_body(block, rsp_var, rsp_ty, returned_var, depth + 1);
                }
                if let Some(else_block) = &i.else_block {
                    self.pad(depth);
                    writeln!(self.out, "}} else {{").ok();
                    self.emit_target_tlm_body(else_block, rsp_var, rsp_ty, returned_var, depth + 1);
                }
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            StmtKind::For(f) => {
                self.pad(depth);
                let var = &f.var.name;
                // A `for` variable binds a C++ name that hides any
                // file-scope one, exactly as a `let` does — register it so a
                // loop variable named like a suppressed enum variant is not
                // diagnosed. (`emit_let` does the same for `let`.)
                self.declared_value_names.insert(var.clone());
                if let ExprKind::RangeLit {
                    lo: Some(lo),
                    hi: Some(hi),
                } = &*f.iter.kind
                {
                    write!(self.out, "for (int64_t {var} = ").ok();
                    self.emit_expr(lo);
                    // `for i in lo .. hi` is INCLUSIVE of `hi` (matches ARCH);
                    // emit `<=`, not `<`. Keep in lockstep with the tbir
                    // lowering in `ir::lower::control::lower_for`.
                    write!(self.out, "; {var} <= ").ok();
                    self.emit_expr(hi);
                    writeln!(self.out, "; {var}++) {{").ok();
                    self.push_probe_shadow_scope();
                    self.declare_probe_shadow(var);
                    self.emit_target_tlm_body(&f.body, rsp_var, rsp_ty, returned_var, depth + 1);
                    self.pop_probe_shadow_scope();
                    self.pad(depth + 1);
                    writeln!(self.out, "if ({returned_var}) break;").ok();
                    self.pad(depth);
                    writeln!(self.out, "}}").ok();
                } else {
                    write!(self.out, "for (auto& {var} : ").ok();
                    self.emit_expr(&f.iter);
                    writeln!(self.out, ") {{").ok();
                    self.push_probe_shadow_scope();
                    self.declare_probe_shadow(var);
                    self.emit_target_tlm_body(&f.body, rsp_var, rsp_ty, returned_var, depth + 1);
                    self.pop_probe_shadow_scope();
                    self.pad(depth + 1);
                    writeln!(self.out, "if ({returned_var}) break;").ok();
                    self.pad(depth);
                    writeln!(self.out, "}}").ok();
                }
            }
            StmtKind::Repeat(r) => {
                self.pad(depth);
                let counter = self.fresh_internal_name("repeat_counter");
                write!(self.out, "for (int64_t {counter} = 0; {counter} < ").ok();
                self.emit_expr(&r.count);
                writeln!(self.out, "; {counter}++) {{").ok();
                self.emit_target_tlm_body(&r.body, rsp_var, rsp_ty, returned_var, depth + 1);
                self.pad(depth + 1);
                writeln!(self.out, "if ({returned_var}) break;").ok();
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            StmtKind::While { cond, body, .. } => {
                self.pad(depth);
                write!(self.out, "while (!{returned_var} && (").ok();
                self.emit_expr(cond);
                writeln!(self.out, ")) {{").ok();
                self.emit_target_tlm_body(body, rsp_var, rsp_ty, returned_var, depth + 1);
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            StmtKind::Loop(body) => {
                self.pad(depth);
                writeln!(self.out, "while (!{returned_var}) {{").ok();
                self.emit_target_tlm_body(body, rsp_var, rsp_ty, returned_var, depth + 1);
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            _ => {
                self.emit_stmt(stmt, depth);
            }
        }
    }

    /// Emit `thread bus.method(args) ... return expr end thread` items in a
    /// bound transactor as target-side TLM responder actors. The actor owns
    /// the method's target handshake:
    ///
    /// 1. Assert `<method>_req_ready`.
    /// 2. Wait for a request handshake and capture args (plus tag for OOO).
    /// 3. Run the HARC body in coroutine context.
    /// 4. Drive optional response payload/tag and hold `rsp_valid` until
    ///    the DUT initiator asserts `rsp_ready`.
    pub(super) fn emit_bound_tlm_target_actors(
        &mut self,
        comp: &ComponentDecl,
        instance: &str,
        depth: usize,
        binding: &(BusDecl, String, String),
    ) {
        let mut subs = std::collections::HashMap::new();
        let mut local_event_types: Vec<(String, String)> = Vec::new();
        let mut local_field_types: Vec<(String, String)> = Vec::new();
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

        for it in &comp.items {
            let ComponentItem::TargetTlmThread(t) = it else {
                continue;
            };
            let Some(bus_alias) = t.method.segments.first().map(|s| s.name.as_str()) else {
                continue;
            };
            if bus_alias != "bus" {
                self.errors.push(format!(
                    "target TLM thread in transactor `{}` must target `bus.<method>`, got `{}`",
                    comp.name.name,
                    t.method
                        .segments
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(".")
                ));
                continue;
            }
            if t.method.segments.len() != 2 {
                self.errors.push(format!(
                    "target TLM thread in transactor `{}` must use `thread bus.<method>(...)`",
                    comp.name.name
                ));
                continue;
            }
            let method_name = &t.method.segments[1].name;
            let Some(method) = binding
                .0
                .tlm_methods
                .iter()
                .find(|m| &m.name.name == method_name)
                .cloned()
            else {
                self.errors.push(format!(
                    "target TLM thread `{}`: bus `{}` has no tlm_method `{}`",
                    instance, binding.0.name.name, method_name
                ));
                continue;
            };
            if t.params.len() != method.args.len() {
                self.errors.push(format!(
                    "target TLM thread bus.{}: expected {} arg(s), got {}",
                    method.name.name,
                    method.args.len(),
                    t.params.len(),
                ));
                continue;
            }

            if method.ret.is_some() && !t.body.stmts.iter().any(stmt_contains_return) {
                self.errors.push(format!(
                    "target TLM thread bus.{} must end with `return <expr>` because the method returns a value",
                    method.name.name
                ));
                continue;
            }

            let (_, root, sig_prefix) = binding;
            let inst_tag = instance.replace('.', "_");
            let method_tag = method.name.name.replace('.', "_");
            let slot_var = format!("_{inst_tag}_{method_tag}_target_slot");
            let sched_var = format!("_{inst_tag}_{method_tag}_target_sched");
            let req_valid = self.bus_signal_name(sig_prefix, &method.name.name, "req_valid");
            let req_ready = self.bus_signal_name(sig_prefix, &method.name.name, "req_ready");
            let rsp_valid = self.bus_signal_name(sig_prefix, &method.name.name, "rsp_valid");
            let rsp_ready = self.bus_signal_name(sig_prefix, &method.name.name, "rsp_ready");
            let rsp_data = self.bus_signal_name(sig_prefix, &method.name.name, "rsp_data");
            let req_tag = self.bus_signal_name(sig_prefix, &method.name.name, "req_tag");
            let rsp_tag = self.bus_signal_name(sig_prefix, &method.name.name, "rsp_tag");
            let tagged = method.mode.name == "out_of_order";
            if method.mode.name != "blocking" && !tagged {
                self.errors.push(format!(
                    "target TLM thread bus.{} supports `blocking` and `out_of_order tags N`, not `{}`",
                    method.name.name, method.mode.name,
                ));
                continue;
            }
            if tagged {
                self.emit_bound_tagged_tlm_target_actors(
                    t,
                    &method,
                    instance,
                    depth,
                    binding,
                    &subs,
                    &local_event_types,
                    &local_field_types,
                );
                continue;
            }

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
            )
            .ok();
            self.pad(depth + 1);
            writeln!(self.out, "{root}->{req_ready} = 0;").ok();
            self.pad(depth + 1);
            writeln!(self.out, "{root}->{rsp_valid} = 0;").ok();
            self.pad(depth + 1);
            writeln!(self.out, "while (true) {{").ok();
            self.pad(depth + 2);
            writeln!(self.out, "{root}->{req_ready} = 1;").ok();
            self.pad(depth + 2);
            writeln!(
                self.out,
                "co_await harc_rt::wait_until(_slot, [&]{{ return {root}->{req_valid} && {root}->{req_ready}; }});",
            )
            .ok();
            for (param, (arg_name, arg_ty)) in t.params.iter().zip(method.args.iter()) {
                let local_name = &param.name.name;
                let local_ty = param.ty.as_ref().unwrap_or(arg_ty);
                let cty = self.c_type_for_param(local_ty);
                let arg_port = self.bus_signal_name(sig_prefix, &method.name.name, &arg_name.name);
                let read_expr = self.record_unpack_expr(local_ty, &format!("{root}->{arg_port}"));
                self.pad(depth + 2);
                writeln!(self.out, "{cty} {local_name} = {read_expr};").ok();
            }
            if tagged {
                self.pad(depth + 2);
                writeln!(self.out, "auto _tlm_req_tag = {root}->{req_tag};").ok();
            }
            self.pad(depth + 2);
            writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
            self.pad(depth + 2);
            writeln!(self.out, "{root}->{req_ready} = 0;").ok();
            self.pad(depth + 2);
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
                None,
                depth + 2,
            );

            let prev_subs = std::mem::replace(&mut self.field_subs, subs.clone());
            let mut saved_field_types = Vec::new();
            for (name, ty) in &local_field_types {
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
                let rsp_stmt = method
                    .ret
                    .as_ref()
                    .map(|ret| {
                        self.record_drive_stmt(
                            ret,
                            &format!("{root}->{rsp_data}"),
                            "_tlm_rsp_value",
                        )
                    })
                    .unwrap_or_else(|| {
                        format!("harc_rt::harc_assign({root}->{rsp_data}, _tlm_rsp_value);")
                    });
                self.pad(depth + 2);
                writeln!(self.out, "{rsp_stmt}").ok();
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

            if tagged {
                self.pad(depth + 2);
                writeln!(self.out, "{root}->{rsp_tag} = _tlm_req_tag;").ok();
            }
            self.emit_tlm_call_trace_event(
                instance,
                "bus",
                &method.name.name,
                "response",
                "target",
                None,
                depth + 2,
            );
            self.pad(depth + 2);
            writeln!(self.out, "{root}->{rsp_valid} = 1;").ok();
            self.pad(depth + 2);
            writeln!(
                self.out,
                "if (!{root}->{rsp_ready}) co_await harc_rt::wait_until(_slot, [&]{{ return {root}->{rsp_ready}; }});",
            )
            .ok();
            self.pad(depth + 2);
            writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
            self.pad(depth + 2);
            writeln!(self.out, "{root}->{rsp_valid} = 0;").ok();
            self.pad(depth + 2);
            writeln!(
                self.out,
                "{instance}._last_out_cycle = (uint64_t)cycle_count;"
            )
            .ok();
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
        }
    }
}
