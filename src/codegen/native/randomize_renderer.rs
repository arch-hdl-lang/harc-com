//! Legacy C++ rendering seam used by the TBIR randomization planner.
//!
//! Planning, runtime-table selection, and cell validation live in
//! `codegen::randomize`. This module owns the focused legacy emitter setup and
//! the two randomization rendering entrypoints while the lower-level C++
//! expression and Z3 primitives are incrementally retired from `native`.

use super::*;

/// The per-element random draw `emit_field_random` writes inside a
/// `list<T>` loop, or `None` when this backend has none for `T` and
/// writes a bare `t->f[_i] = 0;` instead.
///
/// A `None` here is not merely a weak draw. The member is
/// `std::vector<T'>` for whatever `T'` `txn_field_c_type` picked, and
/// `= 0` only compiles when `T'` is a scalar — for
/// `list<Vec<uint<8>, 2>>` it is `std::array<uint64_t, 2>` and g++
/// rejects the assignment, for EVERY program that declares the record.
/// `record_leaf_fate` asks this rather than restating the arm.
fn list_elem_random_expr(elem: &TypeExpr) -> Option<String> {
    let TypeExpr::Builtin { args, .. } = elem else {
        return None;
    };
    match list_elem_random_kind(elem)? {
        ScalarLeafKind::Unsigned => Some(emit_random_unsigned_expr(
            type_arg_width(args).unwrap_or(32),
        )),
        ScalarLeafKind::Signed => {
            let w = type_arg_width(args).unwrap_or(32);
            Some(emit_random_signed_expr(w))
        }
        ScalarLeafKind::Bool => Some("harc_rt::random::harc_rng_range(harc_rng_next, 0, 1)".into()),
    }
}

fn build_randomize_emitter(file: &SourceFile, opts: &EmitOpts) -> Emitter {
    let file = &normalize_vec_record_elem_fields(file);
    // The tbir backend already desugars impl-for before lowering, so the
    // `file` handed here is classic-form; desugar again is idempotent and
    // keeps the problem-table spans aligned with v1.
    let file = desugar_impl_for_test_in_file(file);
    let file = &file;

    let typed_solver_problem_table =
        crate::solver::problem_table::build_typed_solver_problem_table(file);
    let mut runtime_randomize_problem_ids = std::collections::HashMap::new();
    for entry in &typed_solver_problem_table.entries {
        let crate::solver::problem_table::TypedSolverProblemSource::RandomizeSite {
            source_id,
            span,
            ..
        } = entry.source
        else {
            continue;
        };
        if let crate::solver::problem_table::TypedSolverProblemBuild::Z3 { typed, .. } =
            &entry.build
        {
            runtime_randomize_problem_ids
                .insert(SourceSite::new(source_id, span), typed.problem_id.0);
        }
    }

    // Record-field metadata: same collection the main emission path runs
    // (record_fields/record_bodies -> flatten_*_field_infos), restricted
    // to the inputs the constraint solver block consumes.
    let mut record_fields: std::collections::HashMap<String, Vec<Field>> =
        std::collections::HashMap::new();
    let mut enum_domains: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut enums: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut enum_variants: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    let const_elab = crate::constraints::elaborate_constraints(file);
    let consts: std::collections::HashMap<_, _> = const_elab.consts.into_iter().collect();
    let mut relations: std::collections::HashMap<String, RelationDecl> =
        std::collections::HashMap::new();
    let const_widths: std::collections::HashMap<String, u32> = consts
        .iter()
        .filter_map(|(name, constant)| {
            constant
                .width
                .or_else(|| literal_operand_bit_width(&constant.literal))
                .map(|width| (name.clone(), width))
        })
        .collect();
    for item in &file.items {
        match item {
            Item::Enum(decl) => {
                enums.insert(decl.name.name.clone(), decl.variants.len());
                enum_domains.insert(
                    decl.name.name.clone(),
                    decl.variants
                        .iter()
                        .map(|variant| variant.name.clone())
                        .collect(),
                );
                for (index, variant) in decl.variants.iter().enumerate() {
                    enum_variants
                        .entry(variant.name.clone())
                        .or_insert(index as i64);
                }
            }
            Item::Relation(relation) => {
                relations.insert(relation.name.name.clone(), relation.clone());
            }
            _ => {}
        }
    }
    for item in &file.items {
        match item {
            Item::Struct(record) => {
                record_fields.insert(record.name.name.clone(), record.fields.clone());
            }
            Item::Transaction(record) => {
                record_fields.insert(record.name.name.clone(), txn_direct_fields(&record.body));
            }
            _ => {}
        }
    }
    let mut txn_fields: std::collections::HashMap<String, Vec<TxnFieldInfo>> =
        std::collections::HashMap::new();
    for item in &file.items {
        match item {
            Item::Transaction(record) => {
                txn_fields.insert(
                    record.name.name.clone(),
                    flatten_txn_body_field_infos(
                        &record.body,
                        &record_fields,
                        &enums,
                        &enum_domains,
                    ),
                );
            }
            Item::Struct(record) => {
                txn_fields.insert(
                    record.name.name.clone(),
                    flatten_record_field_infos(
                        &record.fields,
                        &record_fields,
                        &enums,
                        &enum_domains,
                    ),
                );
            }
            _ => {}
        }
    }

    Emitter {
        out: String::new(),
        errors: Vec::new(),
        unsafe_variants: Default::default(),
        declared_value_names: Default::default(),
        txn_fields,
        record_fields,
        txn_keeps: std::collections::HashMap::new(),
        probes: std::collections::HashMap::new(),
        probe_widths: std::collections::HashMap::new(),
        partial_probe_names: std::collections::HashSet::new(),
        probe_receivers: std::collections::HashSet::new(),
        probe_shadow_scopes: Vec::new(),
        probe_dut_type: None,
        shadowed_lets: std::collections::HashSet::new(),
        regblocks: std::collections::HashMap::new(),
        addrmaps: std::collections::HashMap::new(),
        let_helper: std::collections::HashMap::new(),
        runtime_randomize_problem_ids,
        source_identities: stable_source_identities(file),
        randomize_site_owners: component_randomize_site_owners(file),
        current_source_id: SourceId::default(),
        randomize_target_sentinel: None,
        randomize_context_sentinel: None,
        randomize_state_sentinel: None,
        randomize_context_ident: "ctx".to_string(),
        capsule_state_ident: "_harc_randomize_state".to_string(),
        relations,
        pointer_vars: std::collections::HashSet::new(),
        let_types: std::collections::HashMap::new(),
        let_widths: std::collections::HashMap::new(),
        vec_lane_widths: opts.effective_vec_lane_widths(),
        let_modes: std::collections::HashMap::new(),
        transactions: std::collections::HashSet::new(),
        structs: std::collections::HashSet::new(),
        scoreboards: std::collections::HashSet::new(),
        covergroups: std::collections::HashMap::new(),
        covers: Vec::new(),
        field_subs: std::collections::HashMap::new(),
        enums,
        enum_variants,
        consts,
        const_widths,
        properties: std::collections::HashMap::new(),
        prop_subs: std::collections::HashMap::new(),
        event_types: std::collections::HashMap::new(),
        clock_names: Vec::new(),
        current_yield_target: None,
        current_unique_tseq_registry: None,
        function_names: std::collections::HashSet::new(),
        extern_function_names: std::collections::HashSet::new(),
        tseq_names: std::collections::HashSet::new(),
        components: std::collections::HashMap::new(),
        buses: std::collections::HashMap::new(),
        bus_bindings: std::collections::HashMap::new(),
        bus_param_envs: std::collections::HashMap::new(),
        dut_bus_port_overrides: opts.dut_bus_port_overrides.clone(),
        bus_remap: std::collections::HashMap::new(),
        pending_tlm_forks: Vec::new(),
        next_tlm_fork_tag: std::collections::HashMap::new(),
        in_coroutine: false,
        flavor: Flavor::Legacy,
        per_run_cells: Vec::new(),
        glue_callees: std::collections::HashSet::new(),
        actor_threads: Vec::new(),
        mt: opts.mt,
        driver_bus_for_hookables: std::collections::HashMap::new(),
        transactors: std::collections::HashMap::new(),
        current_component_instance: None,
        current_component_method: None,
    }
}

/// Narrow bridge to the retiring native emitter for randomization C++ text.
pub(crate) struct NativeRandomizeEmitter {
    emitter: Emitter,
}

impl NativeRandomizeEmitter {
    pub(crate) fn new(file: &SourceFile, opts: &EmitOpts) -> Self {
        Self {
            emitter: build_randomize_emitter(file, opts),
        }
    }

    pub(crate) fn configure_tbir_capsule(&mut self) {
        self.emitter.flavor = Flavor::Capsule;
        self.emitter.randomize_target_sentinel = Some(TBIR_RANDOMIZE_TARGET_SENTINEL);
        self.emitter.randomize_context_sentinel = Some(TBIR_RANDOMIZE_CONTEXT_SENTINEL);
        self.emitter.randomize_state_sentinel = Some(TBIR_RANDOMIZE_STATE_SENTINEL);
    }

    pub(crate) fn emit_site(
        &mut self,
        site: &crate::ir::ConstraintSite,
        depth: usize,
        rewrite_rng_for_tbir: bool,
    ) -> Result<(String, Vec<PerRunCell>), EmitError> {
        self.emitter.out.clear();
        self.emitter.errors.clear();
        self.emitter.current_source_id = site.source_id;
        let cell_start = self.emitter.per_run_cells.len();
        self.emitter.emit_randomize_for_site(site, depth);
        if let Some(error) = self.emitter.errors.first() {
            return Err(EmitError(format!(
                "tbir randomize({}): {error}",
                site.record
            )));
        }
        let mut snippet = std::mem::take(&mut self.emitter.out);
        if rewrite_rng_for_tbir {
            snippet = snippet.replace(
                "harc_rng_next()",
                &format!("{}.rng.next()", TBIR_RANDOMIZE_CONTEXT_SENTINEL),
            );
        }
        Ok((snippet, self.emitter.per_run_cells[cell_start..].to_vec()))
    }

    pub(crate) fn all_cells(&self) -> &[PerRunCell] {
        &self.emitter.per_run_cells
    }

    pub(crate) fn begin_record_helpers(&mut self) {
        self.emitter.out.clear();
        self.emitter.errors.clear();
    }

    pub(crate) fn emit_record_helper(&mut self, item: &Item) -> Result<(), EmitError> {
        match item {
            Item::Struct(record) => self
                .emitter
                .emit_record_randomize_fn(&record.name.name, &record.fields),
            Item::Transaction(record) => {
                let fields = txn_all_fields(&record.body);
                self.emitter
                    .emit_record_randomize_fn(&record.name.name, &fields);
            }
            _ => unreachable!("record helper item must be a struct or transaction"),
        }
        if let Some(error) = self.emitter.errors.first() {
            return Err(EmitError(format!("tbir record randomize helper: {error}")));
        }
        Ok(())
    }

    pub(crate) fn finish_record_helpers(self) -> String {
        self.emitter.out
    }
}

impl Emitter {
    /// Emit one TB-IR `ConstraintSite` through the legacy randomize renderer.
    fn emit_randomize_for_site(&mut self, site: &crate::ir::ConstraintSite, depth: usize) {
        let ty = site.record.clone();
        let target = &site.target;
        let combined = &site.constraints;
        self.report_runtime_dependent_randomize_field_attrs(&ty);
        let has_solver_policy_fields = self.txn_fields.get(&ty).is_some_and(|fields| {
            fields
                .iter()
                .any(|field| field_attr_unique(field) || !auto_coverage_values(field).is_empty())
        });
        if combined.is_empty() && !has_solver_policy_fields {
            if !self.emit_runtime_unconstrained_randomize_call(&ty, target, depth) {
                self.pad(depth);
                write!(self.out, "randomize_{ty}(&").ok();
                self.emit_randomize_target(target);
                writeln!(self.out, ", harc_rng);").ok();
            }
            self.emit_randomize_trace_event(&ty, target, depth);
        } else {
            self.emit_constraint_solver_block(&ty, target, combined, site.blocking, depth);
        }
    }

    pub(super) fn emit_randomize_target(&mut self, target: &Expr) {
        if let Some(sentinel) = self.randomize_target_sentinel {
            self.out.push_str(sentinel);
        } else {
            self.emit_expr(target);
        }
    }

    pub(super) fn runtime_randomize_problem_id(&self, target: &Expr) -> Option<u64> {
        self.runtime_randomize_problem_ids
            .get(&SourceSite::new(self.current_source_id, target.span))
            .copied()
    }

    pub(super) fn randomize_site_identity(
        &self,
        target: &Expr,
        runtime_problem_id: Option<u64>,
    ) -> (u64, String) {
        if let Some(problem_id) = runtime_problem_id {
            return (problem_id, format!("_solver_site_{problem_id}"));
        }
        let source = self
            .source_identities
            .get(&self.current_source_id)
            .copied()
            .unwrap_or_else(|| {
                let fallback = [b"<unknown>".as_slice(), b"".as_slice()];
                StableSourceIdentity {
                    tag: stable_identity_hash("harc-source-tag-v2", &fallback),
                    site_seed: stable_identity_hash("harc-source-site-v2", &fallback),
                }
            });
        let owner = self
            .randomize_site_owners
            .get(&SourceSite::new(self.current_source_id, target.span))
            .cloned()
            .or_else(|| {
                self.current_component_method
                    .as_ref()
                    .map(|(component, _, method, _)| format!("component {component}::{method}"))
            })
            .unwrap_or_else(|| "anonymous".to_string());
        let owner_tag = stable_identity_hash(
            "harc-component-randomize-owner-v1",
            &[&source.tag.to_le_bytes(), owner.as_bytes()],
        );
        (
            stable_component_randomize_site_id(source, &owner, target.span),
            format!(
                "_solver_site_h{:016x}_b{}_e{}",
                owner_tag, target.span.start, target.span.end
            ),
        )
    }

    pub(super) fn randomize_global_seed_ref(&self) -> String {
        format!("{}.rng.initial_seed", self.randomize_context_ref())
    }

    pub(super) fn component_call_site_ref(&mut self, site_id: u64, cache_tag: &str) -> String {
        let tag = format!("{cache_tag}_call_site");
        let init = format!("{{{site_id}ULL, {site_id}ULL, 0}}");
        self.per_run_cell(tag, "harc_rt::random::HarcRuntimeCallSite", Some(&init))
    }

    pub(super) fn runtime_problem_call_site_ref(
        &mut self,
        site_id: u64,
        cache_tag: &str,
    ) -> String {
        if let Some(state) = self.randomize_state_sentinel {
            format!("{state}.call_site")
        } else if self.flavor == Flavor::Legacy {
            self.randomize_context_ref().to_string()
        } else {
            self.component_call_site_ref(site_id, cache_tag)
        }
    }

    pub(super) fn emit_runtime_unconstrained_randomize_call(
        &mut self,
        ty: &str,
        target: &Expr,
        depth: usize,
    ) -> bool {
        let runtime_problem_id = self.runtime_randomize_problem_id(target);
        let (site_id, cache_tag) = self.randomize_site_identity(target, runtime_problem_id);
        let global_seed = self.randomize_global_seed_ref();
        if runtime_problem_id.is_none() {
            let call_site = self.component_call_site_ref(site_id, &cache_tag);
            self.pad(depth);
            writeln!(
                self.out,
                "{{   // site-stable runtime randomize scaffold for unconstrained record"
            )
            .ok();
            self.pad(depth + 1);
            writeln!(
                self.out,
                "auto _harc_rt_seed = harc_rt::random::harc_call_site_next_seed({call_site}, {global_seed});"
            )
            .ok();
            self.pad(depth + 1);
            writeln!(self.out, "harc_rt::random::HarcRng _harc_site_rng;").ok();
            self.pad(depth + 1);
            writeln!(self.out, "_harc_site_rng.state = _harc_rt_seed;").ok();
            self.pad(depth + 1);
            writeln!(self.out, "_harc_site_rng.initial_seed = _harc_rt_seed;").ok();
            self.pad(depth + 1);
            write!(self.out, "randomize_{ty}(&").ok();
            self.emit_randomize_target(target);
            writeln!(self.out, ", _harc_site_rng);").ok();
            self.pad(depth);
            writeln!(self.out, "}}").ok();
            return true;
        }
        let problem_id = site_id;
        let prepare_state = self.runtime_problem_call_site_ref(problem_id, &cache_tag);
        self.pad(depth);
        writeln!(
            self.out,
            "{{   // runtime queued randomize scaffold for unconstrained record"
        )
        .ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "auto _harc_rt_call = harc_prepare_randomize_call({prepare_state}, {problem_id}ULL, {global_seed}, 0);"
        )
        .ok();
        self.pad(depth + 1);
        writeln!(self.out, "auto _harc_rt_seed = _harc_rt_call.seed;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "harc_rt::random::HarcRng _harc_site_rng;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "_harc_site_rng.state = _harc_rt_seed;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "_harc_site_rng.initial_seed = _harc_rt_seed;").ok();
        self.pad(depth + 1);
        write!(
            self.out,
            "auto _harc_rt_status = harc_rt::random::harc_solve_queued("
        )
        .ok();
        self.emit_randomize_target(target);
        writeln!(
            self.out,
            ", _harc_rt_call.problem_id, _harc_rt_seed, [&](auto* _harc_target) {{ randomize_{ty}(_harc_target, _harc_site_rng); }});"
        )
        .ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "(void)harc_rt::random::harc_handle_solve_status(_harc_rt_status);"
        )
        .ok();
        self.pad(depth);
        writeln!(self.out, "}}").ok();
        true
    }

    pub(super) fn emit_randomize_trace_event(&mut self, ty: &str, target: &Expr, depth: usize) {
        let Some(fields) = self.txn_fields.get(ty).cloned() else {
            return;
        };
        self.pad(depth);
        writeln!(self.out, "if (trace.enabled) {{").ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "std::string _trace_fields = \"\\\"txn_type\\\":\\\"{}\\\",\\\"fields\\\":{{\";",
            escape_c(ty)
        )
        .ok();
        let mut emitted = 0usize;
        for f in &fields {
            if f.width == 0 && f.list.is_none() {
                continue;
            }
            self.pad(depth + 1);
            let prefix = if emitted == 0 {
                format!("\\\"{}\\\":", escape_c(&f.name))
            } else {
                format!(",\\\"{}\\\":", escape_c(&f.name))
            };
            emitted += 1;
            if f.list.is_some() {
                write!(
                    self.out,
                    "_trace_fields += \"{prefix}\" + std::to_string((unsigned long long)("
                )
                .ok();
                self.emit_randomize_target(target);
                writeln!(self.out, ".{}.size()));", f.name).ok();
            } else if f.width > 64 {
                write!(
                    self.out,
                    "_trace_fields += \"{prefix}\\\"0x\" + std::string((const char*)harc_rt::HarcHexBufWide("
                )
                .ok();
                self.emit_randomize_target(target);
                writeln!(
                    self.out,
                    ".{}, {}, false)) + \"\\\"\";",
                    f.name,
                    f.width.div_ceil(4)
                )
                .ok();
            } else {
                let cast = if f.signed {
                    "long long"
                } else {
                    "unsigned long long"
                };
                write!(
                    self.out,
                    "_trace_fields += \"{prefix}\" + std::to_string(({cast})("
                )
                .ok();
                self.emit_randomize_target(target);
                writeln!(self.out, ".{}));", f.name).ok();
            }
        }
        self.pad(depth + 1);
        writeln!(self.out, "_trace_fields += \"}}\";").ok();
        self.pad(depth + 1);
        writeln!(self.out, "trace.randomize(cycle_count, _trace_fields);").ok();
        self.pad(depth);
        writeln!(self.out, "}}").ok();
    }

    /// Emit one random-field assignment honouring `[range(...)]` and
    /// `[dist {...}]` attributes. Falls back to a uniform sample over the
    /// declared type's value range.
    fn emit_field_random(&mut self, f: &Field) {
        if let Some(info) = list_field_info(&f.ty) {
            let max_len = info.declared_max_len.unwrap_or(0);
            writeln!(
                self.out,
                "{INDENT}t->{}.resize((size_t)harc_rt::random::harc_rng_range(harc_rng_next, 0, {}));",
                f.name.name, max_len
            )
            .ok();
            writeln!(
                self.out,
                "{INDENT}for (size_t _i = 0; _i < t->{}.size(); ++_i) {{",
                f.name.name
            )
            .ok();
            let draw = list_elem_type(&f.ty)
                .and_then(list_elem_random_expr)
                .unwrap_or_else(|| "0".to_string());
            writeln!(self.out, "{INDENT}{INDENT}t->{}[_i] = {draw};", f.name.name).ok();
            writeln!(self.out, "{INDENT}}}").ok();
            return;
        }

        // Look for a `[range(lo, hi)]` or `[dist {...}]` attribute.
        let mut handled = false;
        for a in &f.attrs {
            match a.name.name.as_str() {
                "range" => {
                    if a.args.len() >= 2 {
                        if let (AttrArg::Expr(lo), AttrArg::Expr(hi)) = (&a.args[0], &a.args[1]) {
                            write!(
                                self.out,
                                "{INDENT}t->{} = harc_rt::random::harc_rng_range(harc_rng_next, ",
                                f.name.name
                            )
                            .ok();
                            self.emit_expr(lo);
                            write!(self.out, ", ").ok();
                            self.emit_expr(hi);
                            writeln!(self.out, ");").ok();
                            handled = true;
                        }
                    }
                }
                "dist" => {
                    let dist_args: Option<&Vec<DistEntry>> = a.args.iter().find_map(|x| match x {
                        AttrArg::Dist(d) => Some(d),
                        _ => None,
                    });
                    if let Some(entries) = dist_args {
                        write!(
                            self.out,
                            "{INDENT}t->{} = harc_rt::random::harc_rng_dist(harc_rng_next, {{",
                            f.name.name
                        )
                        .ok();
                        self.emit_rng_dist_entries(entries);
                        writeln!(self.out, "}});").ok();
                        handled = true;
                    }
                }
                _ => {}
            }
            if handled {
                break;
            }
        }
        if handled {
            return;
        }

        // Fallback: type-driven uniform sampling.
        match &f.ty {
            TypeExpr::Builtin { name, args, .. } => {
                let width = type_arg_width(args);
                match name {
                    BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits => {
                        let w = width.unwrap_or(32);
                        writeln!(
                            self.out,
                            "{INDENT}t->{} = {};",
                            f.name.name,
                            emit_random_unsigned_expr(w)
                        )
                        .ok();
                    }
                    BuiltinTy::SInt | BuiltinTy::SIntCap => {
                        let w = width.unwrap_or(32);
                        writeln!(
                            self.out,
                            "{INDENT}t->{} = {};",
                            f.name.name,
                            emit_random_signed_expr(w)
                        )
                        .ok();
                    }
                    BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => {
                        writeln!(
                            self.out,
                            "{INDENT}t->{} = harc_rt::random::harc_rng_range(harc_rng_next, 0, 1);",
                            f.name.name
                        )
                        .ok();
                    }
                    BuiltinTy::Int => {
                        writeln!(
                            self.out,
                            "{INDENT}t->{} = harc_rt::random::harc_rng_range(harc_rng_next, 0, 0x7FFFFFFF);",
                            f.name.name
                        )
                        .ok();
                    }
                    _ => {
                        writeln!(
                            self.out,
                            "{INDENT}// {} : <unsupported type for v0 randomize>",
                            f.name.name
                        )
                        .ok();
                    }
                }
            }
            TypeExpr::Named { name, .. } => {
                let last = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                if let Some(&n) = self.enums.get(last) {
                    let hi = if n == 0 { 0 } else { (n - 1) as i64 };
                    writeln!(
                        self.out,
                        "{INDENT}t->{} = harc_rt::random::harc_rng_range(harc_rng_next, 0, {});",
                        f.name.name, hi
                    )
                    .ok();
                } else if self.is_record_type(last) {
                    writeln!(
                        self.out,
                        "{INDENT}randomize_{last}(&t->{}, harc_rng);",
                        f.name.name
                    )
                    .ok();
                } else {
                    writeln!(
                        self.out,
                        "{INDENT}// {} : {} (named, not yet supported)",
                        f.name.name, last
                    )
                    .ok();
                }
            }
        }
    }

    pub(super) fn emit_rng_dist_entries(&mut self, entries: &[DistEntry]) {
        for (i, e) in entries.iter().enumerate() {
            if i > 0 {
                write!(self.out, ", ").ok();
            }
            // Each dist entry: (lo, hi, weight). The `value` expression is
            // either a RangeLit (use lo/hi) or a scalar (use as both lo/hi).
            match &*e.value.kind {
                ExprKind::RangeLit {
                    lo: Some(lo),
                    hi: Some(hi),
                } => {
                    write!(self.out, "{{(int64_t)(").ok();
                    self.emit_expr(lo);
                    write!(self.out, "), (int64_t)(").ok();
                    self.emit_expr(hi);
                    write!(self.out, "), (int64_t)(").ok();
                    self.emit_expr(&e.weight);
                    write!(self.out, ")}}").ok();
                }
                _ => {
                    write!(self.out, "{{(int64_t)(").ok();
                    self.emit_expr(&e.value);
                    write!(self.out, "), (int64_t)(").ok();
                    self.emit_expr(&e.value);
                    write!(self.out, "), (int64_t)(").ok();
                    self.emit_expr(&e.weight);
                    write!(self.out, ")}}").ok();
                }
            }
        }
    }

    pub(super) fn emit_record_randomize_fn(&mut self, name: &str, fields: &[Field]) {
        writeln!(
            self.out,
            "static void randomize_{name}({name}* t, harc_rt::random::HarcRng& harc_rng) {{"
        )
        .ok();
        writeln!(
            self.out,
            "{INDENT}auto harc_rng_next = [&]() {{ return harc_rng.next(); }};"
        )
        .ok();
        for field in fields {
            if field.non_random {
                if let Some(default) = &field.default {
                    write!(self.out, "{INDENT}t->{} = ", field.name.name).ok();
                    self.emit_expr(default);
                    writeln!(self.out, ";").ok();
                }
                continue;
            }
            self.emit_field_random(field);
        }
        writeln!(self.out, "}}").ok();
        writeln!(self.out).ok();
    }
}
