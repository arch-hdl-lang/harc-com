//! Native C++ TSeq, function, block, and statement emission.

use super::*;

impl Emitter {
    pub(super) fn emit_tseq(&mut self, t: &TseqDecl, depth: usize) {
        // Inner type: pull T out of `TSeq<T>`. Default to `int64_t` if
        // the user wrote a bare `tseq`-as-block without a return clause.
        let inner = t
            .return_ty
            .as_ref()
            .and_then(tseq_inner_type)
            .unwrap_or_else(|| "int64_t".to_string());
        self.pad(depth);
        let common_member = self.flavor != Flavor::Legacy;
        let callable_name = global_callable_cpp_name(&t.name.name);
        if common_member {
            write!(
                self.out,
                "std::vector<{inner}> HarcSuiteGlue::{callable_name}("
            )
            .ok();
        } else {
            write!(self.out, "auto {callable_name} = [&](").ok();
        }
        let mut added: Vec<String> = Vec::new();
        let param_names = cpp_param_names(&t.params);
        self.declared_value_names
            .extend(param_names.iter().cloned());
        self.push_probe_shadow_scope();
        for param_name in &param_names {
            self.declare_probe_shadow(param_name);
        }
        for (i, p) in t.params.iter().enumerate() {
            if i > 0 {
                write!(self.out, ", ").ok();
            }
            let pty =
                p.ty.as_ref()
                    .map(c_type_for)
                    .unwrap_or("int64_t".to_string());
            write!(self.out, "{pty} {}", param_names[i]).ok();
            if matches!(&p.ty, Some(TypeExpr::Named { .. })) && p.name.name != "_" {
                if self.pointer_vars.insert(p.name.name.clone()) {
                    added.push(p.name.name.clone());
                }
            }
        }
        if common_member {
            writeln!(self.out, ") {{").ok();
        } else {
            writeln!(self.out, ") -> std::vector<{inner}> {{").ok();
        }
        self.pad(depth + 1);
        writeln!(self.out, "std::vector<{inner}> _result;").ok();
        let unique_registry =
            collision_free_generated_ident("_harc_unique_tseq", &self.declared_value_names);
        self.declared_value_names.insert(unique_registry.clone());
        self.pad(depth + 1);
        writeln!(
            self.out,
            "harc_rt::random::HarcUniqueRegistry {unique_registry};"
        )
        .ok();
        let prev = self.current_yield_target.replace("_result".to_string());
        let previous_unique_scope = self.current_unique_tseq_registry.replace(unique_registry);
        self.emit_block(&t.body, depth + 1);
        self.current_unique_tseq_registry = previous_unique_scope;
        self.current_yield_target = prev;
        self.pad(depth + 1);
        writeln!(self.out, "return _result;").ok();
        for k in added {
            self.pointer_vars.remove(&k);
        }
        self.pop_probe_shadow_scope();
        if common_member {
            self.pad(depth);
            writeln!(self.out, "}}").ok();
        } else {
            self.pad(depth);
            writeln!(self.out, "}};").ok();
        }
    }

    pub(super) fn emit_function(&mut self, f: &FunctionDecl, depth: usize) {
        self.pad(depth);
        let common_member = self.flavor != Flavor::Legacy;
        let callable_name = global_callable_cpp_name(&f.name.name);
        let ret = f
            .return_ty
            .as_ref()
            .map(c_type_for)
            .unwrap_or_else(|| "void".to_string());
        if common_member {
            write!(self.out, "{ret} HarcSuiteGlue::{callable_name}(").ok();
        } else {
            write!(self.out, "auto {callable_name} = [&](").ok();
        }
        // Track which params are Named-typed (pointer-shaped). Add to
        // `pointer_vars` while emitting the body, then remove on exit so
        // siblings don't leak each other's params.
        let mut added: Vec<String> = Vec::new();
        let param_names = cpp_param_names(&f.params);
        self.declared_value_names
            .extend(param_names.iter().cloned());
        self.push_probe_shadow_scope();
        for param_name in &param_names {
            self.declare_probe_shadow(param_name);
        }
        for (i, p) in f.params.iter().enumerate() {
            if i > 0 {
                write!(self.out, ", ").ok();
            }
            let pty =
                p.ty.as_ref()
                    .map(c_type_for)
                    .unwrap_or("int64_t".to_string());
            write!(self.out, "{pty} {}", param_names[i]).ok();
            if matches!(&p.ty, Some(TypeExpr::Named { .. })) && p.name.name != "_" {
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
        self.emit_block(&f.body, depth + 1);
        for k in added {
            self.pointer_vars.remove(&k);
        }
        self.pop_probe_shadow_scope();
        if common_member {
            self.pad(depth);
            writeln!(self.out, "}}").ok();
        } else {
            self.pad(depth);
            writeln!(self.out, "}};").ok();
        }
    }

    pub(super) fn emit_block(&mut self, b: &Block, depth: usize) {
        self.emit_block_with_bindings(b, depth, std::iter::empty::<&str>());
    }

    pub(super) fn emit_block_with_bindings<'a>(
        &mut self,
        b: &Block,
        depth: usize,
        bindings: impl IntoIterator<Item = &'a str>,
    ) {
        self.push_probe_shadow_scope();
        for name in bindings {
            self.declare_probe_shadow(name);
        }
        for (index, s) in b.stmts.iter().enumerate() {
            let prior_source_id = self.current_source_id;
            let source_id = b.stmt_source(index);
            if source_id.is_known() {
                self.current_source_id = source_id;
            }
            self.emit_stmt(s, depth);
            if let StmtKind::Let(let_stmt) = &s.kind {
                self.declare_probe_shadow(&let_stmt.name.name);
            }
            self.current_source_id = prior_source_id;
        }
        self.pop_probe_shadow_scope();
    }

    pub(super) fn emit_stmt(&mut self, s: &Stmt, depth: usize) {
        match &s.kind {
            StmtKind::Let(l) => self.emit_let(l, depth),
            StmtKind::Send { target, value } => {
                self.pad(depth);
                self.emit_signal_assignment(target, value, depth);
            }
            StmtKind::Assign { target, value } => {
                // Same narrowing rejection as the typed-`let` initializer:
                // reassigning a wider value into a declared-width local is
                // the identical defect (`b = a` for a 256-bit `a` into a
                // 200-bit `b`), and TB-IR rejects both.
                if let ExprKind::Ident(id) = &*target.kind {
                    self.check_scalar_assign_width(&id.name, value);
                }
                self.pad(depth);
                self.emit_signal_assignment(target, value, depth);
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
                    // `for i in lo .. hi` — emit as an indexed C++ for loop.
                    // The range is INCLUSIVE of `hi` (matches ARCH), so the
                    // bound is `<=`, not `<`. Keep in lockstep with the tbir
                    // lowering in `ir::lower::control::lower_for`.
                    write!(self.out, "for (int64_t {var} = ").ok();
                    self.emit_expr(lo);
                    write!(self.out, "; {var} <= ").ok();
                    self.emit_expr(hi);
                    writeln!(self.out, "; {var}++) {{").ok();
                    self.emit_block_with_bindings(
                        &f.body,
                        depth + 1,
                        std::iter::once(var.as_str()),
                    );
                    self.pad(depth);
                    writeln!(self.out, "}}").ok();
                } else {
                    // `for x in <seq-expression>` — assume the rhs evaluates
                    // to something C++ can range-iterate over (e.g. a
                    // `std::vector<T>` returned by a `tseq`). The bound
                    // variable is `auto&` so transactions don't get copied
                    // on each iteration.
                    write!(self.out, "for (auto& {var} : ").ok();
                    self.emit_expr(&f.iter);
                    writeln!(self.out, ") {{").ok();
                    self.emit_block_with_bindings(
                        &f.body,
                        depth + 1,
                        std::iter::once(var.as_str()),
                    );
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
                self.emit_block(&r.body, depth + 1);
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            StmtKind::Loop(b) => {
                self.pad(depth);
                writeln!(self.out, "while (true) {{").ok();
                self.emit_block(b, depth + 1);
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            StmtKind::While { cond, body, .. } => {
                self.pad(depth);
                write!(self.out, "while (").ok();
                self.emit_expr(cond);
                writeln!(self.out, ") {{").ok();
                self.emit_block(body, depth + 1);
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            StmtKind::Break { .. } => {
                self.pad(depth);
                writeln!(self.out, "break;").ok();
            }
            StmtKind::Continue { .. } => {
                self.pad(depth);
                writeln!(self.out, "continue;").ok();
            }
            StmtKind::If(i) => {
                self.pad(depth);
                write!(self.out, "if (").ok();
                self.emit_expr(&i.cond);
                writeln!(self.out, ") {{").ok();
                self.emit_block(&i.then_block, depth + 1);
                for (c, b) in &i.elsifs {
                    self.pad(depth);
                    write!(self.out, "}} else if (").ok();
                    self.emit_expr(c);
                    writeln!(self.out, ") {{").ok();
                    self.emit_block(b, depth + 1);
                }
                if let Some(eb) = &i.else_block {
                    self.pad(depth);
                    writeln!(self.out, "}} else {{").ok();
                    self.emit_block(eb, depth + 1);
                }
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            StmtKind::After { duration, body, .. } => {
                self.pad(depth);
                if self.in_coroutine {
                    // Coroutine path: yield to the scheduler so other
                    // coroutines can advance during the wait.
                    write!(self.out, "co_await harc_rt::wait_cycles(_slot, (uint32_t)(").ok();
                    self.emit_expr(duration);
                    writeln!(self.out, "));").ok();
                } else {
                    write!(self.out, "for (int _ck = 0; _ck < ").ok();
                    self.emit_expr(duration);
                    writeln!(self.out, "; _ck++) tick();").ok();
                }
                if !body.stmts.is_empty() {
                    self.pad(depth);
                    writeln!(self.out, "{{").ok();
                    self.emit_block(body, depth + 1);
                    self.pad(depth);
                    writeln!(self.out, "}}").ok();
                }
            }
            StmtKind::Wait {
                duration, clock, ..
            } => {
                self.pad(depth);
                // `wait N cycles on <clock>` — advance simulated time
                // until the named clock has seen N more rising edges.
                // Other clocks continue ticking at their natural rate.
                // Useful for cycle-relative reasoning in multi-clock
                // tests (e.g. "after 2 dst_clk cycles, X should hold").
                if let Some(c) = clock {
                    let idx = match self.clock_names.iter().position(|n| n == &c.name) {
                        Some(i) => i,
                        None => {
                            self.errors.push(format!(
                                "wait ... on {}: no clock named `{}` declared in this test",
                                c.name, c.name
                            ));
                            return;
                        }
                    };
                    // `wait N cycles on <named-clock>` always lowers to
                    // an inline `eval_clocks_until` loop, regardless of
                    // coroutine context. The main loop's
                    // `eval_clocks_until(next_primary_posedge)` advances
                    // by a FULL primary period per iteration, which is
                    // too coarse when the named clock runs faster than
                    // the primary (e.g. `wait 1 cycle on dst_clk` where
                    // dst is 2× src would skip past the target). The
                    // sync path advances to whichever clock's next edge
                    // is sooner, preserving sub-primary-cycle precision.
                    //
                    // The active run coroutine cannot recursively resume
                    // itself while this inline loop advances time. Each
                    // primary edge instead services the selected run's
                    // actor schedulers through `_harc_advance_actors`.
                    write!(
                        self.out,
                        "{{ if (_harc_actor_tick_due) {{ _harc_advance_actors(); _harc_actor_tick_due = false; }} long long _target = clocks_[{idx}].rising_count + (long long)("
                    )
                    .ok();
                    self.emit_expr(duration);
                    writeln!(
                        self.out,
                        "); while (clocks_[{idx}].rising_count < _target && !_fatal) {{"
                    )
                    .ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "int _before_cycle = cycle_count;").ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "long long _next = clocks_[0].next_edge_ps;").ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "for (auto& _ck : clocks_) if (_ck.next_edge_ps < _next) _next = _ck.next_edge_ps;").ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "eval_clocks_until(_next);").ok();
                    self.pad(depth + 1);
                    writeln!(
                        self.out,
                        "if (cycle_count != _before_cycle) _harc_advance_actors();"
                    )
                    .ok();
                    self.pad(depth);
                    writeln!(self.out, "}} for (auto& _c : _checkers) _c(); }}").ok();
                    return;
                }
                // Wall-clock duration (e.g. `wait 100ns`) advances absolute
                // time when multi-clock; under single-clock backward compat
                // we don't have absolute time, so fall back to a comment +
                // a single tick (rare path; the documented form for single
                // clock is `wait N cycles`).
                if let ExprKind::Time(s) = &*duration.kind {
                    match time_literal_to_ps(s) {
                        Ok(ps) => {
                            writeln!(self.out, "eval_clocks_until(now_ps + {ps});").ok();
                        }
                        Err(e) => self.errors.push(e),
                    }
                } else if self.in_coroutine {
                    write!(self.out, "co_await harc_rt::wait_cycles(_slot, (uint32_t)(").ok();
                    self.emit_expr(duration);
                    writeln!(self.out, "));").ok();
                } else {
                    write!(self.out, "for (int _w = 0; _w < ").ok();
                    self.emit_expr(duration);
                    writeln!(self.out, "; _w++) tick();").ok();
                }
            }
            StmtKind::WaitUntil {
                mode,
                conditions,
                timeout,
                ..
            } => {
                self.emit_wait_until(mode, conditions, timeout.as_ref(), depth);
            }
            StmtKind::Assert(v) => {
                // Spec §2 LL(1) table: bare IDENT → named property
                // reference (concurrent); expression with temporal ops →
                // concurrent inline; plain bool expression → immediate
                // point-in-time check. The legacy `property` keyword still
                // parses (back-compat) but is no longer required and no
                // longer changes dispatch.
                if let Some(expr) = &v.expr {
                    if is_concurrent_assertion(expr, &self.properties) {
                        self.emit_property_check("FAIL", v, depth);
                    } else {
                        self.emit_inline_assert(v, depth);
                    }
                }
            }
            StmtKind::Fail { msg, .. } => {
                // Standalone `fail("...")` — unconditional failure log
                // + error counter bump. Same emission as the failure
                // arm of an inline assert, just without the surrounding
                // `if (!cond)` guard.
                let raw = match &*msg.kind {
                    ExprKind::String(s) => s.clone(),
                    _ => "fail() with non-string arg".to_string(),
                };
                let (fmt, caps) = process_interp(&raw);
                self.pad(depth);
                writeln!(self.out, "{{").ok();
                self.emit_interp_capture_temps(&caps, depth + 1);
                self.pad(depth + 1);
                write!(self.out, "sim_log_line(\"FAIL\", \"{}\"", escape_c(&fmt)).ok();
                for (index, c) in caps.iter().enumerate() {
                    self.emit_interp_temp_arg(c, index);
                }
                writeln!(self.out, ");").ok();
                self.pad(depth + 1);
                let runtime_context = self.randomize_context_ref().to_string();
                writeln!(self.out, "{runtime_context}.errors++;").ok();
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            StmtKind::Assume(v) => {
                if let Some(expr) = &v.expr {
                    if is_concurrent_assertion(expr, &self.properties) {
                        self.emit_property_check("ASSUME-FAIL", v, depth);
                    } else {
                        self.emit_inline_assume(v, depth);
                    }
                }
            }
            StmtKind::Cover(v) => {
                // Concurrent cover (spec §5): a witness counter — every
                // primary-clock edge where the expression is true bumps
                // the hit count. End-of-test report aggregates all
                // registered covers (similar to a covergroup, but flat).
                // `cover NAME` resolves a declared property; otherwise
                // the inline expression is used directly.
                let expr = match &v.expr {
                    Some(e) => e,
                    None => return,
                };
                let (label, body) = if let ExprKind::Ident(id) = &*expr.kind {
                    if let Some(b) = self.properties.get(&id.name).cloned() {
                        (id.name.clone(), b)
                    } else {
                        (id.name.clone(), expr.clone())
                    }
                } else {
                    (
                        format!("cov_{}_{}", expr.span.start, expr.span.end),
                        expr.clone(),
                    )
                };
                let tag = format!("c_{}_{}", expr.span.start, expr.span.end);
                self.covers.push(CoverInfo {
                    tag: tag.clone(),
                    label,
                });
                let hits_cell =
                    self.per_run_cell(format!("_cov_{tag}_hits"), "uint64_t", Some("0"));
                self.pad(depth);
                writeln!(self.out, "_checkers.push_back([&]() {{").ok();
                self.pad(depth + 1);
                write!(self.out, "if ((bool)(").ok();
                // Note: temporal ops (|->, |=>, past/rose/etc.) inside a
                // cover body aren't translated for v0 — the same
                // limitation as inline-temporal asserts before the
                // property-decl path was added. Use a `property` decl
                // and `cover <name>` for temporal patterns, or stick to
                // same-cycle bool expressions inline.
                self.emit_expr(&body);
                writeln!(self.out, ")) {hits_cell}++;").ok();
                self.pad(depth);
                writeln!(self.out, "}});").ok();
            }
            StmtKind::Log { args, .. } => self.emit_log(args, None, depth),
            StmtKind::LogF { args, .. } => {
                // First positional string literal is the file path; the
                // remaining args follow `log` semantics (severity ident +
                // message string).
                let (path, rest): (Option<String>, Vec<&CallArg>) = {
                    let mut path = None;
                    let mut rest = Vec::new();
                    for a in args {
                        if path.is_none() {
                            if let CallArg::Expr(e) = a {
                                if let ExprKind::String(s) = &*e.kind {
                                    path = Some(s.clone());
                                    continue;
                                }
                            }
                        }
                        rest.push(a);
                    }
                    (path, rest)
                };
                if path.is_none() {
                    self.errors
                        .push("logf requires a string-literal file path as the first arg".into());
                    return;
                }
                let collected: Vec<CallArg> = rest.into_iter().cloned().collect();
                self.emit_log(&collected, path, depth);
            }
            StmtKind::Expr(e) => {
                // `fork bus.<method>(args)` issues a TLM request without
                // waiting for the response; `join_all` drains responses.
                if self.try_emit_bus_tlm_fork(e, None, depth) {
                    return;
                }
                // `bus.<method>(args)` for bus-level `tlm_method`
                // declarations expands into ARCH-compatible req/rsp
                // protocol wires.
                if self.try_emit_bus_tlm_method(e, None, depth) {
                    return;
                }
                // `bus.<channel>.send(args)` and `bus.<channel>.recv()`
                // expand into a multi-statement v/r handshake. When
                // it's a discarded-result `recv()`, just run the dance.
                if self.try_emit_bus_handshake(e, None, depth) {
                    return;
                }
                // `bitbash(regs)` — RFC §7.7 compile-time-unrolled
                // walk-all over each RW register in the regblock.
                // Emits write(all-ones) + read + assert + write(0) +
                // read + assert for every RW register. RO/WO are
                // skipped with a comment. See docs/ral-support.md.
                if self.try_emit_bitbash(e, depth) {
                    return;
                }
                // `regs.record_write(addr, data)` — passive mirror update
                // keyed by address, with per-register write-callback
                // dispatch. Codegen owns the address decode.
                if self.try_emit_record_write(e, depth) {
                    return;
                }
                self.pad(depth);
                self.emit_expr(e);
                writeln!(self.out, ";").ok();
            }
            StmtKind::JoinAll { .. } => {
                self.emit_tlm_join_all(depth);
            }
            StmtKind::Release(e) => {
                // `release <expr>` — disable the active SV procedural
                // force on a `probe force` signal. Lowers to a single
                // store: `<name>_en = 0`. Read-only probes (no `force`
                // modifier) error.
                if let Some(name) = self.partial_probe_target(e) {
                    let receiver = match &*e.kind {
                        ExprKind::Field { target, .. } => match &*target.kind {
                            ExprKind::Ident(id) => id.name.as_str(),
                            _ => "dut",
                        },
                        _ => "dut",
                    };
                    self.errors.push(format!(
                        "shared DUT access `{receiver}.{name}` names a probe that is not declared identically by every test in the suite"
                    ));
                } else if let Some(probe) = self.resolve_force_probe(e) {
                    self.pad(depth);
                    writeln!(self.out, "dut->rootp->{} = 0;", probe.enable()).ok();
                } else {
                    self.errors.push(
                        "`release` target must be a DUT-handle `<probe_name>` access where the named probe was declared with `probe force`".into(),
                    );
                }
            }
            StmtKind::Return(opt) => {
                self.pad(depth);
                if let Some(e) = opt {
                    write!(self.out, "return ").ok();
                    self.emit_expr(e);
                    writeln!(self.out, ";").ok();
                } else {
                    writeln!(self.out, "return;").ok();
                }
            }
            StmtKind::Yield(e) => {
                self.pad(depth);
                if let Some(target) = self.current_yield_target.clone() {
                    write!(self.out, "{target}.push_back(").ok();
                    self.emit_expr(e);
                    writeln!(self.out, ");").ok();
                } else {
                    self.errors
                        .push("`yield` outside a `tseq` body is not supported in v0 cpp_tb".into());
                }
            }
            StmtKind::Emit { name, args, .. } => {
                // `emit e(v)` → call every subscriber.
                //   - Multi-segment path (`emit drv.req(t)`) → emit
                //     verbatim as `drv.req`.
                //   - Single segment (`emit write_seen(...)` inside a
                //     monitor body) → check field_subs to resolve the
                //     bare name to the instance-qualified field
                //     (`mon.write_seen`).
                let event_name = if name.segments.len() > 1 {
                    name.segments
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(".")
                } else {
                    let raw = name
                        .segments
                        .last()
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    self.field_subs.get(&raw).cloned().unwrap_or(raw)
                };
                let arg = args.first();
                self.pad(depth);
                write!(self.out, "for (auto& _s : {event_name}) _s(").ok();
                if let Some(a) = arg {
                    match a {
                        CallArg::Expr(e) => self.emit_expr(e),
                        CallArg::Named { value, .. } => self.emit_expr(value),
                    }
                }
                writeln!(self.out, ");").ok();
                // Activity tracking (spec §7.x): an `emit` counts as an
                // "out" for the surrounding component instance. Emits
                // outside a component (test-run / free function) don't
                // attribute to any instance.
                if let Some(inst) = self.current_component_instance.clone() {
                    self.pad(depth);
                    writeln!(self.out, "{inst}._last_out_cycle = (uint64_t)cycle_count;").ok();
                }
            }
            StmtKind::On(h) => {
                // Three shapes:
                //   `on obj.method pre/post ... end on` — pre/post hook
                //       on a hookable method. Resolves obj to a known
                //       component-typed binding, then pushes the body
                //       closure into the global `<Type>_<method>_<side>`
                //       vector. The method's body wrap fires hooks
                //       around the body each call.
                //   `on event_name(arg) ... end on` — event subscription.
                //   `on <bool-expr> ... end on`     — cycle trigger.
                if let Some(side) = h.hook {
                    if h.phase == OnPhase::PostEval {
                        self.errors.push(
                            "on <obj>.<method> phase post_eval is not supported; use `pre`/`post` method hooks or a cycle-trigger `on <expr> phase post_eval`".into()
                        );
                        return;
                    }
                    if let Some((comp_ty, method_name, params)) =
                        self.resolve_component_hookable(&h.event)
                    {
                        let side_str = match side {
                            HookSide::Pre => "pre",
                            HookSide::Post => "post",
                        };
                        let param_names = cpp_param_names(&params);
                        self.declared_value_names
                            .extend(param_names.iter().cloned());
                        let arg_decls: Vec<String> = params
                            .iter()
                            .enumerate()
                            .map(|(i, p)| {
                                let ty =
                                    p.ty.as_ref()
                                        .map(|t| self.c_type_for_param(t))
                                        .unwrap_or_else(|| "int64_t".to_string());
                                format!("{ty} {}", param_names[i])
                            })
                            .collect();
                        self.pad(depth);
                        writeln!(
                            self.out,
                            "{comp_ty}_{method_name}_{side_str}.push_back([&]({}) {{",
                            arg_decls.join(", "),
                        )
                        .ok();
                        self.emit_block_with_bindings(
                            &h.body,
                            depth + 1,
                            param_names.iter().map(String::as_str),
                        );
                        self.pad(depth);
                        writeln!(self.out, "}});").ok();
                        return;
                    } else {
                        self.errors.push(
                            "on <obj>.<method> pre/post: obj.method must resolve to a `hookable` on a known component type".into()
                        );
                        return;
                    }
                }
                // RAL per-register write callback: `on regs.REG ... end on`
                // where `regs` is a regblock binding and REG is one of its
                // registers. Lowers to a `void(uint64_t data)` closure
                // stored in `<regs>_cbs.REG`; `record_write` fires it after
                // updating the mirror cell. The body sees the written value
                // as the local `data`. No hook side / period. Recognized
                // before the cycle-trigger fallback (a regblock register
                // has no meaningful boolean-trigger reading — that would
                // issue a bus read every cycle). See docs/ral-support.md §3.2.
                if h.hook.is_none() && !h.periodic {
                    if let ExprKind::Field {
                        target,
                        name: reg_name,
                    } = &*h.event.kind
                    {
                        if let ExprKind::Ident(regs_id) = &*target.kind {
                            let is_reg = self
                                .let_types
                                .get(&regs_id.name)
                                .and_then(|ty| self.regblocks.get(ty))
                                .map(|b| b.registers.iter().any(|r| r.name.name == reg_name.name))
                                .unwrap_or(false);
                            if is_reg {
                                self.pad(depth);
                                writeln!(
                                    self.out,
                                    "{}_cbs.{} = [&](uint64_t data) {{",
                                    regs_id.name, reg_name.name,
                                )
                                .ok();
                                self.emit_block_with_bindings(
                                    &h.body,
                                    depth + 1,
                                    std::iter::once("data"),
                                );
                                self.pad(depth);
                                writeln!(self.out, "}};").ok();
                                return;
                            }
                        }
                    }
                }
                if let ExprKind::Call { callee, args } = &*h.event.kind {
                    if h.phase == OnPhase::PostEval {
                        self.errors.push(
                            "on <event>(arg) phase post_eval is not supported; post_eval is only for cycle-trigger handlers".into()
                        );
                        return;
                    }
                    // Event-subscription path.
                    let raw = match &*callee.kind {
                        ExprKind::Ident(id) => id.name.clone(),
                        ExprKind::Field { target: _, name } => {
                            // `mon.write_e` → emit raw "mon.write_e".
                            // Use a string buffer to capture the qualified name.
                            let mut tmp = String::new();
                            std::mem::swap(&mut self.out, &mut tmp);
                            self.emit_expr(callee);
                            std::mem::swap(&mut self.out, &mut tmp);
                            let _ = name;
                            tmp
                        }
                        _ => {
                            self.errors
                                .push("on <event>(arg): event must be `name` or `obj.name`".into());
                            return;
                        }
                    };
                    let event_ref = self.field_subs.get(&raw).cloned().unwrap_or(raw.clone());
                    let arg = match args.first() {
                        Some(CallArg::Expr(e)) => match &*e.kind {
                            ExprKind::Ident(id) => id.name.clone(),
                            _ => "_v".into(),
                        },
                        _ => "_v".into(),
                    };
                    let arg_ty = self
                        .event_types
                        .get(&raw)
                        .cloned()
                        .unwrap_or_else(|| "int64_t".into());
                    self.pad(depth);
                    // Same lambda-parameter binder as the component-body
                    // `on` handler above.
                    self.declared_value_names.insert(arg.clone());
                    writeln!(self.out, "{event_ref}.push_back([&]({arg_ty} {arg}) {{").ok();
                    self.emit_block_with_bindings(
                        &h.body,
                        depth + 1,
                        std::iter::once(arg.as_str()),
                    );
                    self.pad(depth);
                    writeln!(self.out, "}});").ok();
                } else {
                    // Cycle-trigger path. Edge mode comes from the parser
                    // (`rising` / `falling` / `level`). Default is rising
                    // — matches typical handshake protocols.
                    self.emit_cycle_trigger(h, depth, "_t_");
                }
            }
            StmtKind::Randomize {
                blocking,
                target,
                with_body,
            } => {
                let ty = match &*target.kind {
                    ExprKind::Ident(id) => self.let_types.get(&id.name).cloned(),
                    _ => None,
                };
                let ty = match ty {
                    Some(t) if self.is_record_type(&t) => t,
                    Some(t) => {
                        self.errors.push(format!(
                            "randomize(t): t has type `{t}` but no `transaction {t}` or `struct {t}` is declared in this file"
                        ));
                        return;
                    }
                    None => {
                        self.errors.push(
                            "randomize(t): could not resolve t's type — declare with `let t : <Transaction>`".into()
                        );
                        return;
                    }
                };
                // Spec §4: transaction-level `keep` constraints are
                // semantically part of every `randomize(t)` of that
                // type. Merge them with any user-supplied `with …`
                // body before handing to the solver. Without this
                // merge, `keep` would silently parse but emit zero
                // C++ — a correctness footgun (caught while auditing
                // §4 implementation status).
                let txn_keeps = self.txn_keeps.get(&ty).cloned().unwrap_or_default();
                let mut combined: Vec<Expr> = Vec::with_capacity(txn_keeps.len() + with_body.len());
                combined.extend(txn_keeps);
                combined.extend(with_body.iter().cloned());
                self.report_runtime_dependent_randomize_field_attrs(&ty);
                let has_solver_policy_fields = self.txn_fields.get(&ty).is_some_and(|fields| {
                    fields
                        .iter()
                        .any(|f| field_attr_unique(f) || !auto_coverage_values(f).is_empty())
                });
                if combined.is_empty() && !has_solver_policy_fields {
                    // No constraints anywhere: route the former direct PRNG
                    // call through the runtime shell. The shell currently
                    // delegates to `randomize_T`, preserving behavior while
                    // exercising the new boundary.
                    if !self.emit_runtime_unconstrained_randomize_call(&ty, target, depth) {
                        self.pad(depth);
                        write!(self.out, "randomize_{ty}(&").ok();
                        self.emit_expr(target);
                        writeln!(self.out, ", harc_rng);").ok();
                    }
                    self.emit_randomize_trace_event(&ty, target, depth);
                } else {
                    // Constraint-solving path via Z3.
                    self.emit_constraint_solver_block(&ty, target, &combined, *blocking, depth);
                }
            }
            other => {
                self.errors.push(format!(
                    "statement not supported in v0 cpp_tb: {:?}",
                    std::mem::discriminant(other)
                ));
            }
        }
    }
}
