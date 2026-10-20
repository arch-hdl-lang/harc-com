//! Native C++ covergroup structure, sampling, and reporting emission.

use super::*;

impl Emitter {
    /// Emit the C++ boolean expression for "is `_v` in this bin?".
    ///   `{x}`        — `(_v == x)`
    ///   `{a, b, c}`  — `(_v == a || _v == b || _v == c)`
    ///   `[a..b]`     — `(_v >= a && _v <= b)`
    ///   `(inner)`    — recurse
    ///   anything else — falls back to `(_v == <expr>)` (e.g. a single Int).
    fn emit_bin_membership(&mut self, spec: &Expr) {
        match &*spec.kind {
            ExprKind::SetLit(items) => {
                write!(self.out, "(").ok();
                if items.is_empty() {
                    write!(self.out, "false").ok();
                } else {
                    for (i, it) in items.iter().enumerate() {
                        if i > 0 {
                            write!(self.out, " || ").ok();
                        }
                        // Recurse — set-of-ranges (`{[1..3], 7}`) works.
                        self.emit_bin_membership(it);
                    }
                }
                write!(self.out, ")").ok();
            }
            ExprKind::RangeLit { lo, hi } => {
                write!(self.out, "(").ok();
                if let Some(l) = lo {
                    write!(self.out, "_v >= ").ok();
                    self.emit_expr(l);
                    if hi.is_some() {
                        write!(self.out, " && ").ok();
                    }
                }
                if let Some(h) = hi {
                    write!(self.out, "_v <= ").ok();
                    self.emit_expr(h);
                }
                if lo.is_none() && hi.is_none() {
                    write!(self.out, "true").ok();
                }
                write!(self.out, ")").ok();
            }
            ExprKind::Paren(inner) => self.emit_bin_membership(inner),
            // Default: equality test against the expression. Covers the
            // single-value form (`{1}` parses as a SetLit but `1` alone
            // would land here).
            _ => {
                write!(self.out, "(_v == ").ok();
                self.emit_expr(spec);
                write!(self.out, ")").ok();
            }
        }
    }

    /// Emit a covergroup as a C++ struct: one nested struct per cover-point
    /// holding `uint64_t` bin counters plus sample-local cross counters.
    /// Crosses are updated only from bins hit in the same sample invocation;
    /// this is deliberately a semantic sample record, not a post-hoc mix of
    /// bins hit at unrelated times.
    pub(super) fn emit_covergroup_struct(&mut self, g: &CovergroupDecl) {
        let auto_crosses = covergroup_auto_crosses(g);
        let declared_crosses = match covergroup_declared_crosses(g) {
            Ok(crosses) => crosses,
            Err(err) => {
                self.errors.push(err);
                Vec::new()
            }
        };
        writeln!(self.out, "struct {} {{", g.name.name).ok();
        // Per-cover-point nested struct of bin counters. The struct is
        // pure data; the sample() logic is emitted at `let cov : G` time
        // as a `_checkers` closure (so `dut` is in scope via `[&]`
        // capture). report() lives here because it only reads its own
        // counters.
        for it in &g.items {
            if let CoverItem::Point(p) = it {
                writeln!(self.out, "{INDENT}struct {{").ok();
                for b in &p.bins {
                    writeln!(self.out, "{INDENT}{INDENT}uint64_t {} = 0;", b.name.name).ok();
                }
                writeln!(self.out, "{INDENT}}} {};", p.name.name).ok();
            }
        }
        for (a, b) in &auto_crosses {
            writeln!(
                self.out,
                "{INDENT}uint64_t _auto_cross_{}__{}[{}][{}] = {{}};",
                a.name.name,
                b.name.name,
                a.bins.len(),
                b.bins.len()
            )
            .ok();
        }
        for cross in &declared_crosses {
            writeln!(
                self.out,
                "{INDENT}uint64_t {}[{}] = {{}};",
                cross.storage, cross.total_bins
            )
            .ok();
        }
        writeln!(self.out, "{INDENT}FILE* _harc_coverage_json = nullptr;").ok();
        writeln!(self.out, "").ok();
        // report() — ARCH-format coverage dump (header line + per-bin
        // lines + `*NOT HIT*` marker). Mirrors arch-com's
        // sim_codegen/fsm.rs `_arch_cov_dump` shape, but writes to stdout
        // (not stderr): in a HARC TB the program's stdout already IS
        // the framework log (everything the user wrote via log() goes
        // there too). ARCH writes to stderr because in ARCH-sim, stdout
        // belongs to the user's test program — that distinction doesn't
        // apply to a HARC-emitted TB.
        let total_bins: usize = g
            .items
            .iter()
            .map(|it| match it {
                CoverItem::Point(p) => p.bins.len(),
                _ => 0,
            })
            .sum();
        writeln!(self.out, "{INDENT}void report() const {{").ok();
        self.pad(2);
        writeln!(
            self.out,
            "uint64_t _total = {total_bins}; uint64_t _hit = 0;"
        )
        .ok();
        for it in &g.items {
            if let CoverItem::Point(p) = it {
                for b in &p.bins {
                    self.pad(2);
                    writeln!(self.out, "if ({}.{} > 0) _hit++;", p.name.name, b.name.name).ok();
                }
            }
        }
        self.pad(2);
        writeln!(
            self.out,
            "harc_rt::log::harc_print_covergroup_summary(\"{}\", _hit, _total);",
            g.name.name
        )
        .ok();
        self.pad(2);
        writeln!(
            self.out,
            "harc_rt::log::harc_cov_json_summary(_harc_coverage_json, \"{}\", _hit, _total);",
            g.name.name
        )
        .ok();
        for it in &g.items {
            if let CoverItem::Point(p) = it {
                for b in &p.bins {
                    self.pad(2);
                    writeln!(
                        self.out,
                        "harc_rt::log::harc_print_covergroup_bin(\"{0}\", \"{1}\", {0}.{1});",
                        p.name.name, b.name.name
                    )
                    .ok();
                    self.pad(2);
                    writeln!(
                        self.out,
                        "harc_rt::log::harc_cov_json_bin(_harc_coverage_json, \"{}\", \"{}\", \"{}\", {}.{});",
                        g.name.name, p.name.name, b.name.name, p.name.name, b.name.name
                    )
                    .ok();
                }
            }
        }
        for cross in &declared_crosses {
            self.pad(2);
            writeln!(self.out, "{{").ok();
            self.pad(3);
            writeln!(self.out, "uint64_t _cross_hit = 0;").ok();
            self.pad(3);
            writeln!(self.out, "uint64_t _cross_missing = 0;").ok();
            self.pad(3);
            writeln!(
                self.out,
                "for (size_t _i = 0; _i < {}; ++_i) if ({}[_i] > 0) _cross_hit++;",
                cross.total_bins, cross.storage
            )
            .ok();
            self.pad(3);
            writeln!(
                self.out,
                "harc_rt::log::harc_print_covergroup_cross_summary(\"{}\", \"cross\", \"{}\", _cross_hit, {});",
                g.name.name,
                escape_c(&cross.label),
                cross.total_bins
            )
            .ok();
            self.pad(3);
            writeln!(
                self.out,
                "harc_rt::log::harc_cov_json_cross_summary(_harc_coverage_json, \"{}\", \"cross\", \"{}\", _cross_hit, {});",
                g.name.name,
                escape_c(&cross.label),
                cross.total_bins
            )
            .ok();
            for (idx, label) in declared_cross_bin_labels(cross) {
                self.pad(3);
                writeln!(
                    self.out,
                    "harc_rt::log::harc_cov_json_cross_bin(_harc_coverage_json, \"{}\", \"cross\", \"{}\", \"{}\", {}[{idx}]);",
                    g.name.name,
                    escape_c(&cross.label),
                    escape_c(&label),
                    cross.storage
                )
                .ok();
                self.pad(3);
                writeln!(
                    self.out,
                    "if ({}[{}] == 0) {{ if (_cross_missing < {}) harc_rt::log::harc_print_covergroup_missing_bin(\"{}\"); _cross_missing++; }}",
                    cross.storage,
                    idx,
                    COVERGROUP_CROSS_MISSING_DETAIL_LIMIT,
                    escape_c(&label)
                )
                .ok();
            }
            self.pad(3);
            writeln!(
                self.out,
                "harc_rt::log::harc_print_covergroup_more_missing(_cross_missing, {}, \"cross\");",
                COVERGROUP_CROSS_MISSING_DETAIL_LIMIT,
            )
            .ok();
            self.pad(2);
            writeln!(self.out, "}}").ok();
        }
        for (a, b) in &auto_crosses {
            self.pad(2);
            writeln!(self.out, "{{").ok();
            self.pad(3);
            writeln!(self.out, "uint64_t _cross_hit = 0;").ok();
            self.pad(3);
            writeln!(self.out, "uint64_t _cross_missing = 0;").ok();
            self.pad(3);
            writeln!(
                self.out,
                "for (size_t _i = 0; _i < {}; ++_i) for (size_t _j = 0; _j < {}; ++_j) if (_auto_cross_{}__{}[_i][_j] > 0) _cross_hit++;",
                a.bins.len(),
                b.bins.len(),
                a.name.name,
                b.name.name
            )
            .ok();
            self.pad(3);
            writeln!(
                self.out,
                "harc_rt::log::harc_print_covergroup_cross_summary(\"{}\", \"auto_cross\", \"{} x {}\", _cross_hit, {});",
                escape_c(&g.name.name),
                escape_c(&a.name.name),
                escape_c(&b.name.name),
                a.bins.len() * b.bins.len()
            )
            .ok();
            self.pad(3);
            writeln!(
                self.out,
                "harc_rt::log::harc_cov_json_cross_summary(_harc_coverage_json, \"{}\", \"auto_cross\", \"{} x {}\", _cross_hit, {});",
                escape_c(&g.name.name),
                escape_c(&a.name.name),
                escape_c(&b.name.name),
                a.bins.len() * b.bins.len()
            )
            .ok();
            for (i, ab) in a.bins.iter().enumerate() {
                for (j, bb) in b.bins.iter().enumerate() {
                    self.pad(3);
                    writeln!(
                        self.out,
                        "harc_rt::log::harc_cov_json_cross_bin(_harc_coverage_json, \"{}\", \"auto_cross\", \"{} x {}\", \"{}.{} x {}.{}\", _auto_cross_{}__{}[{}][{}]);",
                        escape_c(&g.name.name),
                        escape_c(&a.name.name),
                        escape_c(&b.name.name),
                        escape_c(&a.name.name),
                        escape_c(&ab.name.name),
                        escape_c(&b.name.name),
                        escape_c(&bb.name.name),
                        a.name.name,
                        b.name.name,
                        i,
                        j
                    )
                    .ok();
                    self.pad(3);
                    writeln!(
                        self.out,
                        "if (_auto_cross_{}__{}[{}][{}] == 0) {{ if (_cross_missing < {}) harc_rt::log::harc_print_covergroup_missing_bin(\"{}.{} x {}.{}\"); _cross_missing++; }}",
                        a.name.name,
                        b.name.name,
                        i,
                        j,
                        COVERGROUP_CROSS_MISSING_DETAIL_LIMIT,
                        a.name.name,
                        ab.name.name,
                        b.name.name,
                        bb.name.name
                    )
                    .ok();
                }
            }
            self.pad(3);
            writeln!(
                self.out,
                "harc_rt::log::harc_print_covergroup_more_missing(_cross_missing, {}, \"auto-cross\");",
                COVERGROUP_CROSS_MISSING_DETAIL_LIMIT,
            )
            .ok();
            self.pad(2);
            writeln!(self.out, "}}").ok();
        }
        writeln!(self.out, "{INDENT}}}").ok();
        writeln!(self.out, "}};").ok();
        writeln!(self.out, "").ok();
    }

    /// At a `let cov : G` site, register the covergroup's sample logic.
    /// Clock/no-trigger covergroups use the legacy per-cycle `_checkers`
    /// path; hook-triggered covergroups subscribe to the existing
    /// `<Type>_<method>_pre/post` hook vectors so sampling occurs at the
    /// semantic hook invocation point rather than every cycle.
    pub(super) fn emit_covergroup_sample_registration(
        &mut self,
        g: &CovergroupDecl,
        instance: &str,
        depth: usize,
    ) {
        self.pad(depth);
        writeln!(
            self.out,
            "{instance}._harc_coverage_json = log_ctx.coverage_json;"
        )
        .ok();
        match &g.trigger {
            Some(CoverTrigger::Hook { call, side }) => {
                self.emit_covergroup_hook_sample_registration(g, instance, call, *side, depth);
            }
            _ => self.emit_covergroup_clock_sample_registration(g, instance, depth),
        }
    }

    fn emit_covergroup_clock_sample_registration(
        &mut self,
        g: &CovergroupDecl,
        instance: &str,
        depth: usize,
    ) {
        let binned_points = covergroup_binned_points(g);
        let auto_crosses = covergroup_auto_crosses(g);
        let declared_crosses = match covergroup_declared_crosses(g) {
            Ok(crosses) => crosses,
            Err(_) => Vec::new(),
        };
        self.pad(depth);
        writeln!(self.out, "_checkers.push_back([&]() {{").ok();
        self.emit_covergroup_sample_body(
            g,
            instance,
            &binned_points,
            &auto_crosses,
            &declared_crosses,
            depth + 1,
        );
        self.pad(depth);
        writeln!(self.out, "}});").ok();
    }

    fn emit_covergroup_hook_sample_registration(
        &mut self,
        g: &CovergroupDecl,
        instance: &str,
        call: &Expr,
        side: HookSide,
        depth: usize,
    ) {
        let ExprKind::Call { callee, args } = &*call.kind else {
            self.errors.push(format!(
                "covergroup `{}` hook trigger must be a method call",
                g.name.name
            ));
            return;
        };
        let Some((comp_ty, method_name, params)) = self.resolve_component_hookable(callee) else {
            self.errors.push(format!(
                "covergroup `{}` hook trigger must resolve to a `hookable` on a known component type",
                g.name.name
            ));
            return;
        };
        if args.len() != params.len() {
            self.errors.push(format!(
                "covergroup `{}` hook trigger `{method_name}` expects {} argument(s), got {}",
                g.name.name,
                params.len(),
                args.len()
            ));
            return;
        }
        for (arg, param) in args.iter().zip(params.iter()) {
            let CallArg::Expr(arg_expr) = arg else {
                self.errors.push(format!(
                    "covergroup `{}` hook trigger arguments must be identifiers",
                    g.name.name
                ));
                return;
            };
            let ExprKind::Ident(arg_name) = &*arg_expr.kind else {
                self.errors.push(format!(
                    "covergroup `{}` hook trigger arguments must be identifiers",
                    g.name.name
                ));
                return;
            };
            if arg_name.name != param.name.name || arg_name.name == "_" {
                self.errors.push(format!(
                    "covergroup `{}` hook trigger argument `{}` must match hook parameter `{}`",
                    g.name.name, arg_name.name, param.name.name
                ));
                return;
            }
        }

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
        let binned_points = covergroup_binned_points(g);
        let auto_crosses = covergroup_auto_crosses(g);
        let declared_crosses = match covergroup_declared_crosses(g) {
            Ok(crosses) => crosses,
            Err(_) => Vec::new(),
        };
        self.pad(depth);
        writeln!(
            self.out,
            "{comp_ty}_{method_name}_{side_str}.push_back([&]({}) {{",
            arg_decls.join(", "),
        )
        .ok();
        self.emit_covergroup_sample_body(
            g,
            instance,
            &binned_points,
            &auto_crosses,
            &declared_crosses,
            depth + 1,
        );
        self.pad(depth);
        writeln!(self.out, "}});").ok();
    }

    fn emit_covergroup_sample_body(
        &mut self,
        g: &CovergroupDecl,
        instance: &str,
        binned_points: &[&CoverPoint],
        auto_crosses: &[(&CoverPoint, &CoverPoint)],
        declared_crosses: &[DeclaredCoverCross<'_>],
        depth: usize,
    ) {
        if !auto_crosses.is_empty() || !declared_crosses.is_empty() {
            for p in binned_points {
                self.pad(depth);
                writeln!(
                    self.out,
                    "bool _cg_hit_{}[{}] = {{}};",
                    p.name.name,
                    p.bins.len()
                )
                .ok();
            }
        }
        for it in &g.items {
            if let CoverItem::Point(p) = it {
                self.pad(depth);
                writeln!(self.out, "{{").ok();
                self.pad(depth + 1);
                write!(self.out, "uint64_t _v = (uint64_t)(").ok();
                self.emit_expr(&p.target);
                writeln!(self.out, ");").ok();
                for (bin_idx, b) in p.bins.iter().enumerate() {
                    self.pad(depth + 1);
                    write!(self.out, "if (").ok();
                    self.emit_bin_membership(&b.spec);
                    if auto_crosses.is_empty() && declared_crosses.is_empty() {
                        writeln!(self.out, ") {instance}.{}.{}++;", p.name.name, b.name.name).ok();
                    } else {
                        writeln!(
                            self.out,
                            ") {{ {instance}.{}.{}++; _cg_hit_{}[{}] = true; }}",
                            p.name.name, b.name.name, p.name.name, bin_idx
                        )
                        .ok();
                    }
                }
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
        }
        for (a, b) in auto_crosses {
            self.pad(depth);
            writeln!(
                self.out,
                "for (size_t _i = 0; _i < {}; ++_i) {{",
                a.bins.len()
            )
            .ok();
            self.pad(depth + 1);
            writeln!(
                self.out,
                "for (size_t _j = 0; _j < {}; ++_j) {{",
                b.bins.len()
            )
            .ok();
            self.pad(depth + 2);
            writeln!(
                self.out,
                "if (_cg_hit_{}[_i] && _cg_hit_{}[_j]) {instance}._auto_cross_{}__{}[_i][_j]++;",
                a.name.name, b.name.name, a.name.name, b.name.name
            )
            .ok();
            self.pad(depth + 1);
            writeln!(self.out, "}}").ok();
            self.pad(depth);
            writeln!(self.out, "}}").ok();
        }
        for cross in declared_crosses {
            self.emit_declared_cover_cross_sample_update(instance, cross, depth);
        }
    }

    fn emit_declared_cover_cross_sample_update(
        &mut self,
        instance: &str,
        cross: &DeclaredCoverCross<'_>,
        depth: usize,
    ) {
        for (idx, point) in cross.points.iter().enumerate() {
            self.pad(depth + idx);
            writeln!(
                self.out,
                "for (size_t _i{idx} = 0; _i{idx} < {}; ++_i{idx}) {{",
                point.bins.len()
            )
            .ok();
        }
        self.pad(depth + cross.points.len());
        let hit_cond = cross
            .points
            .iter()
            .enumerate()
            .map(|(idx, point)| format!("_cg_hit_{}[_i{idx}]", point.name.name))
            .collect::<Vec<_>>()
            .join(" && ");
        writeln!(self.out, "if ({hit_cond}) {{").ok();
        self.pad(depth + cross.points.len() + 1);
        writeln!(
            self.out,
            "{instance}.{}[{}]++;",
            cross.storage,
            declared_cross_index_expr(cross)
        )
        .ok();
        self.pad(depth + cross.points.len());
        writeln!(self.out, "}}").ok();
        for idx in (0..cross.points.len()).rev() {
            self.pad(depth + idx);
            writeln!(self.out, "}}").ok();
        }
    }
}
