//! Legacy C++ rendering seam used by the TBIR randomization planner.
//!
//! Planning, runtime-table selection, and cell validation live in
//! `codegen::randomize`. This module owns the focused legacy emitter setup and
//! the two randomization rendering entrypoints while the lower-level C++
//! expression and Z3 primitives are incrementally retired from `native`.

use super::*;

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
