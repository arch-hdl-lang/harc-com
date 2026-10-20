//! Native C++ trace, logging, and per-run state emission.

use super::*;

impl Emitter {
    pub(super) fn trace_component_context(&self) -> String {
        self.current_component_instance.clone().unwrap_or_default()
    }

    pub(super) fn emit_tlm_call_trace_event(
        &mut self,
        component: &str,
        bus: &str,
        method: &str,
        phase: &str,
        direction: &str,
        tag: Option<&str>,
        depth: usize,
    ) {
        self.pad(depth);
        write!(
            self.out,
            "trace.tlm_call(cycle_count, \"{}\", \"{}\", \"{}\", \"{}\", \"{}\"",
            escape_c(component),
            escape_c(bus),
            escape_c(method),
            escape_c(phase),
            escape_c(direction),
        )
        .ok();
        if let Some(tag) = tag {
            write!(self.out, ", (int64_t)({tag})").ok();
        }
        writeln!(self.out, ");").ok();
    }

    /// Emit a `log` or `logf` call. When `file_path` is `Some`, lower to
    /// `sim_logf_line(log_ctx.file(path), sev, fmt, args)`; otherwise lower
    /// to `sim_log_line(sev, fmt, args)`. Severity / message extraction
    /// matches `log()`'s rules (first ident is severity; first string is
    /// the message).
    pub(super) fn emit_log(&mut self, args: &[CallArg], file_path: Option<String>, depth: usize) {
        let sev = args
            .iter()
            .find_map(|a| match a {
                CallArg::Expr(e) => match &*e.kind {
                    ExprKind::Ident(id) => Some(id.name.to_uppercase()),
                    _ => None,
                },
                _ => None,
            })
            .unwrap_or_else(|| "INFO".to_string());
        let msg = args
            .iter()
            .find_map(|a| match a {
                CallArg::Expr(e) => match &*e.kind {
                    ExprKind::String(s) => Some(s.clone()),
                    _ => None,
                },
                _ => None,
            })
            .unwrap_or_else(|| "".to_string());
        let (fmt, caps) = process_interp(&msg);
        // Make interpolation sequencing explicit instead of relying on the
        // unspecified evaluation order of C++ function arguments.
        self.pad(depth);
        writeln!(self.out, "{{").ok();
        self.emit_interp_capture_temps(&caps, depth + 1);
        self.pad(depth + 1);
        match file_path {
            Some(p) => {
                write!(
                    self.out,
                    "sim_logf_line(log_ctx.file(\"{}\"), \"{}\", \"{}\"",
                    escape_c(&p),
                    sev,
                    escape_c(&fmt)
                )
                .ok();
            }
            None => {
                write!(self.out, "sim_log_line(\"{}\", \"{}\"", sev, escape_c(&fmt)).ok();
            }
        }
        for (index, c) in caps.iter().enumerate() {
            self.emit_interp_temp_arg(c, index);
        }
        writeln!(self.out, ");").ok();

        // Spec §7.7 test-result semantics:
        //   `log(error, ...)` increments the failure counter; the test
        //                     fails at end of run if any error logged.
        //   `log(fatal, ...)` increments + sets `_fatal` so the main
        //                     simulation loop exits at end of the
        //                     current cycle (this test instance aborts;
        //                     others in a regression continue).
        // `warn` / `info` / `debug` have no test-result effect.
        match sev.as_str() {
            "ERROR" => {
                self.pad(depth + 1);
                let runtime_context = self.randomize_context_ref().to_string();
                writeln!(self.out, "{runtime_context}.errors++;").ok();
            }
            "FATAL" => {
                self.pad(depth + 1);
                let runtime_context = self.randomize_context_ref().to_string();
                writeln!(self.out, "{runtime_context}.errors++; _fatal = true;").ok();
            }
            _ => {}
        }
        self.pad(depth);
        writeln!(self.out, "}}").ok();
    }

    /// Evaluate each interpolation capture in source order before passing
    /// it to a variadic C++ logging function.
    pub(super) fn emit_interp_capture_temps(&mut self, caps: &[InterpCap], depth: usize) {
        for (index, cap) in caps.iter().enumerate() {
            self.pad(depth);
            write!(self.out, "auto _harc_interp_{index} = ").ok();
            match crate::parser::parse_expr_fragment(&cap.expr) {
                Ok(e) => self.emit_expr(&e),
                Err(_) => self.reject_unparseable_capture(&cap.expr),
            }
            writeln!(self.out, ";").ok();
        }
    }

    /// Emit one previously snapshotted interpolation value with the printf
    /// carrier selected for its format specifier.
    pub(super) fn emit_interp_temp_arg(&mut self, cap: &InterpCap, index: usize) {
        write!(self.out, ", ").ok();
        match cap.wide_hex {
            Some((width, upper)) => {
                let upper_str = if upper { "true" } else { "false" };
                if width > 32 {
                    write!(self.out, "(const char*)harc_rt::HarcHexBufWide(").ok();
                } else {
                    write!(self.out, "(const char*)harc_rt::HarcHexBuf128(").ok();
                }
                write!(self.out, "_harc_interp_{index}, {width}, {upper_str})").ok();
            }
            None => {
                write!(self.out, "harc_rt::harc_printf_ll(_harc_interp_{index})").ok();
            }
        }
    }

    /// Emit a top-level `function` as a `[&]`-captured lambda. Named-typed
    /// parameters are recognised as DUT pointers (`VName*`); their field
    /// access in the body uses `->`. Capture-all (`[&]`) so the body can
    /// still see `tick` and any test-level state.
    /// Emit a `tseq` declaration as a `[&]`-capturing lambda that returns
    /// a `std::vector<T>` filled in by `yield` statements. The inner type
    /// `T` is the argument of `TSeq<T>` in the return-type slot.
    /// Emit one test's `run_<Name>(int argc, char** argv)` entry point:
    /// prologue (runtime context, DUT, waves, seed, scheduler), the
    /// scenario coroutine, the drive loop, and the epilogue. Shared by
    /// the legacy single-TU backend and the common-object capsule
    /// emitter so both produce identical run bodies.
    /// Register (or look up) a per-run mutable cell named `tag` and
    /// return the C++ lvalue every use must go through.
    ///
    /// Every layout records the cell in its per-run context and returns its
    /// `ctx._cells.<tag>` lvalue. Tags are derived from source spans, so they
    /// are stable across emission orders and independent of test registry
    /// position.
    pub(super) fn per_run_cell(&mut self, tag: String, ctype: &str, init: Option<&str>) -> String {
        let init = init.map(str::to_string);
        if let Some(existing) = self.per_run_cells.iter().find(|cell| cell.tag == tag) {
            if existing.ctype != ctype || existing.init != init {
                self.errors.push(format!(
                    "randomize runtime cell `{tag}` has conflicting declarations: `{}` / {:?} versus `{ctype}` / {:?}",
                    existing.ctype, existing.init, init
                ));
            }
        } else {
            self.per_run_cells.push(PerRunCell {
                tag: tag.clone(),
                ctype: ctype.to_string(),
                init,
            });
        }
        format!("{}.{tag}", self.randomize_state_ref())
    }

    pub(super) fn emit_capsule_state_decl(&mut self, depth: usize) {
        if self.flavor != Flavor::Capsule || self.per_run_cells.is_empty() {
            return;
        }
        let cells = self.per_run_cells.clone();
        self.pad(depth);
        writeln!(self.out, "struct {{").ok();
        for cell in cells {
            self.pad(depth + 1);
            match cell.init {
                Some(init) => {
                    writeln!(self.out, "{} {} = {};", cell.ctype, cell.tag, init).ok();
                }
                None => {
                    writeln!(self.out, "{} {}{{}};", cell.ctype, cell.tag).ok();
                }
            }
        }
        self.pad(depth);
        writeln!(self.out, "}} {}{{}};", self.capsule_state_ident).ok();
    }

    /// Whether `name` is emitted as a member of the per-run glue object
    /// (common TU) rather than as a run-scope `[&]` lambda. Under the
    /// legacy layout nothing is a glue member; under the common layout
    /// everything is EXCEPT the bus-bound hookables, which stay
    /// capsule-local (see `bus_bound_component_types`). Declaration form
    /// and call-site spelling must both consult this — a callable
    /// declared one way and called the other does not link.
    pub(super) fn is_glue_member(&self, name: &str) -> bool {
        self.flavor != Flavor::Legacy && self.glue_callees.contains(name)
    }

    /// Reference to the auto-coverage state cell for `cache_tag`.
    /// Legacy yields the function-local static's name; the common
    /// layout yields the per-run `ctx._cells` slot. EVERY reader must
    /// go through this: the declaration and its use sites live in
    /// different emitter methods, so a bare `_auto_cov_state_<tag>`
    /// left behind in one of them is an undeclared identifier the
    /// moment the static stops being emitted.
    pub(super) fn auto_cov_state_ref(&mut self, cache_tag: &str) -> String {
        self.per_run_cell(
            format!("_auto_cov_state_{cache_tag}"),
            "harc_rt::random::HarcAutoCovState",
            None,
        )
    }

    /// Reference to the persistent `[unique within ...]` history for one
    /// semantic record field. The registry key deliberately excludes the
    /// randomize call site: `within test` shares across every site in that
    /// run, while tseq invocations and sequencer instances own separate
    /// registries. Bare `[unique]` never calls this helper because it has no
    /// persistent lifetime.
    pub(super) fn unique_history_ref(
        &mut self,
        scope: &str,
        record: &str,
        c_name: &str,
        value_ty: &str,
    ) -> String {
        let key = stable_identity_hash(
            "harc-unique-history-v1",
            &[scope.as_bytes(), record.as_bytes(), c_name.as_bytes()],
        );
        let registry = match scope {
            "test" => format!("{}.rng.unique", self.randomize_context_ref()),
            "tseq" => {
                if self.randomize_context_sentinel.is_some() {
                    TBIR_RANDOMIZE_TSEQ_SCOPE_SENTINEL.to_string()
                } else if let Some(registry) = &self.current_unique_tseq_registry {
                    registry.clone()
                } else {
                    self.errors.push(format!(
                        "randomize({record}): `[unique within tseq]` is only valid inside a tseq"
                    ));
                    format!("{}.rng.unique", self.randomize_context_ref())
                }
            }
            "sequencer" => {
                if self.randomize_context_sentinel.is_some() {
                    TBIR_RANDOMIZE_SEQUENCER_SCOPE_SENTINEL.to_string()
                } else if let Some((component, receiver, _, _)) = &self.current_component_method {
                    if let Some(decl) = self
                        .components
                        .get(component)
                        .filter(|decl| decl.kind == ComponentKind::Sequencer)
                    {
                        let member =
                            component_unique_registry_name(decl.items.iter().filter_map(|item| {
                                let ComponentItem::Field(field) = item else {
                                    return None;
                                };
                                Some(field.name.name.as_str())
                            }));
                        format!("{receiver}.{member}")
                    } else {
                        self.errors.push(format!(
                            "randomize({record}): `[unique within sequencer]` is only valid inside a sequencer"
                        ));
                        format!("{}.rng.unique", self.randomize_context_ref())
                    }
                } else {
                    self.errors.push(format!(
                        "randomize({record}): `[unique within sequencer]` is only valid inside a sequencer"
                    ));
                    format!("{}.rng.unique", self.randomize_context_ref())
                }
            }
            other => {
                self.errors.push(format!(
                    "randomize({record}): unsupported unique scope `{other}`"
                ));
                format!("{}.rng.unique", self.randomize_context_ref())
            }
        };
        format!("{registry}.get<{value_ty}>({key}ULL)")
    }

    /// Bind the historical trace-dump helper names to their glue
    /// implementations inside a common-layout capsule.
    pub(super) fn emit_capsule_dump_shims(&mut self) {
        writeln!(
            self.out,
            "{INDENT}auto _harc_trace_dump_next = [&](const char* clock, uint64_t clock_cycle) {{ _glue._harc_trace_dump_next(clock, clock_cycle); }};"
        )
        .ok();
        writeln!(
            self.out,
            "{INDENT}auto _harc_trace_dump_at = [&](uint64_t t, const char* clock, uint64_t clock_cycle) {{ _glue._harc_trace_dump_at(t, clock, clock_cycle); }};"
        )
        .ok();
    }
}
