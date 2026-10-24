//! Native emitter declared-name and enum collision analysis.

use super::*;

impl Emitter {
    pub(super) fn fresh_internal_name(&mut self, stem: &str) -> String {
        let mut suffix = 0usize;
        loop {
            let name = format!("__harc_{stem}_{suffix}");
            suffix += 1;
            if !self.declared_value_names.contains(&name)
                && !self.internal_reserved_names.contains(&name)
            {
                self.internal_reserved_names.insert(name.clone());
                return name;
            }
        }
    }

    /// File-scope C++ names this backend writes into EVERY generated file,
    /// which therefore cannot also be an enum variant's `constexpr`.
    /// Unlike a record or component name, nothing in the user's program
    /// declares these, so `collect_unsafe_enum_variants` cannot discover
    /// them by inspection.
    ///
    /// `main` is the only one an ordinary program is likely to hit — `enum
    /// Stage { alpha, main, omega }` is unremarkable naming, and lowercase
    /// variants are spec-sanctioned (`enum Severity { debug, info, ... }`).
    /// That example would read more naturally as `{ setup, main, teardown
    /// }`, but `setup` and `teardown` are reserved words and do not parse.
    ///
    /// The rest are cheap to cover and cost nothing to list.
    const RESERVED_EMITTED_NAMES: &[&str] = &[
        "main",
        "HarcTestContext",
        "HarcTraceC",
        "harc_rng",
        "harc_rng_next",
    ];

    /// Seed `declared_value_names` from the AST, once, before any emission.
    ///
    /// This replaces registering each binder at its own emission site.
    /// Five such sites surfaced across three review passes — two `for`
    /// flavors, the synchronous and actor lowerings of `on <event>(arg)`,
    /// and `on bus.<ch>.handshake(arg)` — each found only by reading a
    /// different emission path. The set is not closable by inspection,
    /// because one HARC binder can have several lowerings and a new one
    /// can be added without touching anything this feature owns.
    ///
    /// A walker is the right tool HERE and the wrong tool for emission,
    /// but NOT because a gap here is harmless — read the consumer before
    /// concluding that. It diagnoses when the name is ABSENT
    /// (`if !declared_value_names.contains(..)`), so the two directions
    /// cost:
    ///
    ///   * missing a name  → the diagnostic fires → a WORKING PROGRAM IS
    ///     REJECTED. This is the expensive direction, and it is the exact
    ///     false-positive class that took three review passes to close.
    ///   * over-collecting → the diagnostic stays silent → a missed
    ///     diagnostic, leaving the user the same raw C++ error this
    ///     backend produced before the feature existed. Cheap.
    ///
    /// What makes a walker safe here is the opposite property from the one
    /// that makes an emission walker dangerous: over-collection is free,
    /// so the walker can be as liberal as it likes, and a gap is backed by
    /// the per-site registrations below. It collects every `Ident`
    /// argument of an `on` handler's event expression, whatever the
    /// handler's eventual lowering, rather than mirroring
    /// `extract_event_subscription` / `extract_bus_handshake_event` and
    /// drifting from them later.
    ///
    /// The per-emission-site registrations are KEPT alongside this and are
    /// LOAD-BEARING, not redundancy — do not delete them as cleanup. Both
    /// mechanisms only ever INSERT into the same set, so their result is a
    /// union and neither can contradict the other, and the union is what
    /// covers the arms this walker does not descend into. Two are known
    /// and verified: a binder inside a `phase <name>` block
    /// (`TestItem::Phase`) and one inside a `watchdog` body
    /// (`ComponentItem::Watchdog`) reach emission with only the per-site
    /// insert to register them. Conversely, the two sites with NO per-site
    /// registration — the `on bus.<ch>.handshake(arg)` argument and the
    /// actor lowering of `on <event>(arg)` — depend entirely on this
    /// walker. Each mechanism covers what the other misses.
    pub(super) fn seed_declared_value_names(&mut self, file: &SourceFile) {
        fn from_block(out: &mut std::collections::HashSet<String>, b: &Block) {
            for st in &b.stmts {
                from_stmt(out, st);
            }
        }
        fn from_on(out: &mut std::collections::HashSet<String>, h: &OnHandler) {
            // `on ev(arg)`, `on bus.w.handshake(arg)`, `on drv.req(arg)` —
            // every spelling puts the binder in the call's argument list.
            if let ExprKind::Call { args, .. } = &*h.event.kind {
                for a in args {
                    if let CallArg::Expr(e) = a {
                        if let ExprKind::Ident(id) = &*e.kind {
                            out.insert(id.name.clone());
                        }
                    }
                }
            }
            from_block(out, &h.body);
        }
        fn from_stmt(out: &mut std::collections::HashSet<String>, st: &Stmt) {
            match &st.kind {
                StmtKind::Let(l) => {
                    out.insert(l.name.name.clone());
                }
                StmtKind::For(f) => {
                    out.insert(f.var.name.clone());
                    from_block(out, &f.body);
                }
                StmtKind::Repeat(r) => from_block(out, &r.body),
                StmtKind::Loop(b) => from_block(out, b),
                StmtKind::While { body, .. } => from_block(out, body),
                StmtKind::If(i) => {
                    from_block(out, &i.then_block);
                    for (_, b) in &i.elsifs {
                        from_block(out, b);
                    }
                    if let Some(b) = &i.else_block {
                        from_block(out, b);
                    }
                }
                StmtKind::Fork(f) => {
                    for b in &f.branches {
                        from_block(out, b);
                    }
                }
                StmtKind::Parallel(bs) | StmtKind::Schedule(bs) => {
                    for b in bs {
                        from_block(out, b);
                    }
                }
                StmtKind::Select(arms) => {
                    for a in arms {
                        from_block(out, &a.action);
                    }
                }
                StmtKind::On(h) => from_on(out, h),
                StmtKind::After { body, .. } => from_block(out, body),
                _ => {}
            }
        }

        let mut out = std::mem::take(&mut self.declared_value_names);
        for it in &file.items {
            match it {
                Item::Function(f) => {
                    out.extend(cpp_param_names(&f.params));
                    from_block(&mut out, &f.body);
                }
                Item::Tseq(t) => {
                    out.extend(cpp_param_names(&t.params));
                    from_block(&mut out, &t.body);
                }
                Item::ExternFn(f) => out.extend(cpp_param_names(&f.params)),
                _ => {}
            }
        }

        // Component-shaped declarations: their items carry handler
        // arguments, hookable/method params, and lifecycle-phase blocks.
        fn from_component_items(
            out: &mut std::collections::HashSet<String>,
            items: &[ComponentItem],
        ) {
            for ci in items {
                match ci {
                    ComponentItem::OnHandler(h) => from_on(out, h),
                    ComponentItem::Hookable(h) => {
                        out.extend(cpp_param_names(&h.params));
                        from_block(out, &h.body);
                    }
                    ComponentItem::TargetTlmThread(t) => {
                        out.extend(cpp_param_names(&t.params));
                        from_block(out, &t.body);
                    }
                    ComponentItem::Lifecycle(_, b) => from_block(out, b),
                    ComponentItem::Watchdog(w) => from_block(out, &w.body),
                    _ => {}
                }
            }
        }

        for it in &file.items {
            match it {
                Item::Agent(c) | Item::Env(c) | Item::Scoreboard(c) | Item::Sequencer(c) => {
                    out.extend(cpp_param_names(&c.params));
                    from_component_items(&mut out, &c.items);
                }
                Item::Transactor(t) => {
                    out.extend(cpp_param_names(&t.params));
                    from_component_items(&mut out, &t.items);
                    if let Some(active) = &t.when_active {
                        from_component_items(&mut out, active);
                    }
                }
                Item::Test(t) => {
                    out.extend(cpp_param_names(&t.params));
                    for ti in &t.items {
                        match ti {
                            TestItem::Let(l) => {
                                out.insert(l.name.name.clone());
                            }
                            TestItem::Stmt(st) => from_stmt(&mut out, st),
                            TestItem::Phase(_, b) => from_block(&mut out, b),
                            TestItem::Scope(sc) => {
                                for b in [&sc.setup, &sc.run, &sc.check, &sc.teardown]
                                    .into_iter()
                                    .flatten()
                                {
                                    from_block(&mut out, b);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        self.declared_value_names = out;
        // Keep file-scope names in a separate set: `declared_value_names` is
        // intentionally local/parameter-only because expression emission uses
        // it to suppress diagnostics for legal local shadowing of enum values.
        for item in &file.items {
            match item {
                Item::Const(c) => {
                    self.internal_reserved_names.insert(c.name.name.clone());
                }
                Item::Enum(e) => {
                    self.internal_reserved_names
                        .extend(e.variants.iter().map(|v| v.name.clone()));
                }
                Item::Relation(r) => {
                    self.internal_reserved_names.insert(r.name.name.clone());
                }
                Item::Function(f) => {
                    self.internal_reserved_names.insert(f.name.name.clone());
                }
                Item::Tseq(t) => {
                    self.internal_reserved_names.insert(t.name.name.clone());
                }
                Item::Property(p) => {
                    self.internal_reserved_names.insert(p.name.name.clone());
                }
                Item::Pseq(p) => {
                    self.internal_reserved_names.insert(p.name.name.clone());
                }
                Item::CoverSequence(c) => {
                    self.internal_reserved_names.insert(c.name.name.clone());
                }
                Item::ExternFn(f) => {
                    self.internal_reserved_names.insert(f.name.name.clone());
                }
                _ => {}
            }
        }
    }

    /// Variant names that must NOT get a file-scope `constexpr`, mapped to
    /// the reason, for the diagnostic a use of one produces.
    ///
    /// Three disqualifiers, each a case where emitting the definition is
    /// worse than not having one:
    ///
    /// * **A declared type already owns the name.** Transactions, structs,
    ///   scoreboards, components, transactors and covergroups emit a
    ///   `struct <Name>`, and `extern function`s emit C-linkage
    ///   declarations. A variable declaration hides a class name in the
    ///   same scope, so `enum Kind { Pkt }` beside `transaction Pkt` turns
    ///   every later `Pkt p;` into "must use 'struct' tag to refer to type
    ///   'Pkt'" — a program that compiles today.
    ///
    ///   Only those kinds. Enums emit no C++ symbol at all, buses emit
    ///   only `<Bus>_<chan>_payload`, and regblocks/addrmaps emit
    ///   `<Name>_Mirror` and friends — none claims the bare name, so
    ///   listing them would reject working programs on a false premise.
    ///   An earlier revision did list them; the branch below records it.
    /// * **The backend always emits the name.** `RESERVED_EMITTED_NAMES`.
    ///   Nothing in the user's program declares these, so no amount of
    ///   inspecting its items will find them.
    /// * **More than one enum declares it.** No single index is right:
    ///   `enum RdResp { OKAY, SLVERR }` and `enum WrResp { SLVERR, OKAY }`
    ///   disagree about both names. The `enum_variants` map resolves this
    ///   first-wins because v0 assumes variant names are globally unique
    ///   (see its construction), which is tolerable for a solver token but
    ///   not for a value: `let w : WrResp = OKAY` would compile to 0.
    ///
    /// A name repeated inside ONE enum is NOT here — that is a malformed
    /// enum rather than an ambiguity between two, so an index is still
    /// well defined and the emission loop dedups it instead.
    ///
    /// A `const` of the same name is NOT here either: that one has a
    /// defined answer (the `const` wins, as in constraint lowering), so it
    /// is skipped silently by the caller rather than diagnosed.
    pub(super) fn collect_unsafe_enum_variants(
        &self,
        file: &SourceFile,
        const_names: &std::collections::HashSet<String>,
    ) -> std::collections::HashMap<String, String> {
        // How many enums declare each variant name, and one owning enum to
        // name in the message.
        let mut decl_count: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        let mut first_owner: std::collections::HashMap<&str, &str> =
            std::collections::HashMap::new();
        for it in &file.items {
            if let Item::Enum(e) = it {
                // A variant repeated WITHIN one enum counts once — that is
                // a malformed enum, not an ambiguity between two of them,
                // and it is not this function's error to report.
                let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
                for v in &e.variants {
                    if !seen.insert(v.name.as_str()) {
                        continue;
                    }
                    *decl_count.entry(v.name.as_str()).or_insert(0) += 1;
                    first_owner.entry(v.name.as_str()).or_insert(&e.name.name);
                }
            }
        }

        let extern_fns: std::collections::HashSet<&str> = file
            .items
            .iter()
            .filter_map(|it| match it {
                Item::ExternFn(f) => Some(f.name.name.as_str()),
                _ => None,
            })
            .collect();

        let mut out = std::collections::HashMap::new();
        for (name, count) in decl_count {
            if const_names.contains(name) {
                continue;
            }
            let reason = if count > 1 {
                Some(format!(
                    "it is declared by {count} different enums, so no single \
                     index is correct for a bare `{name}`. HARC has no \
                     qualified `Enum.VARIANT` form, so rename one of them"
                ))
            } else if Self::RESERVED_EMITTED_NAMES.contains(&name) {
                // A symbol the backend writes into every file, so no
                // declaration in the user's program marks the clash. `main`
                // is the one a real program hits — `enum Phase { setup,
                // main, teardown }` is ordinary naming — and a file-scope
                // `constexpr main` is rejected outright by C++.
                Some(format!(
                    "the generated C++ always defines `{name}` itself, so a \
                     variant of that name cannot also be defined. Rename the \
                     variant, or build with the default backend, which \
                     substitutes the index at the use site and is unaffected"
                ))
            } else if self.is_record_type(name)
                || self.scoreboards.contains(name)
                || self.components.contains_key(name)
                || self.transactors.contains_key(name)
                || self.covergroups.contains_key(name)
            {
                // Exactly the declarations that emit a `struct <Name>`; see
                // the three `struct {}` emission sites. Enums emit no C++
                // symbol at all, buses emit none of their own name, and
                // regblocks/addrmaps emit `<Name>_Mirror` — none of those
                // claims the bare name, so listing them here would reject
                // working programs while telling the reader something untrue
                // about why.
                Some(format!(
                    "a type of the same name is declared in this program, and \
                     the C++ this backend emits gives both the one identifier \
                     `{name}`. Rename the variant or the type, or build with \
                     the default backend, which substitutes the index at the \
                     use site and is unaffected"
                ))
            } else if extern_fns.contains(name) {
                Some(format!(
                    "an `extern function {name}` is declared in this program, \
                     and the C++ this backend emits gives both the one \
                     identifier `{name}`. Rename the variant or the function, \
                     or build with the default backend, which substitutes the \
                     index at the use site and is unaffected"
                ))
            } else {
                None
            };
            if let Some(reason) = reason {
                let owner = first_owner.get(name).copied().unwrap_or("");
                out.insert(name.to_string(), format!("`{owner}.{name}`: {reason}"));
            }
        }
        out
    }
}
