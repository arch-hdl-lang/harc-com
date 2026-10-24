//! Native C++ assertion, wait, property, and cycle-trigger emission.

use super::*;

impl Emitter {
    /// Emit `assert <expr> [else fail("msg")]` as an immediate, point-in-
    /// time check (existing behaviour). Compare with `emit_property_check`
    /// which schedules the check on every primary-clock edge.
    pub(super) fn emit_inline_assert(&mut self, v: &Verify, depth: usize) {
        self.pad(depth);
        let expr = v.expr.as_ref().expect("assert without expr");
        write!(self.out, "if (!(").ok();
        self.emit_expr(expr);
        writeln!(self.out, ")) {{").ok();
        self.pad(depth + 1);
        let msg = v
            .else_fail
            .as_ref()
            .and_then(|e| match &*e.kind {
                ExprKind::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "assertion failed".to_string());
        let (fmt, caps) = process_interp(&msg);
        // C++ does not guarantee a source-order evaluation for variadic
        // function arguments. Snapshot captures before the log call so
        // interpolation side effects happen left-to-right.
        self.pad(depth + 1);
        writeln!(self.out, "{{").ok();
        self.emit_interp_capture_temps(&caps, depth + 2);
        self.pad(depth + 2);
        write!(self.out, "sim_log_line(\"FAIL\", \"{}\"", escape_c(&fmt)).ok();
        for (index, c) in caps.iter().enumerate() {
            self.emit_interp_temp_arg(c, index);
        }
        writeln!(self.out, ");").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        self.pad(depth + 1);
        let runtime_context = self.randomize_context_ref().to_string();
        writeln!(self.out, "{runtime_context}.errors++;").ok();
        self.pad(depth);
        writeln!(self.out, "}}").ok();
    }

    /// Emit a `wait until …` statement (spec §7.9). Four shape axes:
    ///
    /// - **Single / AllOf / AnyOf** — the predicate is one expression
    ///   or the conjunction / disjunction of multiple sub-expressions.
    /// - **With or without `timeout`** — without, the loop is an
    ///   unbounded `co_await wait_until(_slot, [&]{ … })` (or sync
    ///   `while (!cond) tick();`). With timeout, a bounded polling
    ///   loop measured against `cycle_count` plus per-predicate
    ///   diagnostic logging on expiration.
    ///
    /// Diagnostic on `all of` timeout: lists each sub-predicate still
    /// false, identified by its pretty-printed source text (so the
    /// log mentions `env.agent.idle(100)` rather than a synthetic
    /// index). For `any of`, the breakdown lists every sub-predicate
    /// since none became true.
    pub(super) fn emit_wait_until(
        &mut self,
        mode: &WaitUntilMode,
        conditions: &[Expr],
        timeout: Option<&WaitTimeout>,
        depth: usize,
    ) {
        if conditions.is_empty() {
            self.errors
                .push("wait until: at least one condition required".into());
            return;
        }
        let expanded_conditions = self.expand_wait_conditions(conditions);
        // Combine conditions into one predicate expression in C++.
        // `Single` uses the bare expression; `AllOf` / `AnyOf` are
        // emitted as parenthesized `&&` / `||` chains.
        let joiner = match mode {
            WaitUntilMode::Single | WaitUntilMode::AllOf => "&&",
            WaitUntilMode::AnyOf => "||",
        };
        let emit_overall_cond = |this: &mut Self| {
            if expanded_conditions.len() == 1 {
                expanded_conditions[0].emit(this);
                return;
            }
            for (i, c) in expanded_conditions.iter().enumerate() {
                if i > 0 {
                    write!(this.out, " {joiner} ").ok();
                }
                write!(this.out, "(").ok();
                c.emit(this);
                write!(this.out, ")").ok();
            }
        };

        match timeout {
            None => {
                // Untimed wait. Coroutine context: yield to the
                // scheduler; sync context: a synchronous polling loop.
                self.pad(depth);
                if self.in_coroutine {
                    write!(
                        self.out,
                        "co_await harc_rt::wait_until(_slot, [&]{{ return "
                    )
                    .ok();
                    emit_overall_cond(self);
                    writeln!(self.out, "; }});").ok();
                } else {
                    write!(self.out, "while (!(").ok();
                    emit_overall_cond(self);
                    writeln!(self.out, ")) tick();").ok();
                }
            }
            Some(to) => {
                // Timed wait — open a brace block so the budget /
                // start variables don't leak.
                //
                // Coroutine context uses the runtime's
                // `wait_until_timeout` awaiter (one scheduler
                // round-trip): the scheduler evaluates the predicate
                // each cycle AND decrements a per-slot countdown,
                // resuming the coroutine when EITHER pred fires OR
                // the budget hits 0. The awaiter returns `true` for
                // pred-fired, `false` for timed-out — drives the
                // diagnostic emission below.
                //
                // Synchronous context (hookable body, etc.) has no
                // coroutine to suspend, so it stays on the explicit
                // polling loop (the only mechanism available there).
                self.pad(depth);
                writeln!(self.out, "{{").ok();
                self.pad(depth + 1);
                write!(self.out, "int64_t _wu_budget = (int64_t)(").ok();
                self.emit_expr(&to.cycles);
                writeln!(self.out, ");").ok();
                if self.in_coroutine {
                    // Single co_await — runtime handles the wait + countdown.
                    self.pad(depth + 1);
                    write!(self.out, "bool _wu_satisfied = co_await harc_rt::wait_until_timeout(_slot, [&]{{ return ").ok();
                    emit_overall_cond(self);
                    writeln!(self.out, "; }}, (uint32_t)_wu_budget);").ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "if (!_wu_satisfied) {{").ok();
                } else {
                    // Sync context: keep the explicit polling loop —
                    // no scheduler to defer the wait to.
                    self.pad(depth + 1);
                    writeln!(self.out, "int64_t _wu_start = (int64_t)cycle_count;").ok();
                    self.pad(depth + 1);
                    write!(self.out, "while (!(").ok();
                    emit_overall_cond(self);
                    writeln!(
                        self.out,
                        ") && ((int64_t)cycle_count - _wu_start) < _wu_budget) {{"
                    )
                    .ok();
                    self.pad(depth + 2);
                    writeln!(self.out, "tick();").ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "}}").ok();
                    // Final check: re-evaluate the predicate now that
                    // the loop has ended (either pred fired or budget
                    // expired).
                    self.pad(depth + 1);
                    write!(self.out, "if (!(").ok();
                    emit_overall_cond(self);
                    writeln!(self.out, ")) {{").ok();
                }
                // Header line — user-supplied message or default.
                let header = to.message.as_ref().and_then(|e| match &*e.kind {
                    ExprKind::String(s) => Some(s.clone()),
                    _ => None,
                });
                self.pad(depth + 2);
                if let Some(raw) = header {
                    let (fmt, caps) = process_interp(&raw);
                    writeln!(self.out, "{{").ok();
                    self.emit_interp_capture_temps(&caps, depth + 3);
                    self.pad(depth + 3);
                    write!(self.out, "sim_log_line(\"FAIL\", \"{}\"", escape_c(&fmt)).ok();
                    for (index, c) in caps.iter().enumerate() {
                        self.emit_interp_temp_arg(c, index);
                    }
                    writeln!(self.out, ");").ok();
                    self.pad(depth + 2);
                    writeln!(self.out, "}}").ok();
                } else {
                    let label = match mode {
                        WaitUntilMode::Single => "wait until",
                        WaitUntilMode::AllOf => "wait until all of",
                        WaitUntilMode::AnyOf => "wait until any of",
                    };
                    writeln!(self.out,
                        "sim_log_line(\"FAIL\", \"{label} timed out after %lld cycles\", (long long)_wu_budget);"
                    ).ok();
                }
                // Per-sub-predicate breakdown.
                match mode {
                    WaitUntilMode::Single | WaitUntilMode::AllOf => {
                        for c in &expanded_conditions {
                            let escaped = escape_c(&c.label());
                            self.pad(depth + 2);
                            write!(self.out, "if (!(").ok();
                            c.emit(self);
                            writeln!(
                                self.out,
                                ")) sim_log_line(\"FAIL\", \"  not yet true: {escaped}\");"
                            )
                            .ok();
                        }
                    }
                    WaitUntilMode::AnyOf => {
                        // None became true — list everything that was
                        // being waited on so the user can spot the
                        // expected-firing condition that never fired.
                        let mut joined = String::new();
                        for (i, c) in expanded_conditions.iter().enumerate() {
                            if i > 0 {
                                joined.push_str(", ");
                            }
                            joined.push_str(&c.label());
                        }
                        let escaped = escape_c(&joined);
                        self.pad(depth + 2);
                        writeln!(
                            self.out,
                            "sim_log_line(\"FAIL\", \"  none of: {escaped}\");"
                        )
                        .ok();
                    }
                }
                self.pad(depth + 2);
                let runtime_context = self.randomize_context_ref().to_string();
                writeln!(self.out, "{runtime_context}.errors++;").ok();
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
        }
    }

    /// `assume <expr>` (immediate form) — same as inline assert but logged
    /// as ASSUME so the user can grep separately. No `errors++` because in
    /// sim, assumes are warnings (spec §5.2).
    pub(super) fn emit_inline_assume(&mut self, v: &Verify, depth: usize) {
        self.pad(depth);
        let expr = v.expr.as_ref().expect("assume without expr");
        write!(self.out, "if (!(").ok();
        self.emit_expr(expr);
        writeln!(
            self.out,
            ")) sim_log_line(\"ASSUME\", \"assumption failed\");"
        )
        .ok();
    }

    /// Register a concurrent (every-primary-clock-edge) property check.
    /// Resolves a bare `Ident` to a declared property body; otherwise uses
    /// the inline expression directly. Translates the top-level temporal
    /// shape (`a |-> b`, `a |=> b`, or a plain bool) to a stateful closure
    /// pushed into `_checkers`.
    pub(super) fn emit_property_check(&mut self, severity: &str, v: &Verify, depth: usize) {
        let raw = v.expr.as_ref().expect("assert property without body");
        let body: Expr = match &*raw.kind {
            ExprKind::Ident(id) => match self.properties.get(&id.name).cloned() {
                Some(b) => b,
                None => {
                    self.errors.push(format!(
                        "assert property `{}`: no property declaration with that name",
                        id.name
                    ));
                    return;
                }
            },
            _ => raw.clone(),
        };

        // Generate a unique tag from source span for the static state.
        let tag = format!("_p_{}_{}", raw.span.start, raw.span.end);
        let label = property_label(v, raw);

        // Pre-walk the body for `$past`/`$rose`/`$fell`/`$stable` occurrences.
        // Each gets a static delay slot + a per-call current-value local;
        // emit_expr substitutes references into the closure's predicate.
        let temporals = collect_temporal_occurrences(&body);

        self.pad(depth);
        writeln!(self.out, "_checkers.push_back([&]() {{").ok();
        // Delay-state cells survive across closure calls and live in the
        // selected run's context.
        let ps_cells: Vec<String> = (0..temporals.len())
            .map(|i| self.per_run_cell(format!("{tag}_ps{i}"), "int64_t", Some("0")))
            .collect();
        // Current-value locals + populate prop_subs.
        for (i, t) in temporals.iter().enumerate() {
            self.pad(depth + 1);
            write!(self.out, "int64_t {tag}_cur{i} = (int64_t)(").ok();
            self.emit_expr(&t.inner);
            writeln!(self.out, ");").ok();
            let ps = &ps_cells[i];
            let sub = match t.kind {
                SystemFn::Past => format!("{ps}"),
                SystemFn::Rose => format!("(!{ps} && {tag}_cur{i})"),
                SystemFn::Fell => format!("({ps} && !{tag}_cur{i})"),
                SystemFn::Stable => format!("({ps} == {tag}_cur{i})"),
                SystemFn::Clog2 => continue, // not temporal — skip
            };
            self.prop_subs
                .insert(SourceSite::new(self.current_source_id, t.call_span), sub);
        }
        match &*body.kind {
            // a |=> b — one-cycle-delayed implication. State: prev_a.
            ExprKind::Binary {
                op: BinaryOp::PipeImpliesNext,
                lhs,
                rhs,
            } => {
                let prev_cell = self.per_run_cell(format!("{tag}_prev"), "bool", Some("false"));
                self.pad(depth + 1);
                write!(self.out, "bool _curr_a = (bool)(").ok();
                self.emit_expr(lhs);
                writeln!(self.out, ");").ok();
                self.pad(depth + 1);
                write!(self.out, "bool _curr_b = (bool)(").ok();
                self.emit_expr(rhs);
                writeln!(self.out, ");").ok();
                self.pad(depth + 1);
                writeln!(self.out, "if ({prev_cell} && !_curr_b) {{").ok();
                self.pad(depth + 2);
                writeln!(
                    self.out,
                    "sim_log_line(\"{severity}\", \"property `{}` failed (|=>)\");",
                    escape_c(&label)
                )
                .ok();
                if severity == "FAIL" {
                    self.pad(depth + 2);
                    let runtime_context = self.randomize_context_ref().to_string();
                    writeln!(self.out, "{runtime_context}.errors++;").ok();
                }
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
                self.pad(depth + 1);
                writeln!(self.out, "{prev_cell} = _curr_a;").ok();
            }
            // a |-> b — same-cycle implication.
            ExprKind::Binary {
                op: BinaryOp::PipeImplies,
                lhs,
                rhs,
            } => {
                self.pad(depth + 1);
                write!(self.out, "if ((bool)(").ok();
                self.emit_expr(lhs);
                write!(self.out, ") && !(bool)(").ok();
                self.emit_expr(rhs);
                writeln!(self.out, ")) {{").ok();
                self.pad(depth + 2);
                writeln!(
                    self.out,
                    "sim_log_line(\"{severity}\", \"property `{}` failed (|->)\");",
                    escape_c(&label)
                )
                .ok();
                if severity == "FAIL" {
                    self.pad(depth + 2);
                    let runtime_context = self.randomize_context_ref().to_string();
                    writeln!(self.out, "{runtime_context}.errors++;").ok();
                }
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
            }
            // Plain bool — same-cycle invariant.
            _ => {
                self.pad(depth + 1);
                write!(self.out, "if (!(").ok();
                self.emit_expr(&body);
                writeln!(self.out, ")) {{").ok();
                self.pad(depth + 2);
                writeln!(
                    self.out,
                    "sim_log_line(\"{severity}\", \"property `{}` failed\");",
                    escape_c(&label)
                )
                .ok();
                if severity == "FAIL" {
                    self.pad(depth + 2);
                    let runtime_context = self.randomize_context_ref().to_string();
                    writeln!(self.out, "{runtime_context}.errors++;").ok();
                }
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
            }
        }
        // Update delay slots from current values for next-cycle reads.
        for (i, _t) in temporals.iter().enumerate() {
            self.pad(depth + 1);
            writeln!(self.out, "{} = {tag}_cur{i};", ps_cells[i]).ok();
        }
        // Clear substitutions so they don't leak into other emissions.
        self.prop_subs.clear();

        self.pad(depth);
        writeln!(self.out, "}});").ok();
    }

    /// Emit a cycle-trigger `on <bool-expr>` handler — registers a closure
    /// that fires per the handler's `edge` mode (rising / falling / level).
    /// Used by both the test-scope `on` form and monitor body handlers; the
    /// `prefix` distinguishes the static-state tags so concurrent handlers
    /// at the same span don't collide.
    ///
    /// Also handles the periodic form `on <N> cycles ... end on` (spec
    /// §7.10) — fires the body once every `<N>` primary-clock cycles.
    /// `N` is re-read each cycle so per-test overrides via component-
    /// field initialization (or a free `int wdog_period = 5000`)
    /// flow through naturally without rebuilding.
    pub(super) fn emit_cycle_trigger(&mut self, h: &OnHandler, depth: usize, prefix: &str) {
        let tag = format!("{prefix}{}_{}", h.event.span.start, h.event.span.end);
        let target_vec = match h.phase {
            OnPhase::Checker => "_checkers",
            OnPhase::PostEval => "_post_eval_services",
        };
        self.pad(depth);
        writeln!(self.out, "{target_vec}.push_back([&]() {{").ok();
        if h.periodic {
            // Periodic: fire body when cycle_count - last_fired >= N.
            // Initial state: last_fired = 0 means first firing is at
            // cycle N (NOT cycle 0 — the user didn't ask for an
            // immediate-fire-on-start; they asked for "every N cycles").
            let last_cell = self.per_run_cell(format!("{tag}_last"), "int64_t", Some("0"));
            self.pad(depth + 1);
            write!(self.out, "int64_t {tag}_period = (int64_t)(").ok();
            self.emit_expr(&h.event);
            writeln!(self.out, ");").ok();
            // Guard against period <= 0 — a misconfigured period would
            // otherwise spin-fire every cycle (negative) or every cycle
            // forever (zero). Treat as no-op.
            self.pad(depth + 1);
            writeln!(
                self.out,
                "if ({tag}_period > 0 && (int64_t)cycle_count - {last_cell} >= {tag}_period) {{"
            )
            .ok();
            self.pad(depth + 2);
            writeln!(self.out, "{last_cell} = (int64_t)cycle_count;").ok();
            self.emit_block(&h.body, depth + 2);
            self.pad(depth + 1);
            writeln!(self.out, "}}").ok();
            self.pad(depth);
            writeln!(self.out, "}});").ok();
            return;
        }
        match h.edge {
            EdgeMode::Level => {
                self.pad(depth + 1);
                write!(self.out, "if ((bool)(").ok();
                self.emit_expr(&h.event);
                writeln!(self.out, ")) {{").ok();
                self.emit_block(&h.body, depth + 2);
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
            }
            EdgeMode::Rising | EdgeMode::Falling => {
                let prev_cell = self.per_run_cell(format!("{tag}_prev"), "bool", Some("false"));
                self.pad(depth + 1);
                write!(self.out, "bool {tag}_curr = (bool)(").ok();
                self.emit_expr(&h.event);
                writeln!(self.out, ");").ok();
                self.pad(depth + 1);
                let cond = match h.edge {
                    EdgeMode::Rising => format!("!{prev_cell} && {tag}_curr"),
                    EdgeMode::Falling => format!("{prev_cell} && !{tag}_curr"),
                    EdgeMode::Level => unreachable!(),
                };
                writeln!(self.out, "if ({cond}) {{").ok();
                self.emit_block(&h.body, depth + 2);
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
                self.pad(depth + 1);
                writeln!(self.out, "{prev_cell} = {tag}_curr;").ok();
            }
        }
        self.pad(depth);
        writeln!(self.out, "}});").ok();
    }
}
