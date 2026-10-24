//! Native C++ expression and lvalue emission.

use super::*;

impl Emitter {
    /// Lvalue side of `<-` / `=` — `dut.x` becomes `dut->x`.
    pub(super) fn emit_lvalue(&mut self, e: &Expr) {
        self.emit_expr_with_arrow(e, true);
    }

    pub(super) fn emit_expr(&mut self, e: &Expr) {
        self.emit_expr_with_arrow(e, false);
    }
    fn emit_expr_with_arrow(&mut self, e: &Expr, lvalue: bool) {
        // Property-time substitutions for temporal SystemCall expressions.
        // `emit_property_check` populates `prop_subs` with source site → snippet for
        // each `$past`/`$rose`/`$fell`/`$stable` occurrence; if we encounter
        // one here, emit the snippet instead of recursing into its inner.
        if !self.prop_subs.is_empty() {
            if let Some(s) = self
                .prop_subs
                .get(&SourceSite::new(self.current_source_id, e.span))
            {
                write!(self.out, "{s}").ok();
                return;
            }
        }
        match &*e.kind {
            ExprKind::Int(s) => {
                write!(self.out, "{}", c_value_literal(s)).ok();
            }
            ExprKind::Float(s) => {
                write!(self.out, "{s}").ok();
            }
            ExprKind::Time(s) => {
                // Strip the unit suffix and emit the numeric portion.
                let n: String = s
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '_')
                    .collect();
                write!(self.out, "{n}").ok();
            }
            ExprKind::String(s) => {
                write!(self.out, "{}", string_value_literal_cpp(s)).ok();
            }
            ExprKind::Bool(b) => {
                write!(self.out, "{}", if *b { "true" } else { "false" }).ok();
            }
            ExprKind::Ident(id) => {
                // Monitor-body bare-name rewrite — see field_subs.
                if let Some(s) = self.field_subs.get(&id.name) {
                    write!(self.out, "{s}").ok();
                } else {
                    // An enum variant that could not be given a file-scope
                    // definition (`unsafe_variants`). Writing the name would
                    // emit C++ naming a type or an ambiguous symbol; the
                    // diagnostic lands HERE rather than at the enum
                    // declaration so that merely declaring the clash stays
                    // legal and constraint position, which resolves variants
                    // through `enum_variants` instead, keeps working.
                    if !self.declared_value_names.contains(&id.name) {
                        if let Some(reason) = self.unsafe_variants.get(&id.name) {
                            let msg = format!("enum variant {reason}");
                            if !self.errors.contains(&msg) {
                                self.errors.push(msg);
                            }
                        }
                    }
                    if !self.is_shadowed_value(&id.name) && self.consts.contains_key(&id.name) {
                        write!(self.out, "{}", global_const_cpp_name(&id.name)).ok();
                    } else if !self.is_shadowed_value(&id.name)
                        && (self.function_names.contains(&id.name)
                            || self.tseq_names.contains(&id.name))
                    {
                        write!(self.out, "{}", global_callable_cpp_name(&id.name)).ok();
                    } else if !self.is_shadowed_value(&id.name)
                        && self.extern_function_names.contains(&id.name)
                    {
                        write!(self.out, "::{}", id.name).ok();
                    } else if !self.is_shadowed_value(&id.name)
                        && self.enum_variants.contains_key(&id.name)
                        && !self.unsafe_variants.contains_key(&id.name)
                    {
                        write!(self.out, "{}", global_enum_variant_cpp_name(&id.name)).ok();
                    } else if !self.is_shadowed_value(&id.name) && id.name == "errors" {
                        let runtime_context = self.randomize_context_ref().to_string();
                        write!(self.out, "{runtime_context}.errors").ok();
                    } else {
                        write!(self.out, "{}", id.name).ok();
                    }
                }
            }
            ExprKind::ImplicitSelf => {}
            ExprKind::Field { target, name } => {
                if let ExprKind::Ident(root) = &*target.kind {
                    if self.is_shadowed_probe_receiver(&root.name)
                        && (self.probes.contains_key(&name.name)
                            || self.partial_probe_names.contains(&name.name))
                    {
                        write!(self.out, "{}.{}", root.name, name.name).ok();
                        return;
                    }
                }
                // RAL addrmap subfield read: `chip.inst.REG.FIELD`.
                // Mirror path is `chip.inst.REG`; offset is base+reg_off.
                if !lvalue {
                    if let Some((
                        _chip_var,
                        inst_path,
                        reg_name,
                        helper_var,
                        helper_ty,
                        effective_off,
                        _reg_c_type,
                        bit_pos,
                        bit_width,
                        access,
                    )) = self.resolve_addrmap_subfield_lookup(e)
                    {
                        let mask = field_mask_literal(bit_width);
                        if access.reads_from_bus() {
                            write!(
                                self.out,
                                "((({inst_path}.{reg_name} = {helper_ty}_read({helper_var}, {effective_off})) >> {bit_pos}) & 0x{mask:x}u)",
                            ).ok();
                        } else {
                            write!(
                                self.out,
                                "(({inst_path}.{reg_name} >> {bit_pos}) & 0x{mask:x}u)",
                            )
                            .ok();
                        }
                        return;
                    }
                }
                // RAL addrmap register read: `chip.inst.REG`.
                if !lvalue {
                    if let Some((
                        _chip_var,
                        inst_path,
                        helper_var,
                        helper_ty,
                        effective_off,
                        access,
                    )) = self.resolve_addrmap_register_lookup(e)
                    {
                        if access.reads_from_bus() {
                            write!(
                                self.out,
                                "({inst_path}.{} = {helper_ty}_read({helper_var}, {effective_off}))",
                                name.name,
                            ).ok();
                        } else {
                            write!(self.out, "{inst_path}.{}", name.name).ok();
                        }
                        return;
                    }
                }
                // RAL frontdoor field-level read: `regs.REG.FIELD`.
                // For RW/RO: `((regs.REG = <H>_read(helper, OFFSET)) >> POS) & MASK`
                // — bus read updates the mirror (read-side predict) AND
                // returns the value, then bit-extract.
                // For WO: `((regs.REG >> POS) & MASK)` — the bus would
                // return garbage on a WO register, so we serve from the
                // mirror.
                if !lvalue {
                    if let Some((
                        regs_var,
                        helper_var,
                        helper_ty,
                        offset_lit,
                        reg_name,
                        _reg_c_type,
                        bit_pos,
                        bit_width,
                        access,
                    )) = self.resolve_regblock_subfield_lookup(e)
                    {
                        let mask = field_mask_literal(bit_width);
                        if access.reads_from_bus() {
                            write!(
                                self.out,
                                "((({regs_var}.{reg_name} = {helper_ty}_read({helper_var}, {offset_lit})) >> {bit_pos}) & 0x{mask:x}u)",
                            ).ok();
                        } else {
                            // WO — serve from mirror only.
                            write!(
                                self.out,
                                "(({regs_var}.{reg_name} >> {bit_pos}) & 0x{mask:x}u)",
                            )
                            .ok();
                        }
                        return;
                    }
                }
                // RAL frontdoor register-level read: `regs.NAME` rvalue.
                // RW/RO: `(regs.NAME = <H>_read(helper, OFFSET))` —
                // assignment-expression form so the mirror updates AND
                // the expression yields the value. WO: serve from
                // mirror only.
                if !lvalue {
                    if let Some((regs_var, helper_var, helper_ty, offset_lit, access)) =
                        self.resolve_regblock_field_lookup(e)
                    {
                        // The `name` of the outer Field expr is the
                        // register name itself.
                        if access.reads_from_bus() {
                            write!(
                                self.out,
                                "({regs_var}.{} = {helper_ty}_read({helper_var}, {offset_lit}))",
                                name.name,
                            )
                            .ok();
                        } else {
                            write!(self.out, "{regs_var}.{}", name.name).ok();
                        }
                        return;
                    }
                }
                // Probe lowering. `dut.<name>` where <name> was declared
                // as a `probe` on the `let dut : T` decl resolves to
                // `dut->rootp-><mangled>`, not the (non-existent) top-
                // level `dut-><name>`. Reads still wrap with harc_read
                // so wide signals downcast cleanly. See
                // docs/probe-signals.md.
                if let ExprKind::Ident(id) = &*target.kind {
                    if self.is_active_probe_receiver(&id.name) {
                        if self.partial_probe_names.contains(&name.name) {
                            self.errors.push(format!(
                                "shared DUT access `{}.{}` names a probe that is not declared identically by every test in the suite",
                                id.name, name.name
                            ));
                            write!(self.out, "0").ok();
                            return;
                        }
                        if let Some(probe) = self.probes.get(&name.name).cloned() {
                            // Reads always come from the read-side
                            // accessor regardless of `force`. Writes
                            // are handled elsewhere (emit_signal_
                            // assignment) since they expand to a
                            // two-statement drv+en pair.
                            if lvalue {
                                write!(self.out, "dut->rootp->{}", probe.read).ok();
                            } else {
                                write!(self.out, "harc_rt::harc_read(dut->rootp->{})", probe.read)
                                    .ok();
                            }
                            return;
                        }
                    }
                }
                // Bus-bound binding lowering. If `target` is an Ident
                // bound to a bus (`let axil : BusAxiLite = bind dut`),
                // resolve `axil.signal` to `<root>-><axil>_<signal>`.
                // Two-level form (`axil.aw.addr`) walks into the bus's
                // handshake_channel groupings and emits
                // `<root>-><axil>_aw_addr`.
                if let Some(s) = self.try_emit_bus_field_access(target, name) {
                    if lvalue {
                        write!(self.out, "{s}").ok();
                    } else {
                        // Wrap reads with harc_rt::harc_read so wide
                        // signals (Verilator's VlWide<N> for >64-bit
                        // ports) implicitly convert to uint64_t. For
                        // narrow signals it's a no-op cast.
                        write!(self.out, "harc_rt::harc_read({s})").ok();
                    }
                    return;
                }
                // Pointer-typed identifiers (DUTs, threaded as Named-type
                // params) use `->`. Everything else uses `.`. Tracked in
                // `pointer_vars` — populated from `let x : <NamedType>` and
                // function params with a Named type (see emit_function).
                let is_pointer_root = matches!(&*target.kind,
                    ExprKind::Ident(id) if self.pointer_vars.contains(&id.name)
                );
                let wrap_read = is_pointer_root && !lvalue;
                if wrap_read {
                    write!(self.out, "harc_rt::harc_read(").ok();
                }
                self.emit_expr(target);
                if is_pointer_root {
                    write!(self.out, "->").ok();
                } else {
                    write!(self.out, ".").ok();
                }
                write!(self.out, "{}", name.name).ok();
                if wrap_read {
                    write!(self.out, ")").ok();
                }
            }
            ExprKind::Index { target, index } => {
                // Packed DUT lane/bit read: `dut.<port>[i]`. A raw C++
                // subscript does not compile for either Verilator packed
                // scalars or ARCH native scalar ports; route through
                // `harc_vec_lane_read<W>` (bit-extract on a packed value,
                // direct index on a true array).
                // Only fires for ports recorded in `vec_lane_widths`
                // (built from the `--sv` port table) — genuine
                // unpacked-`Vec` ports and wide-signal `VlWide` word
                // indexing fall through to the legacy subscript below.
                // Read (R-value) only: `harc_vec_lane_read` is not an
                // l-value, so the write side is handled separately in
                // `emit_signal_assignment` (via `harc_vec_lane_write`).
                // Guarding on `!lvalue` keeps any l-value request on the
                // legacy subscript path rather than emitting a
                // non-assignable call.
                if !lvalue {
                    if let Some((root, port, w, idx)) = self.dut_packed_lane(e) {
                        if w > 64 {
                            self.errors.push(format!(
                                "DUT access `{root}.{port}` indexes a packed {w}-bit lane; packed lane helpers support at most 64 bits"
                            ));
                            write!(self.out, "0").ok();
                            return;
                        }
                        write!(
                            self.out,
                            "harc_rt::harc_vec_lane_read<{w}>({root}->{port}, (std::size_t)("
                        )
                        .ok();
                        self.emit_expr(idx);
                        write!(self.out, "))").ok();
                        return;
                    }
                }
                // Pass `lvalue=true` to suppress the `harc_rt::harc_read`
                // wrap on a pointer-rooted Field target. Indexing a
                // wide signal (VlWide<N>) needs the raw `dut->field`
                // expression so VlWide's `operator[]` is reachable;
                // wrapping with `harc_read` would yield a `uint64_t`
                // which can't be indexed. Narrow signals shouldn't be
                // indexed in the first place — that path errors at
                // C++ compile, which is the desired outcome.
                self.emit_expr_with_arrow(target, /*lvalue*/ true);
                write!(self.out, "[").ok();
                self.emit_expr(index);
                write!(self.out, "]").ok();
            }
            ExprKind::BitSlice { target, hi, lo } => {
                write!(self.out, "harc_rt::harc_bits(").ok();
                self.emit_expr(target);
                write!(self.out, ", (uint32_t)(").ok();
                self.emit_expr(hi);
                write!(self.out, "), (uint32_t)(").ok();
                self.emit_expr(lo);
                write!(self.out, "))").ok();
            }
            ExprKind::Call { callee, args } => {
                if args.is_empty() {
                    if let ExprKind::Field { target, name } = &*callee.kind {
                        if name.name == "len" {
                            self.emit_expr(target);
                            write!(self.out, ".size()").ok();
                            return;
                        }
                    }
                }
                // RAL passive read: `regs.record_read(addr)` — decode the
                // address to the mirror cell with no bus traffic. Lowers
                // to the generated `<Regblock>_record_read` free function.
                if args.len() == 1 {
                    if let ExprKind::Field { target, name } = &*callee.kind {
                        if name.name == "record_read" {
                            if let ExprKind::Ident(regs_id) = &*target.kind {
                                if let Some(regs_ty) = self.let_types.get(&regs_id.name).cloned() {
                                    if self.regblocks.contains_key(&regs_ty) {
                                        let arg = match &args[0] {
                                            CallArg::Expr(ex) => ex,
                                            CallArg::Named { value, .. } => value,
                                        };
                                        write!(
                                            self.out,
                                            "{regs_ty}_record_read({}, (uint64_t)(",
                                            regs_id.name,
                                        )
                                        .ok();
                                        self.emit_expr(arg);
                                        write!(self.out, "))").ok();
                                        return;
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some((comp_ty, receiver, method, current_method, current_method_active)) =
                    self.resolve_bare_sibling_method_call(callee)
                {
                    if self.transactors.contains_key(&comp_ty)
                        && self.method_lives_in_when_active(&comp_ty, &method)
                        && !current_method_active
                    {
                        self.errors.push(format!(
                            "method `{comp_ty}.{current_method}(...)`: bare sibling call `{method}(...)` targets a hookable inside `when active` of transactor `{comp_ty}`. Move `{current_method}` into `when active`, or call `{method}` only from active-only code."
                        ));
                        write!(self.out, "/* unsupported bare active sibling call */ 0").ok();
                        return;
                    }
                    write!(self.out, "{comp_ty}_{method}({receiver}").ok();
                    for a in args.iter() {
                        write!(self.out, ", ").ok();
                        match a {
                            CallArg::Expr(ex) => self.emit_expr(ex),
                            CallArg::Named { value, .. } => self.emit_expr(value),
                        }
                    }
                    write!(self.out, ")").ok();
                    return;
                }
                // Env/test-level quiescence helper: `env.quiesced(N)`
                // aggregates the built-in `idle(N)` predicate over all
                // nested component fields registered under that component.
                // Timed `wait until` expands the same helper into per-leaf
                // predicates so timeout diagnostics name the blocking
                // sub-component.
                if let Some((instances, n_expr)) =
                    self.resolve_component_quiesced_predicate(callee, args)
                {
                    if instances.is_empty() {
                        write!(self.out, "true").ok();
                    } else if instances.len() == 1 {
                        self.emit_idle_predicate(&instances[0], "idle", n_expr);
                    } else {
                        for (i, instance) in instances.iter().enumerate() {
                            if i > 0 {
                                write!(self.out, " && ").ok();
                            }
                            write!(self.out, "(").ok();
                            self.emit_idle_predicate(instance, "idle", n_expr);
                            write!(self.out, ")").ok();
                        }
                    }
                    return;
                }
                // Built-in activity-tracking predicates: `obj.idle(N)`,
                // `obj.idle_in(N)`, `obj.idle_out(N)` lower directly to
                // arithmetic on the auto-injected heartbeat fields.
                // Recognized BEFORE hookable method dispatch so a user
                // can't shadow them. (Spec §7.x — activity tracking.)
                if let Some((instance, kind)) = self.resolve_component_idle_predicate(callee) {
                    if args.len() != 1 {
                        self.errors.push(format!(
                            "{instance}.{kind}: expected 1 cycle-count arg, got {}",
                            args.len(),
                        ));
                        write!(self.out, "false").ok();
                        return;
                    }
                    let n_expr = match &args[0] {
                        CallArg::Expr(e) => e,
                        CallArg::Named { value, .. } => value,
                    };
                    self.emit_idle_predicate(&instance, &kind, n_expr);
                    return;
                }
                // Width-method intrinsics: `.trunc<N>()` / `.zext<N>()` /
                // `.sext<N>()` / `.resize<N>()`. Mirrors arch-com's
                // sim_codegen lowering (src/sim_codegen/mod.rs:2688).
                // Parser emits these as `Call { callee: Field{recv, name},
                // args: [width_expr] }`. The width_expr resolves to a
                // const integer at emit time (literal or const path).
                if self.try_emit_width_method(callee, args) {
                    return;
                }

                // Method-call rewrite: `obj.method(args)` where `obj`'s
                // type is a known component with a `hookable method`
                // lowers to `<Type>_method(obj, args)`. Falls through to
                // the generic call shape when no method match is found,
                // so plain free-function calls keep working.
                if let Some((comp_ty, instance, method)) =
                    self.resolve_component_method_call(callee)
                {
                    // Passive-mode call-site check (spec §8.1):
                    // a hookable declared inside `T.when_active` is
                    // structurally absent from passive instances (the
                    // `when_active` block is elided by
                    // `synth_component_from_transactor` for passive).
                    // Today both halves still emit as free C++ functions
                    // — the actor coroutine is the only thing the
                    // passive mode suppresses — so a `passive_instance
                    // .when_active_method(...)` call would otherwise
                    // silently dispatch into orphan code. Surface it.
                    if self.method_lives_in_when_active(&comp_ty, &method) {
                        let path = Self::extract_method_call_path(callee)
                            .unwrap_or_else(|| vec![instance.clone()]);
                        if let Some(TransactorMode::Passive) = self.resolve_path_mode(&path) {
                            self.errors.push(format!(
                                "method `{}.{}(...)`: hookable `{}` lives inside `when active` of transactor `{}`, \
                                 so it does not exist on a passive instance. Change the let-binding to `{} active`, \
                                 or remove the call. See spec §8.1.",
                                instance, method, method, comp_ty, comp_ty,
                            ));
                            return;
                        }
                    }
                    // Prefix only from capsules, and only for callables
                    // that actually became glue members: inside common-TU
                    // member bodies the callee resolves as a sibling
                    // member, and a capsule-local (bus-bound) hookable is
                    // a plain lambda in this same scope.
                    let glue = if self.flavor == Flavor::Capsule
                        && self.is_glue_member(&format!("{comp_ty}_{method}"))
                    {
                        "_glue."
                    } else {
                        ""
                    };
                    write!(self.out, "{glue}{comp_ty}_{method}({instance}").ok();
                    for a in args.iter() {
                        write!(self.out, ", ").ok();
                        match a {
                            CallArg::Expr(ex) => self.emit_expr(ex),
                            CallArg::Named { value, .. } => self.emit_expr(value),
                        }
                    }
                    write!(self.out, ")").ok();
                    return;
                }
                // Common layout: suite-level helpers (functions,
                // tseqs) are members of the per-run glue object; a
                // plain-name call to one resolves through `_glue.`.
                if self.flavor == Flavor::Capsule {
                    if let ExprKind::Ident(id) = &*callee.kind {
                        if self.glue_callees.contains(&id.name) {
                            let callable = if self.function_names.contains(&id.name)
                                || self.tseq_names.contains(&id.name)
                            {
                                global_callable_cpp_name(&id.name)
                            } else {
                                id.name.clone()
                            };
                            write!(self.out, "_glue.{callable}").ok();
                            write!(self.out, "(").ok();
                            for (i, a) in args.iter().enumerate() {
                                if i > 0 {
                                    write!(self.out, ", ").ok();
                                }
                                match a {
                                    CallArg::Expr(ex) => self.emit_expr(ex),
                                    CallArg::Named { value, .. } => self.emit_expr(value),
                                }
                            }
                            write!(self.out, ")").ok();
                            return;
                        }
                    }
                }
                self.emit_expr(callee);
                write!(self.out, "(").ok();
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        write!(self.out, ", ").ok();
                    }
                    match a {
                        CallArg::Expr(ex) => self.emit_expr(ex),
                        CallArg::Named { value, .. } => self.emit_expr(value),
                    }
                }
                write!(self.out, ")").ok();
            }
            ExprKind::Cast { expr, ty } => {
                // Emit `((<c_type>)(<inner>))` when the target type
                // maps to a known C++ integer type — mirrors arch-com's
                // postfix `expr as Type` lowering. Width-widening
                // casts MATTER in C++: `1 << 31` against a `int`
                // literal is undefined behavior, while
                // `((uint64_t)1) << 31` is well-defined. Bool↔UInt
                // narrowing also flows through here, with C++'s
                // implicit conversions handling the no-op cases.
                //
                // For non-Builtin target types (struct, named type),
                // drop the cast and emit just the inner expression —
                // those are usually identity casts at the type-system
                // level that don't need a C++ representation.
                if matches!(ty, TypeExpr::Builtin { .. }) {
                    let cty = c_type_for(ty);
                    write!(self.out, "(({cty})(").ok();
                    self.emit_expr(expr);
                    write!(self.out, "))").ok();
                } else {
                    self.emit_expr(expr);
                }
            }
            ExprKind::Unary { op, expr } => {
                let s = match op {
                    UnaryOp::Neg => "-",
                    UnaryOp::Not => "!",
                    UnaryOp::NotKw => "!",
                    UnaryOp::BitNot => "~",
                };
                write!(self.out, "{s}").ok();
                self.emit_expr(expr);
            }
            ExprKind::Binary { op, lhs, rhs } => {
                // Wrapping arithmetic (`+% / -% / *%`, harc#473) masks the
                // result to `max(W(lhs), W(rhs))` bits. v1 used to treat
                // these as pass-through sugar for `+ - *`, so an
                // overflowing `uint<8>` add produced 300 where TB-IR
                // produced 44 — the same source, two answers.
                if matches!(
                    op,
                    BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap
                ) {
                    match self.wrap_result_width(*op, lhs, rhs) {
                        Ok(width) => {
                            self.emit_wrapping_binary(*op, lhs, rhs, width);
                            return;
                        }
                        Err(msg) => {
                            self.errors.push(msg);
                            return;
                        }
                    }
                }
                // Wide-literal == / != routing: when either operand is
                // a > 128-bit hex literal, route through
                // `harc_rt::harc_eq_words(sig, {w0, w1, ...})` so the
                // comparison happens word-by-word instead of through
                // `_harc_u128` (which would silently truncate the
                // literal to 128 bits and produce wrong matches).
                if matches!(op, BinaryOp::Eq | BinaryOp::Ne) {
                    let lhs_words = is_wide_int_literal(lhs);
                    let rhs_is_wide = is_wide_int_literal(rhs).is_some();
                    let words = lhs_words.or_else(|| is_wide_int_literal(rhs));
                    if let Some(words) = words {
                        // The signal side is whichever operand is not the
                        // wide literal. Prefer rhs if both happen to be
                        // (which won't normally happen — rejected by
                        // typechecking — but keeps codegen total).
                        let sig_side = if rhs_is_wide { lhs } else { rhs };
                        if matches!(op, BinaryOp::Ne) {
                            write!(self.out, "(!").ok();
                        }
                        write!(self.out, "harc_rt::harc_eq_words(").ok();
                        // The signal side: pass as L-value (no harc_read
                        // wrap) so the helper sees the raw VlWide<N>.
                        self.emit_expr_with_arrow(sig_side, /*lvalue*/ true);
                        write!(self.out, ", {{").ok();
                        for (i, w) in words.iter().enumerate() {
                            if i > 0 {
                                write!(self.out, ", ").ok();
                            }
                            write!(self.out, "{w}").ok();
                        }
                        write!(self.out, "}})").ok();
                        if matches!(op, BinaryOp::Ne) {
                            write!(self.out, ")").ok();
                        }
                        return;
                    }
                }
                // Shifts on wide scalar values operate at the HARC type's
                // declared width, not at the padded C++ carrier width. Use
                // the same runtime helpers as TB-IR so non-word-aligned
                // UInt values cannot retain padding bits and SInt right
                // shifts sign-fill from their declared sign bit.
                if matches!(op, BinaryOp::Shl | BinaryOp::Shr) {
                    if let Some(width) = self.infer_shift_width_best_effort(lhs).filter(|w| *w > 64)
                    {
                        let signed = self.infer_expr_signed_best_effort(lhs);
                        match (*op, width, signed) {
                            (BinaryOp::Shl, 129.., _) => {
                                write!(self.out, "harc_rt::harc_wide_mask_bits(((").ok();
                                self.emit_expr(lhs);
                                write!(self.out, ") << (").ok();
                                self.emit_expr(rhs);
                                write!(self.out, ")), {width})").ok();
                            }
                            (BinaryOp::Shl, _, _) => {
                                write!(self.out, "harc_rt::harc_shl_u128((_harc_u128)(").ok();
                                self.emit_expr(lhs);
                                write!(self.out, "), (uint64_t)(").ok();
                                self.emit_expr(rhs);
                                write!(self.out, "), {width})").ok();
                            }
                            (BinaryOp::Shr, 129.., true) => {
                                write!(self.out, "harc_rt::harc_wide_ashr((").ok();
                                self.emit_expr(lhs);
                                write!(self.out, "), (uint64_t)(").ok();
                                self.emit_expr(rhs);
                                write!(self.out, "), {width})").ok();
                            }
                            (BinaryOp::Shr, 129.., false) => {
                                write!(self.out, "harc_rt::harc_wide_mask_bits(((").ok();
                                self.emit_expr(lhs);
                                write!(self.out, ") >> (").ok();
                                self.emit_expr(rhs);
                                write!(self.out, ")), {width})").ok();
                            }
                            (BinaryOp::Shr, _, true) => {
                                write!(self.out, "harc_rt::harc_ashr_u128((_harc_u128)(").ok();
                                self.emit_expr(lhs);
                                write!(self.out, "), (uint64_t)(").ok();
                                self.emit_expr(rhs);
                                write!(self.out, "), {width})").ok();
                            }
                            (BinaryOp::Shr, _, false) => {
                                write!(self.out, "harc_rt::harc_shr_u128((_harc_u128)(").ok();
                                self.emit_expr(lhs);
                                write!(self.out, "), (uint64_t)(").ok();
                                self.emit_expr(rhs);
                                write!(self.out, "), {width})").ok();
                            }
                            _ => unreachable!(),
                        }
                        return;
                    }
                }
                // Wide signed carriers are stored as unsigned C++ values.
                // Route ordered comparisons through the width-aware runtime
                // helper so the declared sign bit, rather than carrier
                // magnitude, determines the result. Keep this gate narrow:
                // typed unsigned/mixed HarcWide pairs retain their existing
                // unsupported/native behavior.
                if matches!(
                    op,
                    BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
                ) {
                    if let Some(width) = self.signed_wide_compare_width(lhs, rhs) {
                        self.emit_signed_wide_compare(*op, lhs, rhs, width);
                        return;
                    }
                }
                self.emit_expr(lhs);
                let s = c_binary_op(*op);
                write!(self.out, " {s} ").ok();
                self.emit_expr(rhs);
            }
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => {
                // Wrap in parens so the ternary doesn't bind into a
                // surrounding higher-precedence operator on the C++
                // side (e.g. `a + (cond ? x : y)` lowers correctly).
                write!(self.out, "(").ok();
                self.emit_expr(cond);
                write!(self.out, " ? ").ok();
                self.emit_expr(then_branch);
                write!(self.out, " : ").ok();
                self.emit_expr(else_branch);
                write!(self.out, ")").ok();
            }
            ExprKind::Paren(inner) => {
                write!(self.out, "(").ok();
                self.emit_expr(inner);
                write!(self.out, ")").ok();
            }
            ExprKind::RangeLit { lo, hi } => {
                // Bare ranges outside of `for`/`in` aren't representable in C++.
                // Emit a comment so the user sees the issue without a hard fail.
                write!(self.out, "/* range ").ok();
                if let Some(l) = lo {
                    self.emit_expr(l);
                }
                write!(self.out, "..").ok();
                if let Some(h) = hi {
                    self.emit_expr(h);
                }
                write!(self.out, " */ 0").ok();
            }
            other => {
                self.errors.push(format!(
                    "expression not supported in v0 cpp_tb: {:?}",
                    std::mem::discriminant(other)
                ));
                write!(self.out, "/* unsupported */ 0").ok();
            }
        }
    }

    fn signed_wide_compare_width(&self, lhs: &Expr, rhs: &Expr) -> Option<u32> {
        let lhs_width = self.infer_expr_width_best_effort(lhs);
        let rhs_width = self.infer_expr_width_best_effort(rhs);
        let width = lhs_width.max(rhs_width)?;
        if width <= 64 {
            return None;
        }
        let lhs_signed = self.infer_expr_signed_best_effort(lhs);
        let rhs_signed = self.infer_expr_signed_best_effort(rhs);
        // Match the lowering gate: a strictly signed pair, or a strictly
        // signed operand with a widthless integer literal, is supported.
        // A typed signed/unsigned mix remains unsupported.
        (lhs_signed && rhs_signed).then_some(width)
    }

    fn emit_signed_wide_compare(&mut self, op: BinaryOp, lhs: &Expr, rhs: &Expr, width: u32) {
        let lhs_cpp = self.signed_wide_compare_operand(lhs, width);
        let rhs_cpp = self.signed_wide_compare_operand(rhs, width);
        let lt = |a: &str, b: &str| {
            if width > 128 {
                format!("harc_rt::harc_wide_slt({a}, {b}, {width})")
            } else {
                format!("harc_rt::harc_slt_u128((_harc_u128)({a}), (_harc_u128)({b}), {width})")
            }
        };
        let expr = match op {
            BinaryOp::Lt => lt(&lhs_cpp, &rhs_cpp),
            BinaryOp::Gt => lt(&rhs_cpp, &lhs_cpp),
            BinaryOp::Le => format!("(!{})", lt(&rhs_cpp, &lhs_cpp)),
            BinaryOp::Ge => format!("(!{})", lt(&lhs_cpp, &rhs_cpp)),
            _ => unreachable!(),
        };
        write!(self.out, "{expr}").ok();
    }

    fn signed_wide_compare_operand(&mut self, expr: &Expr, width: u32) -> String {
        let source_width = self.infer_expr_width_best_effort(expr);
        let mut rendered = String::new();
        let old = std::mem::take(&mut self.out);
        self.emit_expr(expr);
        rendered.push_str(&self.out);
        self.out = old;
        let Some(source_width) = source_width.filter(|w| *w < width) else {
            return rendered;
        };
        if width > 128 {
            let words = width.div_ceil(32);
            format!("harc_rt::harc_wide_sext<{words}>({rendered}, {source_width}, {width})")
        } else {
            format!("harc_rt::harc_sext_u128((_harc_u128)({rendered}), {source_width}, {width})")
        }
    }
}
