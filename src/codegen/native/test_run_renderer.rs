//! Native C++ per-test run harness emission.

use super::*;

impl Emitter {
    pub(super) fn emit_test_run(
        &mut self,
        prep: &SuitePrep,
        test: &TestDecl,
    ) -> Result<(), EmitError> {
        let file = prep.file;
        let funcs = &prep.funcs;
        let tseqs = &prep.tseqs;
        let domains = &prep.domains;
        let dut_type = prep.dut_type;
        let component_order = &prep.component_order;
        let qualified_clock_wait = prep.qualified_clock_wait;
        let runtime_context = self.randomize_context_ref().to_string();
        // ── Reset Emitter per-test state ────────────────────────────
        self.let_types.clear();
        self.let_modes.clear();
        self.let_widths.clear();
        self.pointer_vars.clear();
        self.pointer_vars.insert("dut".to_string());
        self.bus_bindings.clear();
        self.bus_param_envs.clear();
        self.bus_remap.clear();
        self.pending_tlm_forks.clear();
        self.next_tlm_fork_tag.clear();
        self.probes.clear();
        self.probe_widths.clear();
        self.partial_probe_names = prep.partial_component_probe_names.clone();
        self.probe_dut_type = Some(dut_type.to_string());
        self.probe_receivers.clear();
        self.probe_receivers.insert("dut".to_string());
        self.probe_shadow_scopes.clear();
        self.shadowed_lets.clear();
        self.clock_names.clear();
        self.covers.clear();
        self.actor_threads.clear();
        self.field_subs.clear();
        self.driver_bus_for_hookables.clear();
        self.let_helper.clear();
        self.in_coroutine = false;
        self.current_yield_target = None;
        self.current_component_instance = None;

        // ── Derive per-test metadata ───────────────────────────────
        let custom_phases: Vec<(Ident, Block)> = test
            .items
            .iter()
            .filter_map(|it| match it {
                TestItem::Phase(name, body) => Some((name.clone(), body.clone())),
                _ => None,
            })
            .collect();
        let mut other_lets: Vec<&LetStmt> = Vec::new();
        let mut bare_stmts: Vec<&Stmt> = Vec::new();
        let mut explicit_run: Option<&Block> = None;
        let mut clocks: Vec<&ClockDecl> = Vec::new();
        for it in &test.items {
            match it {
                TestItem::Let(l) => {
                    if l.name.name == "dut" {
                        for p in &l.probes {
                            self.probes.insert(
                                p.name.name.clone(),
                                ProbeAccessor {
                                    read: crate::codegen::sv_stub::mangled_accessor(
                                        dut_type,
                                        &p.name.name,
                                    ),
                                    force: p.force,
                                },
                            );
                            // Mirrors TB-IR's `probe_scalar_width`: a
                            // `Bit`/`Bool` probe is one bit wide, not
                            // width-less. Omitting those made v1 reject a
                            // `dut.<bit probe> +% 1` that TB-IR wraps.
                            if let TypeExpr::Builtin { name, args, .. } = &p.ty {
                                let w = match name {
                                    BuiltinTy::Bit | BuiltinTy::Bool | BuiltinTy::BoolLower => {
                                        Some(1)
                                    }
                                    BuiltinTy::UInt | BuiltinTy::SInt | BuiltinTy::Bits => {
                                        type_arg_width(args)
                                    }
                                    _ => None,
                                };
                                if let Some(w) = w {
                                    self.probe_widths.insert(p.name.name.clone(), w);
                                }
                            }
                        }
                    } else {
                        other_lets.push(l);
                    }
                }
                TestItem::Scope(s) => {
                    if let Some(r) = &s.run {
                        explicit_run = Some(r);
                    }
                }
                TestItem::Stmt(s) => bare_stmts.push(s),
                TestItem::Clock(c) => clocks.push(c),
                _ => {}
            }
        }
        if explicit_run.is_none() && bare_stmts.is_empty() {
            return Err(EmitError(format!(
                "test `{}` has no body — add a `scope sim`, `run`, or bare statements",
                test.name.name,
            )));
        }
        let _ = (&explicit_run, &bare_stmts);

        // ── Seed Emitter per-test state ────────────────────────────
        for l in &other_lets {
            if let Some(s) = type_simple_name(l.ty.as_ref()) {
                self.let_types.insert(l.name.name.clone(), s.to_string());
            }
            if let Some(TypeExpr::Named { mode: Some(m), .. }) = l.ty.as_ref() {
                self.let_modes.insert(l.name.name.clone(), *m);
            }
            // Track bit-widths for uint<W> / sint<W> / bits<W> lets so
            // the width-method intrinsics (`.trunc<N>()` / `.sext<N>()`
            // etc.) can statically check that the requested width
            // direction matches the source width, and so sext can emit
            // its shift-fill from the right MSB position.
            if let Some(TypeExpr::Builtin { name, args, .. }) = l.ty.as_ref() {
                if matches!(name, BuiltinTy::UInt | BuiltinTy::SInt | BuiltinTy::Bits) {
                    if let Some(w) = type_arg_width(args) {
                        let lw = LetWidth {
                            bits: w,
                            signed: matches!(name, BuiltinTy::SInt),
                        };
                        if self.let_widths.insert(l.name.name.clone(), lw).is_some() {
                            self.shadowed_lets.insert(l.name.name.clone());
                        }
                    }
                }
            }
            if let Some(simple) = type_simple_name(l.ty.as_ref()) {
                if self.transactions.contains(simple)
                    || self.scoreboards.contains(simple)
                    || self.components.contains_key(simple)
                    || self.covergroups.contains_key(simple)
                    || self.buses.contains_key(simple)
                    || self.transactors.contains_key(simple)
                    || self.regblocks.contains_key(simple)
                    || self.addrmaps.contains_key(simple)
                {
                    continue;
                }
            }
            if matches!(&l.ty, Some(TypeExpr::Named { .. })) {
                self.pointer_vars.insert(l.name.name.clone());
            }
        }
        // Seed `let_types` for the bound testbench's fields (impl-for
        // form). After desugaring, a `testbench`-form test carries
        // `let _tb : <TbType>` and its fields (`drv`, `cov`, ...) live on
        // the `_tb` struct rather than as run-scope lets, so they never
        // hit the `other_lets` loop above. Covergroup HOOK-trigger
        // resolution (`covergroup G @(drv.method(t) post)`) resolves the
        // receiver field via `let_types`, so without this seed it cannot
        // find `drv` under the impl-for form (only the classic `test`
        // form, where the fields ARE run-scope lets, worked). Field names
        // are test-scoped; seeding them by their declared type lets the
        // hook resolver reach the transactor/component method table.
        if let Some(tb_ty) = self.let_types.get("_tb").cloned() {
            if let Some(tb_comp) = self.components.get(&tb_ty).cloned() {
                for ci in &tb_comp.items {
                    if let ComponentItem::Field(f) = ci {
                        if let Some(simple) = type_simple_name(Some(&f.ty)) {
                            self.let_types
                                .entry(f.name.name.clone())
                                .or_insert_with(|| simple.to_string());
                        }
                    }
                }
            }
        }
        self.clock_names = clocks.iter().map(|c| c.name.name.clone()).collect();

        writeln!(
            self.out,
            "int run_{}(int argc, char** argv) {{",
            test.name.name
        )
        .ok();
        if self.flavor == Flavor::Capsule {
            // Common layout uses a deep per-run owner.
            writeln!(self.out, "{INDENT}HarcSuiteRuntime {runtime_context};").ok();
        } else {
            writeln!(self.out, "{INDENT}HarcTestContext {runtime_context};").ok();
        }
        self.emit_capsule_state_decl(1);
        writeln!(
            self.out,
            "{INDENT}{runtime_context}.verilated.commandArgs(argc, argv);"
        )
        .ok();
        writeln!(
            self.out,
            "{INDENT}{runtime_context}.dut = harc_rt::log::harc_make_dut<V{dut_type}>(&{runtime_context}.verilated);"
        )
        .ok();
        writeln!(self.out, "{INDENT}auto* dut = {runtime_context}.dut;").ok();
        writeln!(self.out, "#if HARC_TRACE_ENABLED").ok();
        if self.flavor == Flavor::Legacy {
            // Legacy keeps the historical COPY: every consumer of `tfp` in
            // this layout is a `[&]` lambda in this same scope, so they all
            // see the local, and the byte stream must not change.
            writeln!(self.out, "{INDENT}auto* tfp = {runtime_context}.tfp;").ok();
        } else {
            // Common layout: the dump helpers are glue members reading
            // `ctx.tfp`. A copy here would leave that member null forever and
            // `harc_dump_wave_trace` silently no-ops on a null tracer — a
            // `--waves` run would emit a header-only VCD with no value
            // changes and still exit 0. Alias the runtime slot instead so the
            // `tfp = new HarcTraceC` below publishes it to the glue.
            writeln!(self.out, "{INDENT}auto& tfp = {runtime_context}.tfp;").ok();
        }
        writeln!(
            self.out,
            "{INDENT}auto& _wave_path = {runtime_context}._wave_path;"
        )
        .ok();
        writeln!(self.out, "#endif").ok();
        writeln!(
            self.out,
            "{INDENT}auto& _trace_time = {runtime_context}._trace_time;"
        )
        .ok();
        writeln!(self.out, "{INDENT}auto& _fatal = {runtime_context}._fatal;").ok();
        writeln!(
            self.out,
            "{INDENT}auto& cycle_count = {runtime_context}.cycle_count;"
        )
        .ok();
        writeln!(self.out, "{INDENT}auto& trace = {runtime_context}.trace;").ok();
        writeln!(
            self.out,
            "{INDENT}auto& log_ctx = {runtime_context}.log_ctx;"
        )
        .ok();
        writeln!(self.out, "{INDENT}auto& harc_rng = {runtime_context}.rng;").ok();
        writeln!(
            self.out,
            "{INDENT}auto harc_rng_next = [&]() {{ return harc_rng.next(); }};"
        )
        .ok();
        writeln!(
            self.out,
            "{INDENT}auto& _checkers = {runtime_context}._checkers;"
        )
        .ok();
        writeln!(
            self.out,
            "{INDENT}auto& _post_eval_services = {runtime_context}._post_eval_services;"
        )
        .ok();
        writeln!(
            self.out,
            "{INDENT}auto& _auto_cov_reports = {runtime_context}._auto_cov_reports;"
        )
        .ok();
        if self.flavor == Flavor::Capsule {
            // In the common layout the PRNG lives on the suite
            // runtime so two sequential runs in one process never share a
            // stream; the historical bare-name references keep compiling
            // through this alias. Hook registries likewise live on the
            // runtime — one registry per generated instance — and are bound
            // to their historical names below, so test-scope
            // `on obj.method pre/post` registrations compile unchanged.
            let hook_regs = collect_hook_registries(file, component_order);
            for reg in &hook_regs {
                writeln!(
                    self.out,
                    "{INDENT}auto& {v} = {runtime_context}.{v};",
                    v = reg.vector()
                )
                .ok();
            }
            if qualified_clock_wait {
                writeln!(
                    self.out,
                    "{INDENT}auto& _harc_advance_actors = {runtime_context}._harc_advance_actors;"
                )
                .ok();
                writeln!(
                    self.out,
                    "{INDENT}auto& _harc_actor_tick_due = {runtime_context}._harc_actor_tick_due;"
                )
                .ok();
            }
        } else if qualified_clock_wait {
            writeln!(
                self.out,
                "{INDENT}std::function<void()> _harc_advance_actors = []() {{}};"
            )
            .ok();
            writeln!(self.out, "{INDENT}bool _harc_actor_tick_due = false;").ok();
        }
        if qualified_clock_wait {
            writeln!(
                self.out,
                "{INDENT}harc_rt::ThreadSlot* _harc_running_slot = nullptr;"
            )
            .ok();
        }
        // Waveform tracer setup is compiled only when
        // `harc sim --waves` defined `HARC_TRACE_VCD` or
        // `HARC_TRACE_FST`. `HARC_WAVE_FILE` (set by `harc sim`)
        // selects the output path; `HARC_TRACE_DEPTH` selects the
        // hierarchy depth passed to `dut->trace()`. `_trace_time` is
        // a monotonically increasing dump cursor — its absolute
        // units do not matter, only that successive dump helper calls
        // receive strictly-increasing values so GTKWave /
        // surfer can order events.
        writeln!(self.out, "#if HARC_TRACE_ENABLED").ok();
        writeln!(self.out, "{INDENT}Verilated::traceEverOn(true);").ok();
        writeln!(self.out, "{INDENT}tfp = new HarcTraceC;").ok();
        writeln!(
        self.out,
        "{INDENT}_wave_path = harc_rt::log::harc_open_wave_trace(dut, tfp, harc_rt::log::harc_wave_default_name());"
    )
    .ok();
        writeln!(self.out, "#endif").ok();
        // Per spec §7.7: `log(fatal, ...)` aborts this test instance at
        // the end of the current cycle. The flag is checked by the
        // main simulation-loop guard below.
        writeln!(self.out, "").ok();
        // Seed PRNG from HARC_SEED env (or 1 if unset). Logged after sim_log_line
        // is defined so it lands in sim.log along with normal test output.
        writeln!(self.out, "{INDENT}harc_rng.seed_from_env();").ok();
        writeln!(
        self.out,
        "{INDENT}harc_rt::trace::harc_start_trace(trace, harc_rng.state, \"{dut_type}\", \"{}\", cycle_count);",
        test.name.name
    )
    .ok();
        if self.flavor == Flavor::Legacy {
            writeln!(
        self.out,
        "{INDENT}auto _harc_trace_dump_next = [&](const char* clock, uint64_t clock_cycle) {{"
    )
            .ok();
            writeln!(self.out, "{INDENT}{INDENT}uint64_t t = _trace_time++;").ok();
            writeln!(
                self.out,
                "{INDENT}{INDENT}trace.set_timing(t, clock, clock_cycle);"
            )
            .ok();
            writeln!(self.out, "{INDENT}{INDENT}HARC_RT_DUMP_WAVE_TRACE(tfp, t);").ok();
            writeln!(self.out, "{INDENT}}};").ok();
            writeln!(
        self.out,
        "{INDENT}auto _harc_trace_dump_at = [&](uint64_t t, const char* clock, uint64_t clock_cycle) {{"
    )
    .ok();
            writeln!(
                self.out,
                "{INDENT}{INDENT}trace.set_timing(t, clock, clock_cycle);"
            )
            .ok();
            writeln!(self.out, "{INDENT}{INDENT}HARC_RT_DUMP_WAVE_TRACE(tfp, t);").ok();
            writeln!(self.out, "{INDENT}}};").ok();
            writeln!(self.out, "").ok();
        }
        if self.flavor != Flavor::Legacy {
            // Dump-helper shims are emitted together with the tick binding
            // below (both live on the common-layout glue object).
            writeln!(self.out, "").ok();
        }
        // sim.log captures every log()/assert/fail line with cycle + severity
        // prefix. Path is configurable via the HARC_SIM_LOG env var (so the
        // outer harness can put it in the build dir); default `sim.log` in cwd.
        // Echo the active waveform path into sim.log so post-mortem
        // log inspection links to the matching VCD/FST without
        // grepping stderr. No-op in non-trace builds.
        writeln!(
            self.out,
            "{INDENT}HARC_RT_LOG_WAVE_FILE(log_ctx.sim_log, _wave_path);"
        )
        .ok();
        writeln!(self.out, "").ok();
        // Concurrent assertion hook — every `assert property <expr>` /
        // `assert property NAME` registers a closure here; tick() invokes the
        // whole list after each `eval()`. Same-cycle (`|->`) and one-cycle
        // (`|=>`) properties run on every primary-clock edge.
        writeln!(self.out, "").ok();

        let capsule = self.flavor == Flavor::Capsule;
        // Resolve every declared clock's half-period up front so both the
        // legacy inline scheduler and the common-layout glue configuration
        // see identical values (`5ns` literal or `domain` reference).
        let mut resolved_clocks: Vec<(String, i64)> = Vec::with_capacity(clocks.len());
        for c in &clocks {
            let period_ps = match &*c.period.kind {
            ExprKind::Time(s) => time_literal_to_ps(s).map_err(EmitError)?,
            ExprKind::Ident(id) => *domains.get(&id.name).ok_or_else(|| EmitError(format!(
                "clock {} references domain `{}` but no `domain {}` declaration was found in any input file",
                c.name.name, id.name, id.name
            )))?,
            _ => return Err(EmitError(format!(
                "clock {} period must be a time literal (e.g. 5ns) or a domain name (e.g. FastDomain)",
                c.name.name
            ))),
        };
            resolved_clocks.push((c.name.name.clone(), period_ps / 2));
        }

        if clocks.is_empty() {
            if capsule {
                // Common layout: tick is a glue member defined once in the
                // common TU; the capsule binds the historical name to it.
                // The glue also owns the dump helpers' bodies — bind those
                // names here too so drive-loop dumps compile unchanged.
                writeln!(self.out, "{INDENT}HarcSuiteGlue _glue({runtime_context});").ok();
                writeln!(self.out, "{INDENT}auto tick = [&]() {{ _glue.tick(); }};").ok();
                self.emit_capsule_dump_shims();
                writeln!(self.out, "").ok();
            } else {
                // Single-clock backward-compat path: drives `dut->clk` when the DUT
                // has that member (clocked modules). Purely combinational DUTs have no
                // `clk` port, so `_harc_eval_{negedge,posedge}` silently skip the
                // assignment via `if constexpr (requires { dut->clk; })`.
                // cycle_count increments once per tick.
                writeln!(self.out, "{INDENT}auto tick = [&]() {{").ok();
                writeln!(self.out, "{INDENT}{INDENT}_harc_eval_negedge(dut);").ok();
                writeln!(
                    self.out,
                    "{INDENT}{INDENT}_harc_trace_dump_next(\"clk\", (uint64_t)cycle_count);"
                )
                .ok();
                writeln!(self.out, "{INDENT}{INDENT}_harc_eval_posedge(dut);").ok();
                writeln!(
                    self.out,
                    "{INDENT}{INDENT}_harc_trace_dump_next(\"clk\", (uint64_t)(cycle_count + 1));"
                )
                .ok();
                writeln!(self.out, "{INDENT}{INDENT}cycle_count++;").ok();
                writeln!(
                    self.out,
                    "{INDENT}{INDENT}for (auto& _svc : _post_eval_services) _svc();"
                )
                .ok();
                writeln!(
                    self.out,
                    "{INDENT}{INDENT}if (!_post_eval_services.empty()) dut->eval();"
                )
                .ok();
                writeln!(
            self.out,
            "{INDENT}{INDENT}if (!_post_eval_services.empty()) _harc_trace_dump_next(\"clk\", (uint64_t)cycle_count);"
        )
        .ok();
                writeln!(self.out, "{INDENT}{INDENT}for (auto& _c : _checkers) _c();").ok();
                writeln!(self.out, "{INDENT}}};").ok();
                writeln!(self.out, "").ok();
            }
        } else {
            // Multi-clock scheduler: every declared clock keeps its own next-edge
            // timestamp; we advance simulation time to the earliest pending edge,
            // toggle that clock, call eval(). cycle_count tracks rising edges of
            // the primary clock (first-declared) so existing log lines remain
            // meaningful.
            if capsule {
                // Per-clock signal writes are capsule-provided setters (the
                // glue cannot name DUT members generically); the glue owns
                // the edge-advance state machine itself.
                writeln!(
                    self.out,
                    "{INDENT}std::vector<HarcClockCell> _harc_clock_cells;"
                )
                .ok();
                for (name, half) in &resolved_clocks {
                    writeln!(
                    self.out,
                    "{INDENT}_harc_clock_cells.push_back(HarcClockCell{{\"{name}\", {half}, {half}, 0, 0, [&](int l) {{ dut->{name} = l; }}}});",
                    name = name,
                    half = half
                )
                .ok();
                }
                writeln!(
                    self.out,
                    "{INDENT}HarcSuiteGlue _glue({runtime_context}, std::move(_harc_clock_cells));"
                )
                .ok();
                writeln!(self.out, "{INDENT}auto& now_ps = _glue.now_ps;").ok();
                writeln!(self.out, "{INDENT}auto& clocks_ = _glue.clocks_;").ok();
                writeln!(
                self.out,
                "{INDENT}auto eval_clocks_until = [&](long long t_ps) {{ _glue.eval_clocks_until(t_ps); }};"
            )
            .ok();
                writeln!(self.out, "{INDENT}auto tick = [&]() {{ _glue.tick(); }};").ok();
                self.emit_capsule_dump_shims();
                writeln!(self.out, "").ok();
            } else {
                writeln!(self.out, "{INDENT}long long now_ps = 0;").ok();
                writeln!(self.out, "{INDENT}struct ClockState {{ const char* name; long long half_period_ps; long long next_edge_ps; int level; long long rising_count; }};").ok();
                writeln!(self.out, "{INDENT}std::vector<ClockState> clocks_;").ok();
                for (name, half) in &resolved_clocks {
                    // First edge fires at half_period (rising) so initial state is 0.
                    writeln!(
                        self.out,
                        "{INDENT}clocks_.push_back(ClockState{{\"{}\", {half}, {half}, 0, 0}});",
                        name
                    )
                    .ok();
                    writeln!(self.out, "{INDENT}dut->{} = 0;", name).ok();
                }
                writeln!(self.out, "").ok();
                writeln!(
                    self.out,
                    "{INDENT}auto eval_clocks_until = [&](long long t_ps) {{"
                )
                .ok();
                writeln!(self.out, "{INDENT}{INDENT}while (now_ps < t_ps) {{").ok();
                writeln!(self.out, "{INDENT}{INDENT}{INDENT}long long next = t_ps;").ok();
                writeln!(self.out, "{INDENT}{INDENT}{INDENT}for (auto& c : clocks_) if (c.next_edge_ps < next) next = c.next_edge_ps;").ok();
                writeln!(self.out, "{INDENT}{INDENT}{INDENT}now_ps = next;").ok();
                writeln!(
                    self.out,
                    "{INDENT}{INDENT}{INDENT}bool _primary_rising = false;"
                )
                .ok();
                writeln!(
                    self.out,
                    "{INDENT}{INDENT}{INDENT}const char* _last_edge_clock = \"\";"
                )
                .ok();
                writeln!(
                    self.out,
                    "{INDENT}{INDENT}{INDENT}uint64_t _last_edge_cycle = 0;"
                )
                .ok();
                writeln!(
                    self.out,
                    "{INDENT}{INDENT}{INDENT}for (size_t i = 0; i < clocks_.size(); i++) {{"
                )
                .ok();
                writeln!(
                    self.out,
                    "{INDENT}{INDENT}{INDENT}{INDENT}auto& c = clocks_[i];"
                )
                .ok();
                writeln!(
                    self.out,
                    "{INDENT}{INDENT}{INDENT}{INDENT}if (c.next_edge_ps == now_ps) {{"
                )
                .ok();
                writeln!(
                    self.out,
                    "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}c.level = !c.level;"
                )
                .ok();
                // Per-clock signal write — done by name lookup.
                for (idx, c) in clocks.iter().enumerate() {
                    writeln!(
                self.out,
                "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}if (i == {idx}) dut->{} = c.level;",
                c.name.name
            )
                    .ok();
                }
                writeln!(
                    self.out,
                    "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}c.next_edge_ps += c.half_period_ps;"
                )
                .ok();
                // Per-clock rising-edge count (consumed by `wait N cycles on <clock>`).
                writeln!(
                    self.out,
                    "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}if (c.level == 1) c.rising_count++;"
                )
                .ok();
                writeln!(
                    self.out,
                    "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}_last_edge_clock = c.name;"
                )
                .ok();
                writeln!(
            self.out,
            "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}_last_edge_cycle = (uint64_t)c.rising_count;"
        )
                .ok();
                // Primary clock rising edge bumps cycle_count.
                writeln!(
        self.out,
        "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}if (i == 0 && c.level == 1) {{ cycle_count++; _primary_rising = true; }}"
    )
        .ok();
                writeln!(self.out, "{INDENT}{INDENT}{INDENT}{INDENT}}}").ok();
                writeln!(self.out, "{INDENT}{INDENT}{INDENT}}}").ok();
                writeln!(self.out, "{INDENT}{INDENT}{INDENT}dut->eval();").ok();
                // Set semantic trace timing for this edge *before* post_eval
                // services (so their trace events carry the right time), but
                // defer the waveform dump until after those services and the
                // follow-up eval settle. VCD allows only one dump per physical
                // timestamp, so we dump exactly once per `now_ps`.
                writeln!(
            self.out,
            "{INDENT}{INDENT}{INDENT}trace.set_timing((uint64_t)now_ps, _last_edge_clock, _last_edge_cycle);"
        )
        .ok();
                writeln!(
            self.out,
            "{INDENT}{INDENT}{INDENT}if (_primary_rising) {{ for (auto& _svc : _post_eval_services) _svc(); if (!_post_eval_services.empty()) dut->eval(); }}"
        )
        .ok();
                writeln!(
            self.out,
            "{INDENT}{INDENT}{INDENT}_harc_trace_dump_at((uint64_t)now_ps, _last_edge_clock, _last_edge_cycle);"
        )
        .ok();
                writeln!(self.out, "{INDENT}{INDENT}}}").ok();
                writeln!(self.out, "{INDENT}}};").ok();
                writeln!(self.out, "").ok();
                // `tick()` advances by one full primary clock period (one rising
                // edge). Other clocks tick at their natural rate during this span.
                writeln!(self.out, "{INDENT}auto tick = [&]() {{").ok();
                writeln!(
                    self.out,
                    "{INDENT}{INDENT}long long target = now_ps + clocks_[0].half_period_ps * 2;"
                )
                .ok();
                writeln!(self.out, "{INDENT}{INDENT}eval_clocks_until(target);").ok();
                writeln!(self.out, "{INDENT}{INDENT}for (auto& _c : _checkers) _c();").ok();
                writeln!(self.out, "{INDENT}}};").ok();
                writeln!(self.out, "").ok();
            }
        }
        // Variadic so log()/assert/fail callers can pass printf-style args
        // produced by `${expr}` string-interpolation lowering. The generated
        // lambdas keep the varargs ABI; runtime helpers own the sinks.
        // Per-file log handles, opened on first reference, closed at exit.
        // Relative paths are anchored to HARC_LOG_DIR by the runtime helper.
        writeln!(
            self.out,
            "{INDENT}auto sim_logf_line = [&](FILE* f, const char* sev, const char* fmt, ...) {{"
        )
        .ok();
        writeln!(
            self.out,
            "{INDENT}{INDENT}HARC_RT_LOG_FILE_ONLY_PRINTF(f, cycle_count, sev, fmt);"
        )
        .ok();
        writeln!(self.out, "{INDENT}}};").ok();
        writeln!(self.out, "").ok();
        // After sim_log_line below is defined, emit the seed line so it lands
        // in sim.log on every run — required for reproducing failures.
        let log_seed = true;

        writeln!(
            self.out,
            "{INDENT}auto sim_log_line = [&](const char* sev, const char* fmt, ...) {{"
        )
        .ok();
        writeln!(
            self.out,
            "{INDENT}{INDENT}HARC_RT_LOG_PRINTF(log_ctx.sim_log, &trace, cycle_count, sev, fmt);"
        )
        .ok();
        writeln!(self.out, "{INDENT}}};").ok();
        writeln!(self.out, "").ok();

        // Route an empty-queue pop through the sim's own FATAL path
        // instead of the runtime header's abort backstop: the run
        // records the failure, unwinds normally, and still writes its
        // log and trace. `_fatal` is a loop-exit condition, so the run
        // stops at the next scheduler tick — same semantics as
        // `log(fatal, ...)`. The TB-IR prologue
        // (`codegen/tbir/runtime.rs`) emits this verbatim too; the text
        // must stay identical or the v1-vs-TB-IR trace-diff diverges on
        // the FATAL line.
        writeln!(
            self.out,
            "{INDENT}harc_rt::HarcQueueFatalScope _queue_fatal_scope([&]() {{"
        )
        .ok();
        writeln!(
        self.out,
        "{INDENT}{INDENT}sim_log_line(\"FATAL\", \"queue front/pop on an empty queue -- guard it with .empty()/.size(), or wait until the producer has pushed\");"
    )
    .ok();
        writeln!(self.out, "{INDENT}{INDENT}{runtime_context}.errors++;").ok();
        writeln!(self.out, "{INDENT}{INDENT}_fatal = true;").ok();
        writeln!(self.out, "{INDENT}}});").ok();
        writeln!(self.out, "").ok();

        if log_seed {
            writeln!(
                self.out,
                "{INDENT}sim_log_line(\"INFO\", \"seed=%llu\", (long long)harc_rng.state);"
            )
            .ok();
            writeln!(self.out, "").ok();
        }

        // User-defined functions become lambdas. Emitted before the test body so
        // the body can call them. Capture-all (`[&]`) so they see `dut`/`tick`/
        // any test-level let bindings.
        //
        // Common layout: suite-level helpers are glue members defined once in
        // the common TU, so capsules skip the per-test lambda copies entirely
        // rather than emitted independently in every capsule.
        if self.flavor == Flavor::Legacy {
            for f in funcs.iter() {
                self.emit_function(f, 1);
            }
            if !funcs.is_empty() {
                writeln!(self.out, "").ok();
            }
            // Tseqs lower to lambdas returning `std::vector<T>`; emitted alongside
            // functions so the run-block can invoke them and consume the result.
            for t in tseqs.iter() {
                self.emit_tseq(t, 1);
            }
            if !tseqs.is_empty() {
                writeln!(self.out, "").ok();
            }
        }
        // Hookable methods on driver / agent / env / sequencer / scoreboard
        // become free `[&]`-capturing lambdas named `<Type>_<method>`. The
        // method-call site rewrites `obj.method(args)` to
        // `<Type>_<method>(obj, args)` so the body sees `tick` / `_checkers`
        // / etc. from the test scope.
        //
        // Pre/post hook vectors emit FIRST so the method bodies (and the
        // test-scope `on obj.method pre/post` registrations) can `[&]`-
        // capture them. Empty vectors are no-ops; the wrap is unconditional.
        let mut emitted_any_method = false;
        // Hook vectors emit in dependency order so that a method
        // body which calls into another transactor / component's
        // method finds the corresponding `<Type>_<method>_pre/_post`
        // vector already declared at its capture site. The vectors
        // themselves don't reference each other, but the method
        // lambdas below — which `[&]`-capture these vectors — do
        // call across component boundaries; co-locating hook
        // vectors and methods in the same order keeps the capture
        // graph acyclic. See `topo_sort_component_indices` (#301).
        for &idx in component_order.iter() {
            match &file.items[idx] {
                Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) | Item::Scoreboard(c) => {
                    for ci in &c.items {
                        if let ComponentItem::Hookable(h) = ci {
                            self.emit_hook_vectors(c, h, 1);
                        }
                    }
                }
                Item::Transactor(t) => {
                    let synth = synth_component_from_transactor(t, /*include_active*/ true);
                    for ci in &synth.items {
                        if let ComponentItem::Hookable(h) = ci {
                            self.emit_hook_vectors(&synth, h, 1);
                        }
                    }
                }
                _ => {}
            }
        }
        // Watchdog hook vectors must be emitted in the same forward-decl
        // pass as the hookable hook vectors — `on <Type>.watchdog pre/post`
        // captures them. The `emit_watchdog` helper below emits BOTH the
        // hook vectors AND the synthetic method body in one go, since the
        // method body refers to those vectors. So we forward-declare the
        // method via the same pass that emits hookable method lambdas
        // below; here, no separate forward decl is needed because the
        // method lambda + its hook vectors are emitted together at that
        // point.
        // Pre-scan: register test-scope bus bindings (`let axil :
        // BusAxiLite = bind dut`) and driver-type → binding mappings
        // BEFORE hookable methods emit. Hookables on `bound to BusType`
        // drivers need the binding active so `bus.<ch>.send/recv` and
        // `bus.<ch>.<sig>` resolve correctly. emit_let later re-registers
        // the bus bindings (idempotent — same key, same value).
        for it in &test.items {
            if let TestItem::Let(l) = it {
                if l.bind {
                    if let Some(simple) = type_simple_name(l.ty.as_ref()) {
                        if let Some(bus_decl) = self.buses.get(simple).cloned() {
                            if let Some(v) = &l.value {
                                let mut buf = String::new();
                                std::mem::swap(&mut self.out, &mut buf);
                                self.emit_expr(v);
                                std::mem::swap(&mut self.out, &mut buf);
                                let prefix = l.name.name.clone();
                                // Effective param env for `generate_if` gate
                                // evaluation: bus defaults overlaid with the
                                // bind-site generic overrides (`BusAxi4#(READ=0)`)
                                // and then the DUT port's own override
                                // (`port s: target BusAxi4<WRITE=0>`), which is
                                // authoritative for which gated channels
                                // `arch build` actually flattened. The bind name
                                // equals the DUT port name by convention.
                                let env = bus_param_env_with_port_override(
                                    &bus_decl,
                                    l.ty.as_ref(),
                                    self.dut_bus_port_overrides.get(&l.name.name),
                                );
                                self.bus_param_envs.insert(l.name.name.clone(), env);
                                self.bus_bindings
                                    .insert(l.name.name.clone(), (bus_decl, buf, prefix.clone()));
                                // Populate the per-bind signal remap so hookable-
                                // method emission (which precedes `emit_let`) can
                                // resolve `bus.<ch>.<sig>` against the override
                                // table. Without this pre-pass, transactor
                                // bodies use only the prefix-convention name and
                                // the override never fires for indirectly-routed
                                // accesses.
                                if !l.bind_remap.is_empty() {
                                    let mut map: std::collections::HashMap<
                                        (String, String),
                                        String,
                                    > = std::collections::HashMap::new();
                                    for entry in &l.bind_remap {
                                        if entry.path.len() == 2 {
                                            map.insert(
                                                (
                                                    entry.path[0].name.clone(),
                                                    entry.path[1].name.clone(),
                                                ),
                                                entry.port.clone(),
                                            );
                                        }
                                    }
                                    self.bus_remap.insert(prefix, map);
                                }
                            }
                        }
                    }
                }
            }
        }
        for it in &test.items {
            if let TestItem::Let(l) = it {
                if l.bind {
                    if let Some(simple) = type_simple_name(l.ty.as_ref()) {
                        let bound_decl_to_bus = self
                            .components
                            .get(simple)
                            .and_then(|c| c.bound_to.as_ref().map(|_| ()))
                            .is_some()
                            || self
                                .transactors
                                .get(simple)
                                .and_then(|t| t.bound_to.as_ref().map(|_| ()))
                                .is_some();
                        if bound_decl_to_bus {
                            if let Some(v) = &l.value {
                                if let ExprKind::Ident(rhs) = &*v.kind {
                                    if let Some(binding) = self.bus_bindings.get(&rhs.name).cloned()
                                    {
                                        // First binding wins; multi-instance is deferred.
                                        self.driver_bus_for_hookables
                                            .entry(simple.to_string())
                                            .or_insert(binding);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Hookable methods on regular components (agent / env /
        // sequencer / scoreboard) plus transactors. Transactor hookables
        // are emitted via the synth ComponentDecl so the existing
        // emit_component_method path finds the same struct shape.
        //
        // Emission order: dependency-sorted (`component_order` from
        // above) — a method body that calls `field.method(...)`
        // lowers to `<FieldType>_<method>(self.field, ...)`, so the
        // referenced lambda must be declared before the calling
        // lambda's `[&]` capture. Source-order emission breaks for
        // a transactor whose field type appears later in the file
        // so the topo sort guarantees the callee's
        // lambda is in scope at the caller's capture.
        // Under the common layout most bodies are glue members in the common
        // TU and are skipped here; the bus-bound ones are NOT glue members
        // and still emit locally, one copy per capsule, exactly as legacy
        // does (see `bus_bound_component_types`). `is_glue_member` is the
        // single predicate both this site and the call sites consult.
        for &idx in component_order.iter() {
            match &file.items[idx] {
                Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) | Item::Scoreboard(c) => {
                    for ci in &c.items {
                        if let ComponentItem::Hookable(h) = ci {
                            if self.is_glue_member(&format!("{}_{}", c.name.name, h.name.name)) {
                                continue;
                            }
                            self.emit_component_method(c, h, 1);
                            emitted_any_method = true;
                        }
                        if let ComponentItem::Watchdog(w) = ci {
                            if self.is_glue_member(&format!("{}_watchdog", c.name.name)) {
                                continue;
                            }
                            self.emit_watchdog(c, w, 1);
                            emitted_any_method = true;
                        }
                    }
                }
                Item::Transactor(t) => {
                    // include_active = true so any hookable inside `when
                    // active` is also emitted. Active-only hookables on a
                    // passive instance still compile but won't be invoked
                    // at runtime (no input event firing).
                    let synth = synth_component_from_transactor(t, /*include_active*/ true);
                    for ci in &synth.items {
                        if let ComponentItem::Hookable(h) = ci {
                            if self.is_glue_member(&format!("{}_{}", synth.name.name, h.name.name))
                            {
                                continue;
                            }
                            self.emit_component_method(&synth, h, 1);
                            emitted_any_method = true;
                        }
                        if let ComponentItem::Watchdog(w) = ci {
                            if self.is_glue_member(&format!("{}_watchdog", synth.name.name)) {
                                continue;
                            }
                            self.emit_watchdog(&synth, w, 1);
                            emitted_any_method = true;
                        }
                    }
                }
                _ => {}
            }
        }
        if emitted_any_method {
            writeln!(self.out, "").ok();
        }
        // The remainder is test-owned code and uses this test's own probe map.
        self.partial_probe_names.clear();

        // ── Scheduler — declared before hoisted lets so bound coroutine
        // drivers (Phase 2b) can register their slots inline at the
        // hoisted-let site. The run coroutine's slot is added below
        // alongside its body.
        writeln!(self.out, "{INDENT}harc_rt::ThreadScheduler sched;").ok();

        // Keep the surrounding C++ run scope visible while emitting phase
        // lambdas and the run coroutine. Test-level lets are hoisted into
        // that scope and therefore shadow same-named file-scope symbols in
        // every nested body.
        self.push_probe_shadow_scope();

        // Other lets get hoisted up front. Bound coroutine drivers also
        // emit their slot + per-driver transaction queue + actor
        // coroutine here (see the `let drv : Drv = bind axil` arm in
        // emit_let).
        for l in &other_lets {
            self.emit_let(l, 1);
            self.declare_probe_shadow(&l.name.name);
        }
        if qualified_clock_wait {
            writeln!(self.out, "{INDENT}_harc_advance_actors = [&]() {{").ok();
            if self.mt && !self.actor_threads.is_empty() {
                for (scheduler, _) in &self.actor_threads {
                    writeln!(self.out, "{INDENT}{INDENT}{scheduler}.tick();").ok();
                }
            } else {
                writeln!(
                    self.out,
                    "{INDENT}{INDENT}sched.tick_except(_harc_running_slot);"
                )
                .ok();
            }
            writeln!(self.out, "{INDENT}}};").ok();
        }

        // Custom `phase <name> ... end phase <name>` blocks from the test
        // lifecycle body (spec §7.2). Emitted after hoisted lets so
        // phase bodies can reference test-level env/agent/scoreboard
        // instances. Calls of the form `<name>()` from inside `run` lower as
        // plain C++ function calls; `wait` inside the phase takes the sync
        // `tick()` path because the lambda body emits with `in_coroutine =
        // false`.
        if !custom_phases.is_empty() {
            for (name, body) in &custom_phases {
                self.pad(1);
                writeln!(self.out, "auto {} = [&]() -> void {{", name.name).ok();
                self.emit_block(body, 2);
                self.pad(1);
                writeln!(self.out, "}};").ok();
                self.declare_probe_shadow(&name.name);
            }
            writeln!(self.out, "").ok();
        }

        // ── Coroutine wrap for the test body ───────────────────────────────
        //
        // The whole test body (bare stmts + scope sim/{setup,run,check,
        // teardown}) becomes a single C++20 coroutine driven by
        // `harc_rt::ThreadScheduler`. Setting `in_coroutine = true` flips
        // the wait/tick lowering inside this scope to emit
        // `co_await harc_rt::wait_cycles(_slot, N)` instead of the
        // synchronous `for (...) tick();` form.
        //
        // The lambda captures by reference (`[&]`) so it sees `dut`,
        // `tick`, `cycle_count`, `_checkers`, hookable-method lambdas,
        // and any hoisted lets defined above in `main`'s scope.
        //
        // After the coroutine is constructed, `sched.bootstrap()` resumes
        // every initially-Ready slot once (the run setup statements,
        // plus any bound-driver actors' first wait_until on their queue).
        // The main loop then drives the clock until the run coroutine is
        // `Done` — driver coroutines may still be parked in WaitUntil
        // (queue empty) at that point; teardown explicitly destroys those
        // frames before the DUT and their captured state disappear.
        writeln!(self.out, "{INDENT}harc_rt::ThreadSlot _run_slot;").ok();
        if qualified_clock_wait {
            writeln!(self.out, "{INDENT}_harc_running_slot = &_run_slot;").ok();
        }
        writeln!(self.out, "{INDENT}sched.slots.push_back(&_run_slot);").ok();
        writeln!(
        self.out,
        "{INDENT}auto _run_slot_lambda = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{"
    )
        .ok();

        self.in_coroutine = true;
        for (item_index, it) in test.items.iter().enumerate() {
            let prior_source_id = self.current_source_id;
            let source_id = test.item_source(item_index);
            if source_id.is_known() {
                self.current_source_id = source_id;
            }
            match it {
                TestItem::Stmt(s) => {
                    self.emit_stmt(s, 2);
                    if let StmtKind::Let(let_stmt) = &s.kind {
                        self.declare_probe_shadow(&let_stmt.name.name);
                    }
                }
                TestItem::Scope(s) => {
                    if let Some(b) = &s.setup {
                        self.emit_block(b, 2);
                    }
                    if let Some(b) = &s.run {
                        self.emit_block(b, 2);
                    }
                    if let Some(b) = &s.check {
                        self.emit_block(b, 2);
                    }
                    if let Some(b) = &s.teardown {
                        self.emit_block(b, 2);
                    }
                }
                _ => {}
            }
            self.current_source_id = prior_source_id;
        }
        self.in_coroutine = false;

        writeln!(self.out, "{INDENT}{INDENT}co_return;").ok();
        writeln!(self.out, "{INDENT}}};").ok();
        writeln!(
            self.out,
            "{INDENT}_run_slot.thread = _run_slot_lambda(&_run_slot);"
        )
        .ok();
        writeln!(self.out, "").ok();

        // `actor_threads` is populated only when `--mt` is set (cooperative
        // mode pushes actor slots into the global `sched` instead). So
        // `mt` here means "we have per-actor schedulers needing barrier
        // sync"; cooperative mode skips the worker spawn / barrier dance
        // entirely even when actors are present.
        let n_actors = self.actor_threads.len();
        let mt = n_actors > 0;
        debug_assert!(mt == (self.mt && !self.actor_threads.is_empty()));
        let _ = self.mt; // suppress unused warning when no actors

        // ── Bootstrap ──────────────────────────────────────────────────────
        // Single-threaded — workers haven't started yet. Each scheduler
        // (main + per-actor) runs its initially-Ready slots once until
        // they hit their first co_await.
        writeln!(
            self.out,
            "{INDENT}// Resume each coroutine once so initial-setup statements run"
        )
        .ok();
        writeln!(
            self.out,
            "{INDENT}// before the first clock edge. Single-threaded — workers"
        )
        .ok();
        writeln!(self.out, "{INDENT}// haven't been spawned yet.").ok();
        writeln!(self.out, "{INDENT}sched.bootstrap();").ok();
        if mt {
            for (sched_var, _) in &self.actor_threads {
                writeln!(self.out, "{INDENT}{sched_var}.bootstrap();").ok();
            }
        }
        writeln!(self.out, "").ok();

        if mt {
            // ── Phase 3a multi-thread topology ──────────────────────────
            // Each actor coroutine runs on its own `std::thread`. Per
            // posedge: main runs the run-coroutine, two atomic-spin
            // barriers synchronize main with N worker threads, each
            // worker runs `_<n>_sched.tick()` once. Then main does
            // `dut->eval()` (single-threaded — Verilator-generated DUT
            // code is not MT-safe) and runs `_checkers`. The dual
            // barrier mirrors arch-com's Phase 3 design (`Barrier` class
            // shared in `harc_thread_rt.h`); cycle batching to amortize
            // barrier cost is Phase 3b.
            writeln!(
                self.out,
                "{INDENT}// Phase 3a: per-actor OS threads with dual barrier sync."
            )
            .ok();
            writeln!(
                self.out,
                "{INDENT}// {} actor(s) → {} barrier participants (main + workers).",
                n_actors,
                n_actors + 1
            )
            .ok();
            writeln!(self.out, "{INDENT}std::atomic<bool> _shutdown{{false}};").ok();
            writeln!(
                self.out,
                "{INDENT}harc_rt::Barrier _start_barrier({});",
                n_actors + 1
            )
            .ok();
            writeln!(
                self.out,
                "{INDENT}harc_rt::Barrier _end_barrier({});",
                n_actors + 1
            )
            .ok();
            writeln!(self.out, "{INDENT}std::vector<std::thread> _workers;").ok();
            for (sched_var, _) in &self.actor_threads {
                writeln!(self.out, "{INDENT}_workers.emplace_back([&]() {{").ok();
                writeln!(self.out, "{INDENT}{INDENT}while (true) {{").ok();
                writeln!(self.out, "{INDENT}{INDENT}{INDENT}_start_barrier.wait();").ok();
                writeln!(
                    self.out,
                    "{INDENT}{INDENT}{INDENT}if (_shutdown.load(std::memory_order_acquire)) break;"
                )
                .ok();
                writeln!(self.out, "{INDENT}{INDENT}{INDENT}{sched_var}.tick();").ok();
                writeln!(self.out, "{INDENT}{INDENT}{INDENT}_end_barrier.wait();").ok();
                writeln!(self.out, "{INDENT}{INDENT}}}").ok();
                writeln!(self.out, "{INDENT}}});").ok();
            }
            if qualified_clock_wait {
                writeln!(self.out, "{INDENT}_harc_advance_actors = [&]() {{").ok();
                writeln!(self.out, "{INDENT}{INDENT}_start_barrier.wait();").ok();
                writeln!(self.out, "{INDENT}{INDENT}_end_barrier.wait();").ok();
                writeln!(self.out, "{INDENT}}};").ok();
                writeln!(self.out, "").ok();
            }
        }

        writeln!(
            self.out,
            "{INDENT}// Drive the clock until the run coroutine completes."
        )
        .ok();
        writeln!(self.out, "{INDENT}//").ok();
        writeln!(
            self.out,
            "{INDENT}// `wait N cycles` matches Verilog's `@(posedge clk)` semantic:"
        )
        .ok();
        writeln!(
            self.out,
            "{INDENT}// values set in the segment BEFORE the wait are sampled at the"
        )
        .ok();
        writeln!(self.out, "{INDENT}// next posedge. Per loop iteration:").ok();
        writeln!(
            self.out,
            "{INDENT}//   1. Posedge (clk 0→1, eval) — DUT FFs latch the current input"
        )
        .ok();
        writeln!(
            self.out,
            "{INDENT}//      values (set in the previous segment, or in bootstrap on"
        )
        .ok();
        writeln!(self.out, "{INDENT}//      the first iteration).").ok();
        writeln!(
            self.out,
            "{INDENT}//   2. `sched.tick()` — advance the run coroutine to its next"
        )
        .ok();
        writeln!(
            self.out,
            "{INDENT}//      wait, setting the inputs for the NEXT cycle's posedge."
        )
        .ok();
        writeln!(
            self.out,
            "{INDENT}//   3. Falling edge (clk 1→0, eval) — comb re-settles with the"
        )
        .ok();
        writeln!(self.out, "{INDENT}//      newly-set inputs.").ok();
        writeln!(self.out, "{INDENT}//   4. Cycle counter + checkers.").ok();
        writeln!(
            self.out,
            "{INDENT}// One initial `eval(clk=0)` before the loop settles combinational"
        )
        .ok();
        writeln!(
            self.out,
            "{INDENT}// logic with the bootstrap inputs — same role as `initial`-block"
        )
        .ok();
        writeln!(
            self.out,
            "{INDENT}// settle in Verilog. Each `wait 1 cycle` then maps to exactly"
        )
        .ok();
        writeln!(
            self.out,
            "{INDENT}// one posedge that observes the just-set values."
        )
        .ok();
        if mt {
            writeln!(self.out, "{INDENT}//").ok();
            writeln!(
                self.out,
                "{INDENT}// MT mode: workers run between tick() and the falling edge,"
            )
            .ok();
            writeln!(
                self.out,
                "{INDENT}// gated by _start_barrier / _end_barrier. Run-coroutine writes"
            )
            .ok();
            writeln!(
                self.out,
                "{INDENT}// complete BEFORE workers wake → no race on shared queues."
            )
            .ok();
            writeln!(
                self.out,
                "{INDENT}// Workers' DUT-input writes complete BEFORE the falling-edge"
            )
            .ok();
            writeln!(self.out, "{INDENT}// eval → no race on signal state.").ok();
        }
        if clocks.is_empty() {
            // Initial comb settle — bootstrap's inputs propagate through
            // combinational logic before the first posedge.
            writeln!(self.out, "{INDENT}_harc_eval_negedge(dut);").ok();
            writeln!(
                self.out,
                "{INDENT}_harc_trace_dump_next(\"clk\", (uint64_t)cycle_count);"
            )
            .ok();
            writeln!(
                self.out,
                "{INDENT}while (_run_slot.kind != harc_rt::WaitKind::Done && !_fatal) {{"
            )
            .ok();
            // Posedge first — latches current input values.
            writeln!(self.out, "{INDENT}{INDENT}_harc_eval_posedge(dut);").ok();
            writeln!(
                self.out,
                "{INDENT}{INDENT}_harc_trace_dump_next(\"clk\", (uint64_t)(cycle_count + 1));"
            )
            .ok();
            writeln!(self.out, "{INDENT}{INDENT}cycle_count++;").ok();
            writeln!(
                self.out,
                "{INDENT}{INDENT}for (auto& _svc : _post_eval_services) _svc();"
            )
            .ok();
            writeln!(
                self.out,
                "{INDENT}{INDENT}if (!_post_eval_services.empty()) dut->eval();"
            )
            .ok();
            writeln!(
            self.out,
            "{INDENT}{INDENT}if (!_post_eval_services.empty()) _harc_trace_dump_next(\"clk\", (uint64_t)cycle_count);"
        )
        .ok();
            // Then advance the run coroutine for the next cycle's inputs.
            writeln!(self.out, "{INDENT}{INDENT}sched.tick();").ok();
            if mt {
                writeln!(self.out, "{INDENT}{INDENT}_start_barrier.wait();").ok();
                writeln!(self.out, "{INDENT}{INDENT}_end_barrier.wait();").ok();
            }
            // Falling edge + comb resettle with the new inputs.
            writeln!(self.out, "{INDENT}{INDENT}_harc_eval_negedge(dut);").ok();
            writeln!(
                self.out,
                "{INDENT}{INDENT}_harc_trace_dump_next(\"clk\", (uint64_t)cycle_count);"
            )
            .ok();
            writeln!(self.out, "{INDENT}{INDENT}for (auto& _c : _checkers) _c();").ok();
            writeln!(self.out, "{INDENT}}}").ok();
        } else {
            // Multi-clock: initial bare eval() to settle combinational
            // logic with bootstrap inputs (no clock advancement). The
            // loop's eval_clocks_until then advances time by one full
            // primary-clock period per iteration. Posedge-vs-tick ordering
            // is constrained by eval_clocks_until's atomic per-edge eval
            // — we tick AFTER eval_clocks_until so the run coroutine
            // observes the just-completed cycle's outputs and sets the
            // next cycle's inputs in time for the following iteration's
            // first edge. Same effect as the single-clock branch, just
            // with the clock toggling factored into eval_clocks_until.
            writeln!(self.out, "{INDENT}dut->eval();").ok();
            writeln!(
                self.out,
                "{INDENT}_harc_trace_dump_at((uint64_t)now_ps, \"\", 0);"
            )
            .ok();
            writeln!(
                self.out,
                "{INDENT}while (_run_slot.kind != harc_rt::WaitKind::Done && !_fatal) {{"
            )
            .ok();
            writeln!(
                self.out,
                "{INDENT}{INDENT}long long _target = now_ps + clocks_[0].half_period_ps * 2;"
            )
            .ok();
            writeln!(self.out, "{INDENT}{INDENT}eval_clocks_until(_target);").ok();
            if mt && qualified_clock_wait {
                writeln!(self.out, "{INDENT}{INDENT}_harc_actor_tick_due = true;").ok();
            }
            writeln!(self.out, "{INDENT}{INDENT}sched.tick();").ok();
            if mt && qualified_clock_wait {
                writeln!(self.out, "{INDENT}{INDENT}if (_harc_actor_tick_due) {{ _harc_advance_actors(); _harc_actor_tick_due = false; }}").ok();
            } else if mt {
                writeln!(self.out, "{INDENT}{INDENT}_start_barrier.wait();").ok();
                writeln!(self.out, "{INDENT}{INDENT}_end_barrier.wait();").ok();
            }
            writeln!(self.out, "{INDENT}{INDENT}for (auto& _c : _checkers) _c();").ok();
            writeln!(self.out, "{INDENT}}}").ok();
        }

        if mt {
            // Shutdown sequence: workers are blocked on _start_barrier
            // (their next iteration). Set _shutdown, wake them via the
            // start barrier; they observe the flag and break out of their
            // loop without reaching _end_barrier. Then join.
            writeln!(self.out, "").ok();
            writeln!(
                self.out,
                "{INDENT}_shutdown.store(true, std::memory_order_release);"
            )
            .ok();
            writeln!(self.out, "{INDENT}_start_barrier.wait();").ok();
            writeln!(self.out, "{INDENT}for (auto& _w : _workers) _w.join();").ok();
        }
        writeln!(self.out, "").ok();

        // Final + return.
        writeln!(self.out, "").ok();
        writeln!(self.out, "{INDENT}for (auto& _r : _auto_cov_reports) _r();").ok();
        // Property-cover summary (ARCH-style: header line + per-point lines
        // with `*NOT HIT*` marker; stdout destination — see covergroup
        // report() for the rationale on stdout vs stderr).
        if !self.covers.is_empty() {
            let n_covers = self.covers.len();
            writeln!(self.out, "{INDENT}{{").ok();
            writeln!(
                self.out,
                "{INDENT}{INDENT}uint64_t _cov_total = {n_covers};"
            )
            .ok();
            writeln!(self.out, "{INDENT}{INDENT}uint64_t _cov_hit = 0;").ok();
            let covers_clone = self.covers.clone();
            let cover_cells: Vec<String> = covers_clone
                .iter()
                .map(|c| {
                    // Lookup only — the statement registered these cells
                    // during the planning/render pass.
                    format!("{runtime_context}._cells._cov_{}_hits", c.tag)
                })
                .collect();
            for cell in &cover_cells {
                writeln!(self.out, "{INDENT}{INDENT}if ({cell} > 0) _cov_hit++;",).ok();
            }
            writeln!(
                self.out,
                "{INDENT}{INDENT}harc_rt::log::harc_print_cover_summary(_cov_hit, _cov_total);"
            )
            .ok();
            writeln!(
                self.out,
                "{INDENT}{INDENT}harc_rt::log::harc_cov_json_cover_summary(log_ctx.coverage_json, _cov_hit, _cov_total);"
            )
            .ok();
            for (c, cell) in covers_clone.iter().zip(cover_cells.iter()) {
                writeln!(
                    self.out,
                    "{INDENT}{INDENT}harc_rt::log::harc_print_cover_point(\"{label}\", {cell});",
                    label = escape_c(&c.label)
                )
                .ok();
                writeln!(
                    self.out,
                    "{INDENT}{INDENT}harc_rt::log::harc_cov_json_cover_point(log_ctx.coverage_json, \"{label}\", {cell});",
                    label = escape_c(&c.label)
                )
                .ok();
            }
            writeln!(self.out, "{INDENT}}}").ok();
        }
        writeln!(self.out, "{INDENT}_auto_cov_reports.clear();").ok();
        writeln!(self.out, "{INDENT}_post_eval_services.clear();").ok();
        writeln!(self.out, "{INDENT}_checkers.clear();").ok();
        writeln!(
            self.out,
            "{INDENT}harc_rt::harc_destroy_scheduler_threads(sched);"
        )
        .ok();
        for (scheduler, _) in &self.actor_threads {
            writeln!(
                self.out,
                "{INDENT}harc_rt::harc_destroy_scheduler_threads({scheduler});"
            )
            .ok();
        }
        writeln!(self.out, "{INDENT}dut->final();").ok();
        // Verilator coverage write — the runtime macro is a no-op unless the
        // TB was built with `harc sim --coverage` (which sets `--coverage` on
        // verilator → defines `VM_COVERAGE=1`).
        writeln!(
            self.out,
            "{INDENT}HARC_RT_WRITE_COVERAGE({runtime_context}.verilated.coveragep());"
        )
        .ok();
        // Waveform tracer teardown must precede
        // `delete dut` because `tfp->close()` writes the
        // end-of-trace marker via the trace dispatcher held by the
        // DUT root. Skipped via the same compile-time gate as
        // tracer construction.
        writeln!(self.out, "{INDENT}HARC_RT_CLOSE_WAVE_TRACE(tfp);").ok();
        writeln!(self.out, "{INDENT}delete dut;").ok();
        writeln!(self.out, "").ok();
        writeln!(
        self.out,
        "{INDENT}return harc_rt::log::harc_finish_sim_run(log_ctx, trace, cycle_count, {runtime_context}.errors);"
    )
        .ok();
        writeln!(self.out, "}}").ok();
        writeln!(self.out, "").ok();
        self.pop_probe_shadow_scope();
        Ok(())
    }
}
