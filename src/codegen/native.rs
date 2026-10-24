//! Native C++ testbench emission and shared lowering support.
//!
//! Scope (tracking spec §14 "Phase 0" / pre-1a):
//! - One `test T` with one `scope sim` and one `run` block becomes a `main()`
//!   that drives a Verilated DUT.
//! - The conventional name `dut` (per spec §2) marks the DUT instance — the
//!   field-access expression `dut.signal` lowers to `dut->signal`, and
//!   `let dut : <Type>` becomes `V<Type>* dut = new V<Type>;`. The same
//!   pointer-shape rule applies to any `Named`-typed local or function
//!   parameter, not just the literal name `dut`.
//! - **Assignment operators** match ARCH's discipline:
//!     - `=` — blocking assignment (locals, raw DUT signal drives).
//!     - `<-` — channel send (event / buffer / state per spec §17.2; future).
//!     - `<=` — non-blocking assignment, future, valid only inside `seq` /
//!       `thread` blocks (parser disambiguation will mirror ARCH's
//!       `no_lteq` flag once those constructs land in codegen).
//!   Both `=` and `<-` lower to the same `dut->x = v;` C++ today.
//! - `after N cycles ... end after` lowers to `for (int _=0; _<N; _++) tick();`
//!   where `tick()` toggles the clock and calls `eval()` twice.
//! - `assert expr [else fail("msg")]` lowers to a runtime check that
//!   increments an `errors` counter; per-test failure reporting and exit
//!   code mirror the hand-written `examples/counter_tb.cpp` shape.
//! - `log(<severity>, "...")` lowers to a `sim_log_line(<SEV>, ...)` call
//!   that writes `[cycle:N <SEV>] <msg>` to both stdout and `sim.log`.
//!   Test-result semantics from §7.7 are honored:
//!     - `error` increments the `errors` counter (test fails at end of run).
//!     - `fatal` increments + sets `_fatal`, and the main simulation loop
//!       exits at end of the current cycle (this test instance aborts).
//!     - `warn` / `info` / `debug` have no test-result effect.
//!   Verbosity flags, component IDs, and per-component overrides are
//!   spec'd in §7.7 but deferred — the runtime currently prints all
//!   severities unconditionally.
//!
//! Out of scope here:
//! - `tseq`, randomization, properties, coverage groups, fork/join, env/
//!   agent/monitor/scoreboard composition, multi-clock domains.
//! - Type checking — we trust the user's HARC and let the C++ compiler
//!   surface signal-name typos against Verilator's generated header.
mod bound_driver_renderer;
mod bound_handler_renderer;
mod bus_access_renderer;
mod component_resolution;
mod component_renderer;
mod component_wiring_renderer;
mod constraint_solver_renderer;
mod covergroup_renderer;
mod expression_renderer;
mod file_scope_renderer;
mod let_renderer;
mod name_analysis;
mod property_renderer;
pub(crate) mod randomize_renderer;
mod randomize_validation;
mod record_layout;
mod record_renderer;
mod runtime_state_renderer;
mod signal_access_renderer;
mod statement_renderer;
mod tagged_target_responder_renderer;
mod target_responder_renderer;
mod test_run_renderer;
mod tlm_initiator_renderer;
mod value_types;
mod width_semantics;

use crate::ast::*;
use crate::codegen::common_artifacts::{self, CommonArtifactPlan};
pub use crate::codegen::dut_interface::{
    add_arch_scalar_bit_lanes, cosim_ports_from_sv, dut_bus_port_overrides_from_files,
    dut_interface_catalog, dut_interface_catalog_with_parameter_overrides,
    dut_port_widths_from_files, dut_port_widths_from_sv, dut_type_name, vec_lane_widths_from_sv,
    CosimOpts, CosimPort, CosimProbe,
};
#[cfg(test)]
use crate::codegen::dut_interface::{
    collect_port_overrides_from_src, collect_port_widths_from_src, scan_sv_module_lane_widths,
    scan_sv_module_ports,
};
pub use crate::codegen::emission::{EmitError, EmitOpts, GeneratedCppFile, SplitCppOutput};
use crate::codegen::lifecycle_transform::desugar_impl_for_test_in_file;
#[cfg(test)]
use crate::codegen::lifecycle_transform::desugar_impl_for_test_sharing_lifecycle;
pub use crate::codegen::probe_catalog::dut_probes;
pub use crate::codegen::randomize::{emit_record_randomize_helpers, uses_constraint_solver};
pub use randomize_renderer::emit_randomize_snippets;
use crate::codegen::randomize::{
    PerRunCell, TBIR_RANDOMIZE_CONTEXT_SENTINEL, TBIR_RANDOMIZE_SEQUENCER_SCOPE_SENTINEL,
    TBIR_RANDOMIZE_TSEQ_SCOPE_SENTINEL,
};
#[cfg(test)]
use crate::codegen::randomize::{TBIR_RANDOMIZE_STATE_SENTINEL, TBIR_RANDOMIZE_TARGET_SENTINEL};
use crate::codegen::render_support::{
    bus_param_env, bus_param_env_with_port_override, c_ident, c_type_for,
    collision_free_generated_ident, component_unique_registry_name, cpp_param_names,
    cpp_sint_for_width, cpp_uint_for_width, emit_extern_fn_decls, gate_passes, int_width_from_args,
    tseq_inner_type, txn_field_c_type,
};
use crate::codegen::runtime_support::complete_common_abi_inputs;
pub use crate::codegen::runtime_support::{
    common_runtime_abi_fingerprint, COSIM_RT_HEADER, LOG_RT_HEADER, QUEUE_RT_HEADER,
    RANDOM_RT_HEADER, RUNTIME_HEADERS, THREAD_RT_HEADER, TRACE_RT_HEADER, Z3_RT_HEADER,
};
use crate::codegen::source_transform::{
    and_join, collect_record_keeps, collect_temporal_occurrences, const_usize_expr,
    expr_source_str, fixed_vec_type_args, is_concurrent_assertion, is_list_type,
    list_elem_random_kind, list_elem_type, list_type_args, normalize_vec_record_elem_fields,
    prefix_record_keep_expr, process_interp, property_label, record_type_name,
    string_value_literal_cpp, txn_all_fields, txn_direct_fields, type_arg_const_usize, InterpCap,
    ScalarLeafKind,
};
use crate::lexer::Span;
use std::fmt::Write;

/// HARC's coroutine runtime header. Baked into the binary at build
/// time via `include_str!` and dropped into the test build dir by
/// `harc sim` so the emitted `.cpp` can `#include "harc_thread_rt.h"`
/// without a separate file dependency.
// The two relation-expansion limits and the node counter that charges
// them live in `ast.rs`, because the TYPED lowering path has its own
// expander over the same relations and both run on every `harc sim`.
// Bounding only this one bounds nothing; see the comment there.
use crate::ast::{expr_node_count, RELATION_EXPANSION_BUDGET, RELATION_EXPANSION_MAX_DEPTH};

const INDENT: &str = "    ";

/// Which artifact an [`Emitter`] is currently producing. The legacy
/// self-contained layout keeps byte-identical output; the common-object
/// layout flavors activate the glue-object call rewriting and per-run
/// cell migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavor {
    /// Historical self-contained emission (one TU per shard/test).
    Legacy,
    /// The shared common TU: runtime + glue member definitions.
    CommonImpl,
    /// One stable per-test capsule TU.
    Capsule,
}

/// A function-local mutable `static` migrated into the suite runtime's
/// `_cells` block under the common layout, so two sequential runs in one
/// process start with fresh state.
pub fn emit(file: &SourceFile) -> Result<String, EmitError> {
    emit_with_opts(file, EmitOpts::default())
}

/// Emit a dispatcher plus one or more generated C++ translation units for
/// the tests, so Verilator/Make can compile and recompile them
/// independently.
///
/// Each shard is a self-contained translation unit: `emit_with_opts` over a
/// source filtered to that shard's tests, with the dispatcher `main()`
/// stripped off. The full file-scope scaffolding (the `HarcTestContext`
/// struct, `static`/template helpers, `harc_rng`, …) is re-emitted into every
/// shard, but it all has internal linkage, so the shards link cleanly
/// alongside the generated `main.cpp` dispatcher — the only external symbols
/// are the per-test `run_<TestName>` functions (each unique to one shard) and
/// `main`.
///
/// Incremental granularity comes from `write_if_changed` at the call site
/// (`src/main.rs`): Make skips any shard whose emitted bytes do not change.
/// Absolute source-span-derived identifiers mean an edit that shifts byte
/// offsets can also change later shards. The suite-wide probe contract is a
/// second cross-shard dependency: every shard receives the same validated
/// shared probe intersection/partial-name set, so changing a probe declaration
/// can invalidate shared code independent of shard size. `group_size` (default
/// 4) trades granularity against the per-translation-unit cost of re-parsing
/// the shared scaffolding: `group_size == 1` emits one `test_<name>.cpp` per
/// test; larger groups bundle `group_size` tests per `shard<N>.cpp`.
pub fn emit_split_tests(file: &SourceFile, opts: EmitOpts) -> Result<SplitCppOutput, EmitError> {
    emit_split_tests_with_file_prefix(file, opts, "", 1)
}

pub fn stable_hash_hex(data: &[u8]) -> String {
    common_artifacts::stable_hash_hex(data)
}

/// Output of [`emit_common_split`]: one interface header, one or more
/// common implementation TUs, one stable capsule per test, an explicit
/// registry/dispatcher, all in deterministic order.
#[derive(Debug, Clone)]
pub struct CommonSplitOutput {
    pub files: Vec<GeneratedCppFile>,
    pub test_names: Vec<String>,
    /// FNV fingerprint of the public generated surface. Any change to
    /// shared layouts/signatures changes every dependent artifact.
    pub interface_abi: String,
    /// Fingerprint of every native-build-affecting option. Runtime
    /// selectors (test/seed/variant/log path) never enter it.
    pub build_profile: String,
    pub placement: common_artifacts::PlacementMetrics,
    pub artifact_plan: CommonArtifactPlan,
}

pub(crate) fn sanitize_file_component(name: &str) -> String {
    common_artifacts::sanitize_file_component(name)
}

/// Native-build-affecting fingerprint: covers toolchain-visible knobs only.
/// Runtime selection,
/// seed, variant, and log paths must NOT be included — callers pass
/// exactly the option strings that reach the compiler/linker.
pub fn build_profile_fingerprint(opts: &EmitOpts, extra: &[String]) -> String {
    let mut inputs = opts.build_profile_inputs.clone();
    inputs.extend(extra.iter().cloned());
    common_artifacts::build_profile_fingerprint(opts.mt, &inputs)
}

/// Emit the v1 common-object split layout: reusable
/// runtime/testbench/component/checker/model infrastructure compiles
/// ONCE into common objects; each test emits a small stable capsule;
/// an explicit registry + dispatcher produces one ordinary executable
/// that dispatches tests by name at runtime.
///
/// Ownership model: every shared callable becomes a member of the
/// generated per-run glue object `HarcSuiteGlue`, whose reference
/// members mirror the historical run-scope names (`dut`, `tick`,
/// `_checkers`, hook vectors, ...). Callable BODIES therefore compile
/// verbatim — no capture rewriting — while all mutable state lives on
/// the per-run `HarcSuiteRuntime`, so two sequential runs in one
/// process start fresh.
/// Drain an emitter's accumulated diagnostics into an `EmitError`.
///
/// `Emitter::errors` is a side-channel: emission keeps going after a
/// diagnostic so one bad statement does not mask the rest, and the
/// caller is responsible for checking the vector when it finishes. The
/// legacy single-TU path does that at the end of `emit_with_opts`; the
/// common-object layout drives three separate emitters (header, common
/// impl, one per capsule) and checked NONE of them, so every
/// emitter-level diagnostic was discarded and emission reported success
/// over C++ that was missing the offending statement entirely.
///
/// `context` names which emitter produced the diagnostic, since "the
/// capsule for test T" and "the shared implementation TU" fail for
/// different reasons and the source line alone does not say which.
fn take_emitter_errors(e: &mut Emitter, context: &str) -> Result<(), EmitError> {
    if e.errors.is_empty() {
        return Ok(());
    }
    Err(EmitError(format!(
        "{context}: {}",
        std::mem::take(&mut e.errors).join("\n")
    )))
}

fn normalize_per_run_cells(cells: &mut Vec<PerRunCell>, context: &str) -> Result<(), EmitError> {
    cells.sort_by(|lhs, rhs| lhs.tag.cmp(&rhs.tag));
    cells.dedup_by(|lhs, rhs| lhs.tag == rhs.tag && lhs.ctype == rhs.ctype && lhs.init == rhs.init);
    for pair in cells.windows(2) {
        if pair[0].tag == pair[1].tag {
            return Err(EmitError(format!(
                "{context} randomize cell `{}` has conflicting declarations",
                pair[0].tag
            )));
        }
    }
    Ok(())
}

pub fn emit_common_split(
    file: &SourceFile,
    opts: EmitOpts,
    file_prefix: &str,
    profile_extra: &[String],
) -> Result<CommonSplitOutput, EmitError> {
    let desugared = desugar_impl_for_test_in_file(file);
    let normalized = normalize_vec_record_elem_fields(&desugared);
    let prep = prepare_suite(&normalized, &opts, None)?;
    let tests: Vec<&TestDecl> = prep.tests.clone();
    let test_names: Vec<String> = tests.iter().map(|test| test.name.name.clone()).collect();
    let mut artifact_plan = CommonArtifactPlan::new(file_prefix, &test_names)
        .map_err(|error| EmitError(error.to_string()))?;
    artifact_plan = artifact_plan
        .with_runtime_headers()
        .map_err(|error| EmitError(error.to_string()))?;
    let probe_stub = dut_probes(prep.file)
        .map_err(|error| EmitError(error.to_string()))?
        .map(|(dut_type, probes)| {
            crate::codegen::sv_stub::emit_stub(&dut_type, &probes)
                .map_err(|error| EmitError(error.to_string()))
        })
        .transpose()?;
    if probe_stub.is_some() {
        artifact_plan = artifact_plan
            .with_probe_stub()
            .map_err(|error| EmitError(error.to_string()))?;
    }

    // ── Shared-callable inventory ────────────────────────────────────
    // Suite functions/tseqs plus every component/transactor method and
    // watchdog become glue members. Capsule call sites get a `_glue.`
    // prefix through the emitter's callee table.
    let mut glue_callees: std::collections::HashSet<String> = std::collections::HashSet::new();
    for f in &prep.funcs {
        glue_callees.insert(f.name.name.clone());
    }
    for t in &prep.tseqs {
        glue_callees.insert(t.name.name.clone());
    }
    let hook_regs = collect_hook_registries(prep.file, &prep.component_order);
    // Bus-bound hookables resolve `bus.<ch>.<sig>` against per-test bind
    // state the shared TU cannot see, so they are NOT glue members —
    // each capsule emits its own copy (see `bus_bound_component_types`).
    let bus_bound = bus_bound_component_types(prep.file);
    for reg in &hook_regs {
        if bus_bound.contains(&reg.comp_ty) {
            continue;
        }
        if !reg.watchdog {
            glue_callees.insert(format!("{}_{}", reg.comp_ty, reg.method));
        } else {
            glue_callees.insert(format!("{}_watchdog", reg.comp_ty));
        }
    }
    let common_callable_count = glue_callees.len();
    let capsule_hook_count = hook_regs
        .iter()
        .filter(|reg| bus_bound.contains(&reg.comp_ty))
        .count();
    let mut capsule_reasons = std::collections::BTreeMap::new();
    capsule_reasons.insert("test_body".to_string(), tests.len());
    if capsule_hook_count != 0 {
        capsule_reasons.insert("bus_bound_hook".to_string(), capsule_hook_count);
    }
    let placement = common_artifacts::PlacementMetrics::new(
        common_callable_count,
        tests.len() + capsule_hook_count,
        capsule_reasons,
    )
    .map_err(|error| EmitError(error.to_string()))?;

    // ── Common implementation TU ─────────────────────────────────────
    let mut e_common = emitter_from_seed(prep.emitter_seed.clone());
    e_common.flavor = Flavor::CommonImpl;
    e_common.randomize_context_ident = "this->ctx".to_string();
    e_common.glue_callees = glue_callees.clone();
    e_common.probes = prep.shared_component_probes.clone();
    e_common.probe_widths = prep.shared_component_probe_widths.clone();
    e_common.partial_probe_names = prep.partial_component_probe_names.clone();
    e_common.probe_dut_type = Some(prep.dut_type.to_string());
    e_common.probe_receivers.insert("dut".to_string());

    let profile = build_profile_fingerprint(&opts, profile_extra);
    writeln!(e_common.out, "// Auto-generated by harc — do not edit.").ok();
    writeln!(
        e_common.out,
        "// HARC common-object suite implementation (issue #643)."
    )
    .ok();
    writeln!(e_common.out, "// harc build-profile: {profile}").ok();
    writeln!(e_common.out, "").ok();
    writeln!(e_common.out, "#ifdef __clang__").ok();
    writeln!(e_common.out, "#pragma clang optimize off").ok();
    writeln!(e_common.out, "#endif").ok();
    writeln!(e_common.out, "").ok();
    writeln!(e_common.out, "#include \"{file_prefix}suite_api.hpp\"").ok();
    writeln!(e_common.out, "").ok();
    // Interface-ABI anchor definition. Capsules and the registry take
    // its address, so a capsule compiled against a different interface
    // hash fails to LINK instead of running with stale layout
    // assumptions.
    writeln!(e_common.out, "extern const char harc_suite_abi_ANCHOR[];").ok();
    if !prep.runtime_problem_table.problems.is_empty() {
        e_common.out.push_str(
            &prep
                .runtime_problem_table
                .render_cpp_private_descriptors("_harc_runtime_random_problem_table"),
        );
        writeln!(
            e_common.out,
            "harc_rt::random::HarcRandomizeCall harc_prepare_randomize_call("
        )
        .ok();
        writeln!(
            e_common.out,
            "{INDENT}harc_rt::random::HarcRuntimeCallSite& call_site,"
        )
        .ok();
        writeln!(
            e_common.out,
            "{INDENT}harc_rt::random::harc_problem_id problem_id,"
        )
        .ok();
        writeln!(
            e_common.out,
            "{INDENT}harc_rt::random::harc_seed global_seed,"
        )
        .ok();
        writeln!(
            e_common.out,
            "{INDENT}harc_rt::random::harc_seed fallback_seed) {{"
        )
        .ok();
        writeln!(
            e_common.out,
            "{INDENT}return harc_rt::random::harc_prepare_randomize_call("
        )
        .ok();
        writeln!(
            e_common.out,
            "{INDENT}{INDENT}_harc_runtime_random_problem_table,"
        )
        .ok();
        writeln!(e_common.out, "{INDENT}{INDENT}&call_site,").ok();
        writeln!(e_common.out, "{INDENT}{INDENT}1,").ok();
        writeln!(e_common.out, "{INDENT}{INDENT}problem_id,").ok();
        writeln!(e_common.out, "{INDENT}{INDENT}global_seed,").ok();
        writeln!(e_common.out, "{INDENT}{INDENT}fallback_seed);").ok();
        writeln!(e_common.out, "}}").ok();
        writeln!(e_common.out).ok();
    }

    emit_glue_definitions(&mut e_common, &prep)?;
    emit_shared_callables(&mut e_common, &prep)?;
    take_emitter_errors(&mut e_common, "common-object shared implementation")?;

    let common_cpp = e_common.out;

    // ── Test capsules (one stable source per test) ───────────────────
    let mut files: Vec<GeneratedCppFile> = Vec::new();
    let mut all_cells: Vec<PerRunCell> = Vec::new();
    let mut capsule_sources: Vec<(String, String)> = Vec::new(); // (filename, body)
    for (test_index, (test, artifact)) in tests.iter().zip(artifact_plan.tests()).enumerate() {
        let mut cell_plan = emitter_from_seed(prep.emitter_seed.clone());
        cell_plan.flavor = Flavor::Capsule;
        cell_plan.seed_declared_value_names(prep.file);
        cell_plan.select_randomize_context_ident_for_capsule(prep.file, &test.name.name);
        cell_plan.glue_callees = glue_callees.clone();
        cell_plan.emit_test_run(&prep, test)?;
        take_emitter_errors(
            &mut cell_plan,
            &format!("test `{}` cell plan", test.name.name),
        )?;
        normalize_per_run_cells(
            &mut cell_plan.per_run_cells,
            &format!("test `{}` capsule", test.name.name),
        )?;

        let mut ec = emitter_from_seed(prep.emitter_seed.clone());
        ec.flavor = Flavor::Capsule;
        ec.seed_declared_value_names(prep.file);
        ec.select_randomize_context_ident_for_capsule(prep.file, &test.name.name);
        ec.glue_callees = glue_callees.clone();
        ec.per_run_cells = cell_plan.per_run_cells;
        ec.emit_test_run(&prep, test)?;
        take_emitter_errors(&mut ec, &format!("test `{}`", test.name.name))?;
        normalize_per_run_cells(
            &mut ec.per_run_cells,
            &format!("test `{}` capsule", test.name.name),
        )?;
        let capsule = artifact_plan.capsule_for_test(test_index);
        let fname = artifact_plan
            .artifact(capsule.artifact_index())
            .filename()
            .to_string();
        let mut body = String::new();
        writeln!(body, "// Auto-generated by harc — do not edit.").ok();
        writeln!(body, "// HARC test capsule: {}", test.name.name).ok();
        writeln!(body, "// harc build-profile: {profile}").ok();
        writeln!(body, "").ok();
        writeln!(body, "#ifdef __clang__").ok();
        writeln!(body, "#pragma clang optimize off").ok();
        writeln!(body, "#endif").ok();
        writeln!(body, "").ok();
        writeln!(body, "#include \"{file_prefix}suite_api.hpp\"").ok();
        writeln!(body, "").ok();
        writeln!(body, "extern const char harc_suite_abi_ANCHOR[];").ok();
        writeln!(body, "").ok();
        body.push_str(ec.out.trim_start_matches('\n'));
        // Per-test descriptor: explicit registry entry with no static
        // constructors or link-order discovery.
        writeln!(body, "").ok();
        body.push_str(&common_artifacts::render_test_descriptor_with_required_abi(
            artifact,
        ));
        capsule_sources.push((fname, body));
    }

    // Only shared-callable cells belong to the suite ABI. Test-local cells
    // are declared inside their capsule's run function above.
    for cell in std::mem::take(&mut e_common.per_run_cells) {
        if !all_cells.iter().any(|c| c.tag == cell.tag) {
            all_cells.push(cell);
        }
    }

    // ── Interface header (rendered last: owns the complete cell set,
    //    hook registries, and shared-callable prototypes) ─────────────
    let mut eh = emitter_from_seed(prep.emitter_seed.clone());
    eh.flavor = Flavor::CommonImpl;
    eh.glue_callees = glue_callees;
    let mut header = String::new();
    writeln!(header, "// Auto-generated by harc — do not edit.").ok();
    writeln!(
        header,
        "// HARC common-object suite interface (issue #643)."
    )
    .ok();
    writeln!(
        header,
        "// One binary runs every test: common infra compiled once,"
    )
    .ok();
    writeln!(header, "// per-test capsules carry scenario-only code.").ok();
    writeln!(header, "").ok();
    writeln!(header, "#pragma once").ok();
    writeln!(header, "").ok();
    writeln!(header, "#include <cstdint>").ok();
    writeln!(header, "").ok();
    writeln!(header, "// === iface-begin ===").ok();
    writeln!(header, "").ok();
    // Reuse the exact legacy type surface so both layouts describe
    // identical records/components/RAL/covergroup types.
    std::mem::swap(&mut eh.out, &mut header);
    eh.emit_file_scope_scaffolding(&prep)?;
    take_emitter_errors(&mut eh, "common-object suite interface")?;
    std::mem::swap(&mut eh.out, &mut header);
    // Scaffolding registers no per-run cells today; merge anyway so a
    // future cell registered during type emission cannot be dropped.
    for cell in std::mem::take(&mut eh.per_run_cells) {
        if !all_cells.iter().any(|c| c.tag == cell.tag) {
            all_cells.push(cell);
        }
    }
    emit_runtime_decls(&mut header, &eh, &prep, &all_cells, &hook_regs)?;
    let abi_inputs = complete_common_abi_inputs(&opts.common_abi_inputs);
    let anchor = common_artifacts::AbiAnchor::from_marked_interface_with_identity(
        &header,
        common_artifacts::CodegenBackend::V1,
        common_artifacts::CppLayout::Common,
        &abi_inputs,
    )
    .map_err(|error| EmitError(error.to_string()))?;
    let iface_abi = anchor.digest().to_string();
    let header = anchor
        .bind_declarations(&header)
        .map_err(|error| EmitError(error.to_string()))?;
    let common_cpp = anchor
        .bind_definition(&common_cpp)
        .map_err(|error| EmitError(error.to_string()))?;
    let mut fixed_capsules = Vec::with_capacity(capsule_sources.len());
    for (fname, body) in capsule_sources {
        fixed_capsules.push((
            fname,
            anchor
                .bind_declarations(&body)
                .map_err(|error| EmitError(error.to_string()))?,
        ));
    }
    let reg =
        common_artifacts::render_registry_with_required_abi(&artifact_plan, &profile, &anchor);

    files.push(GeneratedCppFile {
        filename: artifact_plan.interface().filename().to_string(),
        contents: header,
    });
    let common_artifact = artifact_plan
        .common_artifacts()
        .next()
        .expect("v1 common layout always has one common implementation");
    files.push(GeneratedCppFile {
        filename: common_artifact.filename().to_string(),
        contents: common_cpp,
    });
    for (fname, body) in fixed_capsules {
        files.push(GeneratedCppFile {
            filename: fname,
            contents: body,
        });
    }
    files.push(GeneratedCppFile {
        filename: artifact_plan.registry().filename().to_string(),
        contents: reg,
    });
    if let Some(contents) = probe_stub {
        files.push(GeneratedCppFile {
            filename: artifact_plan
                .probe_stub()
                .expect("v1 common probe source has a planned artifact")
                .filename()
                .to_string(),
            contents,
        });
    }

    Ok(CommonSplitOutput {
        files,
        test_names,
        interface_abi: iface_abi,
        build_profile: profile,
        placement,
        artifact_plan,
    })
}

/// Emit the suite interface declarations appended after the shared type
/// surface: per-run runtime struct, clock cell, glue object with every
/// shared-callable prototype, hook registries, mutable-cell storage,
/// and the test-descriptor contract. The iface-begin/end markers delimit
/// the exact byte range
/// hashed into the link-time ABI anchor.
fn emit_runtime_decls(
    out: &mut String,
    e: &Emitter,
    prep: &SuitePrep,
    cells: &[PerRunCell],
    hooks: &[HookRegistryDesc],
) -> Result<(), EmitError> {
    let dut = prep.dut_type;
    let mut o = std::mem::take(out);
    writeln!(o, "").ok();
    emit_extern_fn_decls(&mut o, prep.file);

    // ── Contract types ──────────────────────────────────────────────
    writeln!(o, "struct HarcTestDescriptor {{").ok();
    writeln!(o, "{INDENT}const char* name;").ok();
    writeln!(o, "{INDENT}int (*run)(int argc, char** argv);").ok();
    writeln!(o, "{INDENT}const void* abi_anchor;").ok();
    writeln!(o, "}};").ok();
    writeln!(o, "").ok();

    // ── Clock cell (multi-clock configuration is capsule-provided) ──
    writeln!(o, "struct HarcClockCell {{").ok();
    writeln!(o, "{INDENT}const char* name;").ok();
    writeln!(o, "{INDENT}long long half_period_ps;").ok();
    writeln!(o, "{INDENT}long long next_edge_ps;").ok();
    writeln!(o, "{INDENT}int level;").ok();
    writeln!(o, "{INDENT}long long rising_count;").ok();
    writeln!(o, "{INDENT}std::function<void(int)> set_level;").ok();
    writeln!(o, "}};").ok();
    writeln!(o, "").ok();

    // ── Per-run runtime owner ───────────────────────────────────────
    // Deepened `HarcTestContext`: owns DUT + waves + diagnostics +
    // service registries as before, plus everything that used to be a
    // mutable file-scope/function-local static: PRNG, unique-history /
    // property-slot / watchdog / coverage cells, and the hook
    // registries. One instance per run; two sequential runs in one
    // process share nothing mutable.
    writeln!(o, "struct HarcSuiteRuntime {{").ok();
    writeln!(o, "{INDENT}VerilatedContext verilated;").ok();
    writeln!(o, "{INDENT}V{dut}* dut = nullptr;", dut = dut).ok();
    writeln!(o, "#if HARC_TRACE_ENABLED").ok();
    writeln!(o, "{INDENT}HarcTraceC* tfp = nullptr;").ok();
    writeln!(o, "{INDENT}std::string _wave_path;").ok();
    writeln!(o, "#endif").ok();
    writeln!(o, "{INDENT}uint64_t _trace_time = 0;").ok();
    writeln!(o, "{INDENT}int errors = 0;").ok();
    writeln!(o, "{INDENT}bool _fatal = false;").ok();
    writeln!(o, "{INDENT}int cycle_count = 0;").ok();
    writeln!(o, "{INDENT}harc_rt::trace::HarcTraceWriter trace;").ok();
    writeln!(o, "{INDENT}harc_rt::log::HarcLogContext log_ctx;").ok();
    writeln!(o, "{INDENT}std::vector<std::function<void()>> _checkers;").ok();
    writeln!(
        o,
        "{INDENT}std::vector<std::function<void()>> _post_eval_services;"
    )
    .ok();
    writeln!(
        o,
        "{INDENT}std::vector<std::function<void()>> _auto_cov_reports;"
    )
    .ok();
    if prep.qualified_clock_wait {
        writeln!(
            o,
            "{INDENT}std::function<void()> _harc_advance_actors = []() {{}};"
        )
        .ok();
        writeln!(o, "{INDENT}bool _harc_actor_tick_due = false;").ok();
    }
    writeln!(o, "{INDENT}harc_rt::random::HarcRng rng;").ok();
    // Hook registries — one set per generated instance. Names mirror
    // the historical run-scope variables exactly.
    for reg in hooks {
        let v = reg.vector();
        let arg_csv = if reg.watchdog {
            String::new()
        } else {
            hook_registry_arg_csv(e, prep, reg)
        };
        writeln!(
            o,
            "{INDENT}std::vector<std::function<void({args})>> {v};",
            args = arg_csv
        )
        .ok();
    }
    if !cells.is_empty() {
        writeln!(o, "{INDENT}struct {{").ok();
        for c in cells {
            match &c.init {
                Some(init) => {
                    writeln!(
                        o,
                        "{INDENT}{INDENT}{ty} {tag} = {init};",
                        ty = c.ctype,
                        tag = c.tag,
                        init = init
                    )
                    .ok();
                }
                None => {
                    writeln!(o, "{INDENT}{INDENT}{ty} {tag};", ty = c.ctype, tag = c.tag).ok();
                }
            }
        }
        writeln!(o, "{INDENT}}} _cells;").ok();
    }
    writeln!(o, "}};").ok();
    writeln!(o, "").ok();

    if !prep.runtime_problem_table.problems.is_empty() {
        writeln!(
            o,
            "harc_rt::random::HarcRandomizeCall harc_prepare_randomize_call("
        )
        .ok();
        writeln!(
            o,
            "{INDENT}harc_rt::random::HarcRuntimeCallSite& call_site,"
        )
        .ok();
        writeln!(o, "{INDENT}harc_rt::random::harc_problem_id problem_id,").ok();
        writeln!(o, "{INDENT}harc_rt::random::harc_seed global_seed,").ok();
        writeln!(o, "{INDENT}harc_rt::random::harc_seed fallback_seed);").ok();
        writeln!(o, "").ok();
    }

    // ── Glue object: lexical host for shared callables ───────────────
    // Reference members recreate the historical run-scope names so the
    // converted callable bodies compile verbatim; member functions give
    // `tick`/logging/dump helpers a capture-free home.
    writeln!(o, "struct HarcSuiteGlue {{").ok();
    writeln!(o, "{INDENT}HarcSuiteRuntime& ctx;").ok();
    writeln!(o, "{INDENT}V{dut}*& dut;", dut = dut).ok();
    writeln!(o, "{INDENT}harc_rt::random::HarcRng& harc_rng;").ok();
    writeln!(o, "{INDENT}int& cycle_count;").ok();
    writeln!(o, "{INDENT}int& errors;").ok();
    writeln!(o, "{INDENT}bool& _fatal;").ok();
    writeln!(o, "{INDENT}uint64_t& _trace_time;").ok();
    writeln!(o, "#if HARC_TRACE_ENABLED").ok();
    writeln!(o, "{INDENT}HarcTraceC*& tfp;").ok();
    writeln!(o, "{INDENT}std::string& _wave_path;").ok();
    writeln!(o, "#endif").ok();
    writeln!(o, "{INDENT}harc_rt::trace::HarcTraceWriter& trace;").ok();
    writeln!(o, "{INDENT}harc_rt::log::HarcLogContext& log_ctx;").ok();
    writeln!(o, "{INDENT}std::vector<std::function<void()>>& _checkers;").ok();
    writeln!(
        o,
        "{INDENT}std::vector<std::function<void()>>& _post_eval_services;"
    )
    .ok();
    writeln!(
        o,
        "{INDENT}std::vector<std::function<void()>>& _auto_cov_reports;"
    )
    .ok();
    if prep.qualified_clock_wait {
        writeln!(o, "{INDENT}std::function<void()>& _harc_advance_actors;").ok();
        writeln!(o, "{INDENT}bool& _harc_actor_tick_due;").ok();
    }
    for reg in hooks {
        let v = reg.vector();
        let arg_csv = if reg.watchdog {
            String::new()
        } else {
            hook_registry_arg_csv(e, prep, reg)
        };
        writeln!(
            o,
            "{INDENT}std::vector<std::function<void({args})>>& {v};",
            args = arg_csv
        )
        .ok();
    }
    writeln!(o, "{INDENT}std::vector<HarcClockCell> clocks_;").ok();
    writeln!(o, "{INDENT}long long now_ps = 0;").ok();
    writeln!(o, "").ok();
    writeln!(o, "{INDENT}explicit HarcSuiteGlue(HarcSuiteRuntime& r);").ok();
    writeln!(
        o,
        "{INDENT}HarcSuiteGlue(HarcSuiteRuntime& r, std::vector<HarcClockCell> clocks);"
    )
    .ok();
    writeln!(o, "").ok();
    writeln!(o, "{INDENT}void init_clocks();").ok();
    writeln!(o, "{INDENT}uint64_t harc_rng_next();").ok();
    writeln!(
        o,
        "{INDENT}void sim_logf_line(FILE* f, const char* sev, const char* fmt, ...);"
    )
    .ok();
    writeln!(
        o,
        "{INDENT}void sim_log_line(const char* sev, const char* fmt, ...);"
    )
    .ok();
    writeln!(
        o,
        "{INDENT}void _harc_trace_dump_next(const char* clock, uint64_t clock_cycle);"
    )
    .ok();
    writeln!(
        o,
        "{INDENT}void _harc_trace_dump_at(uint64_t t, const char* clock, uint64_t clock_cycle);"
    )
    .ok();
    writeln!(o, "{INDENT}void eval_clocks_until(long long t_ps);").ok();
    writeln!(o, "{INDENT}void tick();").ok();
    writeln!(o, "").ok();

    // Shared-callable prototypes (bodies live in the common TU only).
    // Emitted in dependency order so capsules and the common TU agree on
    // declaration order regardless of test set.
    for f in &prep.funcs {
        let ret = f
            .return_ty
            .as_ref()
            .map(c_type_for)
            .unwrap_or_else(|| "void".to_string());
        write!(
            o,
            "{INDENT}{ret} {}(",
            global_callable_cpp_name(&f.name.name)
        )
        .ok();
        write_glue_params(&mut o, &f.params);
        writeln!(o, ");").ok();
    }
    for t in &prep.tseqs {
        let inner = t
            .return_ty
            .as_ref()
            .and_then(tseq_inner_type)
            .unwrap_or_else(|| "int64_t".to_string());
        write!(
            o,
            "{INDENT}std::vector<{inner}> {}(",
            global_callable_cpp_name(&t.name.name)
        )
        .ok();
        write_glue_params(&mut o, &t.params);
        writeln!(o, ");").ok();
    }
    let bus_bound = bus_bound_component_types(prep.file);
    for reg in hooks {
        // One prototype per callable (see emit_shared_callables).
        if !reg.watchdog && !reg.pre {
            continue;
        }
        if reg.watchdog {
            continue;
        }
        // Bus-bound hookables are capsule-local, not glue members.
        if bus_bound.contains(&reg.comp_ty) {
            continue;
        }
        let args = hook_registry_arg_csv(e, prep, reg);
        let ret = hook_registry_ret_ty(e, prep, reg);
        writeln!(
            o,
            "{INDENT}{ret} {ty}_{m}({ty}& self{csv});",
            ret = ret,
            ty = reg.comp_ty,
            m = reg.method,
            csv = if args.is_empty() {
                String::new()
            } else {
                format!(", {args}")
            }
        )
        .ok();
    }
    for reg in hooks {
        if !reg.watchdog {
            continue;
        }
        // `collect_hook_registries` records one entry per SIDE (pre and
        // post) for each watchdog, but there is only ONE callable. Without
        // this the prototype is emitted twice and the header fails to
        // compile with "class member cannot be redeclared".
        if !reg.pre {
            continue;
        }
        if bus_bound.contains(&reg.comp_ty) {
            continue;
        }
        writeln!(
            o,
            "{INDENT}void {ty}_watchdog({ty}& self);",
            ty = reg.comp_ty
        )
        .ok();
    }
    writeln!(o, "}};").ok();
    writeln!(o, "").ok();
    writeln!(o, "// === iface-end ===").ok();
    // Link-time interface-ABI anchor: capsules and the registry take
    // its address, so an artifact compiled against a different
    // interface hash fails to LINK instead of running with stale
    // layout assumptions. The symbol
    // name carries the hash of everything between the markers above.
    writeln!(o, "").ok();
    writeln!(o, "extern const char harc_suite_abi_ANCHOR[];").ok();
    *out = o;
    Ok(())
}

/// Parameter list for one hook registry, rendered with the SAME type
/// mapping `emit_component_method` uses for the definition
/// (`c_type_for_param`). Going through the free `c_type_for` here
/// instead renders a declared transaction/struct/enum as `V<Name>*` —
/// a Verilator DUT handle that does not exist — so the header and the
/// out-of-line definition disagree and the suite does not compile.
/// Component / transactor types whose hookable bodies must stay
/// capsule-local under the common layout.
///
/// `bus.<ch>.<sig>` never survives into C++: the emitter resolves it at
/// codegen time into a concrete DUT port write (`dut->axil_aw_addr`)
/// using the binding that `emit_component_method` installs from
/// `driver_bus_for_hookables`. That table is PER-TEST — it is built in
/// `emit_test_run` from this test's `let x : Drv = bind <bus>` — and the
/// resolved port name embeds the test's own let-name as the signal
/// prefix. A single shared definition in the per-SUITE common TU can
/// therefore bake in only one test's binding, and two tests that name
/// their bus binding differently would silently drive different DUT
/// ports through the same shared body.
///
/// Rather than analyse cross-test agreement (whose failure mode is a
/// silently wrong simulation, not a build error), these keep the legacy
/// per-test `[&]` lambda emission — one copy per capsule. Their pre/post
/// hook registries still live on the runtime, so per-run state ownership
/// is unaffected; only the dedup is given up. The predicate is
/// deliberately conservative: every hookable on a `bound to` decl stays
/// local, not just the ones whose body happens to mention `bus`.
fn bus_bound_component_types(file: &SourceFile) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for it in &file.items {
        match it {
            Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) | Item::Scoreboard(c) => {
                if c.bound_to.is_some() {
                    out.insert(c.name.name.clone());
                }
            }
            Item::Transactor(t) => {
                if t.bound_to.is_some() {
                    out.insert(t.name.name.clone());
                }
            }
            _ => {}
        }
    }
    out
}

fn hook_registry_arg_csv(e: &Emitter, prep: &SuitePrep, reg: &HookRegistryDesc) -> String {
    for &idx in &prep.component_order {
        match &prep.file.items[idx] {
            Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) | Item::Scoreboard(c)
                if c.name.name == reg.comp_ty =>
            {
                for ci in &c.items {
                    if let ComponentItem::Hookable(h) = ci {
                        if h.name.name == reg.method {
                            return hook_param_csv(e, h);
                        }
                    }
                }
            }
            Item::Transactor(t) if t.name.name == reg.comp_ty => {
                let synth = synth_component_from_transactor(t, true);
                for ci in &synth.items {
                    if let ComponentItem::Hookable(h) = ci {
                        if h.name.name == reg.method {
                            return hook_param_csv(e, &h);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    String::new()
}

fn hook_param_csv(e: &Emitter, h: &HookableMethod) -> String {
    let names = cpp_param_names(&h.params);
    let mut parts = Vec::new();
    for (i, p) in h.params.iter().enumerate() {
        let pty =
            p.ty.as_ref()
                .map(|t| e.c_type_for_param(t))
                .unwrap_or_else(|| "int64_t".to_string());
        parts.push(format!("{pty} {}", names[i]));
    }
    parts.join(", ")
}

/// Return type for one hookable, matching `emit_component_method`'s
/// `c_type_for_value` (see `hook_registry_arg_csv` for why the free
/// `c_type_for` is wrong here).
fn hook_registry_ret_ty(e: &Emitter, prep: &SuitePrep, reg: &HookRegistryDesc) -> String {
    for &idx in &prep.component_order {
        match &prep.file.items[idx] {
            Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) | Item::Scoreboard(c)
                if c.name.name == reg.comp_ty =>
            {
                for ci in &c.items {
                    if let ComponentItem::Hookable(h) = ci {
                        if h.name.name == reg.method {
                            return h
                                .return_ty
                                .as_ref()
                                .map(|t| e.c_type_for_value(t))
                                .unwrap_or_else(|| "void".to_string());
                        }
                    }
                }
            }
            Item::Transactor(t) if t.name.name == reg.comp_ty => {
                let synth = synth_component_from_transactor(t, true);
                for ci in &synth.items {
                    if let ComponentItem::Hookable(h) = ci {
                        if h.name.name == reg.method {
                            return h
                                .return_ty
                                .as_ref()
                                .map(|t| e.c_type_for_value(t))
                                .unwrap_or_else(|| "void".to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    "void".to_string()
}

fn write_glue_params(out: &mut String, params: &[crate::ast::Param]) {
    let names = cpp_param_names(params);
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            write!(out, ", ").ok();
        }
        let pty =
            p.ty.as_ref()
                .map(c_type_for)
                .unwrap_or_else(|| "int64_t".to_string());
        write!(out, "{pty} {}", names[i]).ok();
    }
}

/// Emit the glue object's runtime-member definitions into the common
/// TU: constructors binding the historical run-scope names as
/// references, clock initialization, RNG/logging/dump helpers, and the
/// clock-advance state machine + `tick()`.
fn emit_glue_definitions(e: &mut Emitter, prep: &SuitePrep) -> Result<(), EmitError> {
    let _dut = prep.dut_type; // DUT type appears only in the header decl
    let hooks = collect_hook_registries(prep.file, &prep.component_order);
    let o = &mut e.out;
    // Reference bindings mirror the legacy prologue aliases exactly.
    // Per-hook registries are reference members too — C++ requires
    // them in every initializer list.
    // Mem-initializer order MUST match the member declaration order in
    // `emit_runtime_decls` (ctx, dut, harc_rng, cycle_count, errors,
    // _fatal, _trace_time, [tfp, _wave_path], trace, log_ctx, _checkers,
    // _post_eval_services, _auto_cov_reports, actor scheduling state, then
    // the hook registries).
    // These are all reference members bound to already-live
    // `HarcSuiteRuntime` fields so the order cannot change behaviour, but
    // a mismatch is a `-Wreorder` diagnostic on every hook-bearing suite
    // and a hard error under `-Werror`.
    writeln!(o, "HarcSuiteGlue::HarcSuiteGlue(HarcSuiteRuntime& r)").ok();
    writeln!(
        o,
        "{INDENT}: ctx(r), dut(r.dut), harc_rng(r.rng), cycle_count(r.cycle_count), errors(r.errors),"
    )
    .ok();
    writeln!(
        o,
        "{INDENT}{INDENT}_fatal(r._fatal), _trace_time(r._trace_time),"
    )
    .ok();
    writeln!(o, "#if HARC_TRACE_ENABLED").ok();
    writeln!(o, "{INDENT}{INDENT}tfp(r.tfp), _wave_path(r._wave_path),").ok();
    writeln!(o, "#endif").ok();
    writeln!(o, "{INDENT}{INDENT}trace(r.trace), log_ctx(r.log_ctx),").ok();
    writeln!(o, "{INDENT}{INDENT}_checkers(r._checkers),").ok();
    writeln!(
        o,
        "{INDENT}{INDENT}_post_eval_services(r._post_eval_services),"
    )
    .ok();
    let ctor_body = "{}";
    if prep.qualified_clock_wait {
        writeln!(o, "{INDENT}{INDENT}_auto_cov_reports(r._auto_cov_reports),").ok();
        writeln!(
            o,
            "{INDENT}{INDENT}_harc_advance_actors(r._harc_advance_actors),"
        )
        .ok();
        if hooks.is_empty() {
            writeln!(
                o,
                "{INDENT}{INDENT}_harc_actor_tick_due(r._harc_actor_tick_due) {ctor_body}"
            )
            .ok();
        } else {
            writeln!(
                o,
                "{INDENT}{INDENT}_harc_actor_tick_due(r._harc_actor_tick_due),"
            )
            .ok();
            for (i, reg) in hooks.iter().enumerate() {
                let v = reg.vector();
                let tail = if i + 1 == hooks.len() {
                    format!(" {ctor_body}")
                } else {
                    ",".to_string()
                };
                writeln!(o, "{INDENT}{INDENT}{v}(r.{v}){tail}").ok();
            }
        }
    } else if hooks.is_empty() {
        writeln!(
            o,
            "{INDENT}{INDENT}_auto_cov_reports(r._auto_cov_reports) {ctor_body}"
        )
        .ok();
    } else {
        writeln!(o, "{INDENT}{INDENT}_auto_cov_reports(r._auto_cov_reports),").ok();
        for (i, reg) in hooks.iter().enumerate() {
            let v = reg.vector();
            let tail = if i + 1 == hooks.len() {
                format!(" {ctor_body}")
            } else {
                ",".to_string()
            };
            writeln!(o, "{INDENT}{INDENT}{v}(r.{v}){tail}").ok();
        }
    }
    writeln!(o, "").ok();
    writeln!(
        o,
        "HarcSuiteGlue::HarcSuiteGlue(HarcSuiteRuntime& r, std::vector<HarcClockCell> clocks)"
    )
    .ok();
    writeln!(
        o,
        "{INDENT}: HarcSuiteGlue(r) {{ clocks_ = std::move(clocks); init_clocks(); }}"
    )
    .ok();
    writeln!(o, "").ok();
    writeln!(o, "void HarcSuiteGlue::init_clocks() {{").ok();
    writeln!(
        o,
        "{INDENT}// First edge fires at half_period (rising) so initial level is 0."
    )
    .ok();
    writeln!(o, "{INDENT}for (auto& c : clocks_) {{").ok();
    writeln!(o, "{INDENT}{INDENT}c.next_edge_ps = c.half_period_ps;").ok();
    writeln!(o, "{INDENT}{INDENT}c.level = 0;").ok();
    writeln!(o, "{INDENT}{INDENT}if (c.set_level) c.set_level(0);").ok();
    writeln!(o, "{INDENT}}}").ok();
    writeln!(o, "}}").ok();
    writeln!(o, "").ok();
    writeln!(
        o,
        "uint64_t HarcSuiteGlue::harc_rng_next() {{ return ctx.rng.next(); }}"
    )
    .ok();
    writeln!(o, "").ok();
    // Variadic loggers — identical bodies to the legacy run-scope
    // lambdas; the runtime owns the sinks.
    writeln!(
        o,
        "void HarcSuiteGlue::sim_logf_line(FILE* f, const char* sev, const char* fmt, ...) {{"
    )
    .ok();
    writeln!(
        o,
        "{INDENT}HARC_RT_LOG_FILE_ONLY_PRINTF(f, cycle_count, sev, fmt);"
    )
    .ok();
    writeln!(o, "}}").ok();
    writeln!(o, "").ok();
    writeln!(
        o,
        "void HarcSuiteGlue::sim_log_line(const char* sev, const char* fmt, ...) {{"
    )
    .ok();
    writeln!(
        o,
        "{INDENT}HARC_RT_LOG_PRINTF(ctx.log_ctx.sim_log, &ctx.trace, cycle_count, sev, fmt);"
    )
    .ok();
    writeln!(o, "}}").ok();
    writeln!(o, "").ok();
    // Wave dumps (no-op unless a waves build defined HARC_TRACE_*).
    writeln!(
        o,
        "void HarcSuiteGlue::_harc_trace_dump_next(const char* clock, uint64_t clock_cycle) {{"
    )
    .ok();
    writeln!(o, "{INDENT}uint64_t t = _trace_time++;").ok();
    writeln!(o, "{INDENT}trace.set_timing(t, clock, clock_cycle);").ok();
    writeln!(o, "{INDENT}HARC_RT_DUMP_WAVE_TRACE(tfp, t);").ok();
    writeln!(o, "}}").ok();
    writeln!(o, "").ok();
    writeln!(
        o,
        "void HarcSuiteGlue::_harc_trace_dump_at(uint64_t t, const char* clock, uint64_t clock_cycle) {{"
    )
    .ok();
    writeln!(o, "{INDENT}trace.set_timing(t, clock, clock_cycle);").ok();
    writeln!(o, "{INDENT}HARC_RT_DUMP_WAVE_TRACE(tfp, t);").ok();
    writeln!(o, "}}").ok();
    writeln!(o, "").ok();
    // Multi-clock advance loop. Empty `clocks_` means the suite (this
    // run) is single-clock; tick() picks the matching path per run so
    // mixed suites stay correct without per-test glue variants.
    writeln!(
        o,
        "void HarcSuiteGlue::eval_clocks_until(long long t_ps) {{"
    )
    .ok();
    writeln!(o, "{INDENT}while (now_ps < t_ps) {{").ok();
    writeln!(o, "{INDENT}{INDENT}long long next = t_ps;").ok();
    writeln!(
        o,
        "{INDENT}{INDENT}for (auto& c : clocks_) if (c.next_edge_ps < next) next = c.next_edge_ps;"
    )
    .ok();
    writeln!(o, "{INDENT}{INDENT}now_ps = next;").ok();
    writeln!(o, "{INDENT}{INDENT}bool _primary_rising = false;").ok();
    writeln!(o, "{INDENT}{INDENT}const char* _last_edge_clock = \"\";").ok();
    writeln!(o, "{INDENT}{INDENT}uint64_t _last_edge_cycle = 0;").ok();
    writeln!(
        o,
        "{INDENT}{INDENT}for (size_t i = 0; i < clocks_.size(); i++) {{"
    )
    .ok();
    writeln!(o, "{INDENT}{INDENT}{INDENT}auto& c = clocks_[i];").ok();
    writeln!(
        o,
        "{INDENT}{INDENT}{INDENT}if (c.next_edge_ps == now_ps) {{"
    )
    .ok();
    writeln!(o, "{INDENT}{INDENT}{INDENT}{INDENT}c.level = !c.level;").ok();
    writeln!(
        o,
        "{INDENT}{INDENT}{INDENT}{INDENT}if (c.set_level) c.set_level(c.level);"
    )
    .ok();
    writeln!(
        o,
        "{INDENT}{INDENT}{INDENT}{INDENT}c.next_edge_ps += c.half_period_ps;"
    )
    .ok();
    writeln!(
        o,
        "{INDENT}{INDENT}{INDENT}{INDENT}if (c.level == 1) c.rising_count++;"
    )
    .ok();
    writeln!(
        o,
        "{INDENT}{INDENT}{INDENT}{INDENT}_last_edge_clock = c.name;"
    )
    .ok();
    writeln!(
        o,
        "{INDENT}{INDENT}{INDENT}{INDENT}_last_edge_cycle = (uint64_t)c.rising_count;"
    )
    .ok();
    writeln!(o, "{INDENT}{INDENT}{INDENT}{INDENT}if (i == 0 && c.level == 1) {{ cycle_count++; _primary_rising = true; }}").ok();
    writeln!(o, "{INDENT}{INDENT}{INDENT}}}").ok();
    writeln!(o, "{INDENT}{INDENT}}}").ok();
    writeln!(o, "{INDENT}{INDENT}ctx.dut->eval();").ok();
    writeln!(
        o,
        "{INDENT}{INDENT}trace.set_timing((uint64_t)now_ps, _last_edge_clock, _last_edge_cycle);"
    )
    .ok();
    writeln!(
        o,
        "{INDENT}{INDENT}if (_primary_rising) {{ for (auto& _svc : _post_eval_services) _svc(); if (!_post_eval_services.empty()) ctx.dut->eval(); }}"
    )
    .ok();
    writeln!(
        o,
        "{INDENT}{INDENT}_harc_trace_dump_at((uint64_t)now_ps, _last_edge_clock, _last_edge_cycle);"
    )
    .ok();
    writeln!(o, "{INDENT}}}").ok();
    writeln!(o, "}}").ok();
    writeln!(o, "").ok();
    writeln!(o, "void HarcSuiteGlue::tick() {{").ok();
    writeln!(o, "{INDENT}if (clocks_.empty()) {{").ok();
    writeln!(o, "{INDENT}{INDENT}// Single-clock / combinational path.").ok();
    writeln!(o, "{INDENT}{INDENT}_harc_eval_negedge(ctx.dut);").ok();
    writeln!(
        o,
        "{INDENT}{INDENT}_harc_trace_dump_next(\"clk\", (uint64_t)cycle_count);"
    )
    .ok();
    writeln!(o, "{INDENT}{INDENT}_harc_eval_posedge(ctx.dut);").ok();
    writeln!(
        o,
        "{INDENT}{INDENT}_harc_trace_dump_next(\"clk\", (uint64_t)(cycle_count + 1));"
    )
    .ok();
    writeln!(o, "{INDENT}{INDENT}cycle_count++;").ok();
    writeln!(
        o,
        "{INDENT}{INDENT}for (auto& _svc : _post_eval_services) _svc();"
    )
    .ok();
    writeln!(
        o,
        "{INDENT}{INDENT}if (!_post_eval_services.empty()) ctx.dut->eval();"
    )
    .ok();
    writeln!(o, "{INDENT}{INDENT}if (!_post_eval_services.empty()) _harc_trace_dump_next(\"clk\", (uint64_t)cycle_count);").ok();
    writeln!(o, "{INDENT}{INDENT}for (auto& _c : _checkers) _c();").ok();
    writeln!(o, "{INDENT}}} else {{").ok();
    writeln!(
        o,
        "{INDENT}{INDENT}// Multi-clock path: one full primary-clock period per tick."
    )
    .ok();
    writeln!(
        o,
        "{INDENT}{INDENT}long long target = now_ps + clocks_[0].half_period_ps * 2;"
    )
    .ok();
    writeln!(o, "{INDENT}{INDENT}eval_clocks_until(target);").ok();
    writeln!(o, "{INDENT}{INDENT}for (auto& _c : _checkers) _c();").ok();
    writeln!(o, "{INDENT}}}").ok();
    writeln!(o, "}}").ok();
    writeln!(o, "").ok();
    Ok(())
}

/// Emit every shared callable once into the common TU as an out-of-line
/// glue member: suite functions, tseqs, hookable component methods
/// (with instance-owned pre/post fan-out), and watchdogs. Body text is
/// produced by the same emitters as the legacy layout — only the
/// signature wrapper differs — so behavior is byte-equivalent per body
/// while preserving per-run ownership.
fn emit_shared_callables(e: &mut Emitter, prep: &SuitePrep) -> Result<(), EmitError> {
    for f in &prep.funcs {
        e.emit_function(f, 0);
    }
    for t in &prep.tseqs {
        e.emit_tseq(t, 0);
    }
    let hooks = collect_hook_registries(prep.file, &prep.component_order);
    let bus_bound = bus_bound_component_types(prep.file);
    for reg in &hooks {
        // Registries carry one entry per side (pre/post); the callable
        // itself is emitted once — on the `pre` entry.
        if !reg.pre {
            continue;
        }
        // Bus-bound hookables stay capsule-local.
        if bus_bound.contains(&reg.comp_ty) {
            continue;
        }
        if !reg.watchdog {
            // Locate the owning decl and reuse the dual-mode emitter.
            for &idx in &prep.component_order {
                match &prep.file.items[idx] {
                    Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) | Item::Scoreboard(c)
                        if c.name.name == reg.comp_ty =>
                    {
                        for ci in &c.items {
                            if let ComponentItem::Hookable(h) = ci {
                                if h.name.name == reg.method {
                                    e.emit_component_method(c, h, 0);
                                }
                            }
                        }
                    }
                    Item::Transactor(t) if t.name.name == reg.comp_ty => {
                        let synth = synth_component_from_transactor(t, true);
                        for ci in &synth.items {
                            if let ComponentItem::Hookable(h) = ci {
                                if h.name.name == reg.method {
                                    e.emit_component_method(&synth, h, 0);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        } else {
            for &idx in &prep.component_order {
                match &prep.file.items[idx] {
                    Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) | Item::Scoreboard(c)
                        if c.name.name == reg.comp_ty =>
                    {
                        for ci in &c.items {
                            if let ComponentItem::Watchdog(w) = ci {
                                if !w.disabled {
                                    e.emit_watchdog(c, w, 0);
                                }
                            }
                        }
                    }
                    Item::Transactor(t) if t.name.name == reg.comp_ty => {
                        let synth = synth_component_from_transactor(t, true);
                        for ci in &synth.items {
                            if let ComponentItem::Watchdog(w) = ci {
                                if !w.disabled {
                                    e.emit_watchdog(&synth, w, 0);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

pub fn emit_split_tests_with_file_prefix(
    file: &SourceFile,
    opts: EmitOpts,
    file_prefix: &str,
    group_size: usize,
) -> Result<SplitCppOutput, EmitError> {
    let group_size = group_size.max(1);
    let lowered = desugar_impl_for_test_in_file(file);
    let test_names: Vec<String> = lowered
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Test(t) => Some(t.name.name.clone()),
            _ => None,
        })
        .collect();
    if test_names.is_empty() {
        return Err(EmitError("no `test` declaration found".into()));
    }
    validate_split_tests_share_dut(&lowered, &test_names)?;
    let suite_probe_catalog = crate::codegen::sv_stub::collect_suite_probes(&lowered)
        .map_err(|error| EmitError(error.to_string()))?;

    let shard_count = test_names.len().div_ceil(group_size);
    let mut files = Vec::with_capacity(shard_count + 1);
    files.push(GeneratedCppFile {
        filename: format!("{file_prefix}main.cpp"),
        contents: emit_split_dispatcher(&test_names),
    });

    for (shard_idx, shard_names) in test_names.chunks(group_size).enumerate() {
        let shard_file = filter_source_to_tests(&lowered, shard_names);
        let cpp = emit_with_opts_and_probe_catalog(
            &shard_file,
            opts.clone(),
            Some(&suite_probe_catalog),
        )?;
        // One test per shard reads better as `test_<name>.cpp`; a bundled
        // shard has no single owning test, so it gets `shard<N>.cpp`.
        let filename = if group_size == 1 {
            format!(
                "{file_prefix}test_{}.cpp",
                sanitize_file_component(&shard_names[0])
            )
        } else {
            format!("{file_prefix}shard{}.cpp", shard_idx + 1)
        };
        files.push(GeneratedCppFile {
            filename,
            contents: strip_generated_dispatcher(&cpp)?,
        });
    }

    Ok(SplitCppOutput { files, test_names })
}

fn filter_source_to_tests(file: &SourceFile, test_names: &[String]) -> SourceFile {
    let mut items = Vec::new();
    let mut item_sources = Vec::new();
    for (index, item) in file.items.iter().enumerate() {
        match item {
            Item::Test(t) if test_names.iter().any(|name| name == &t.name.name) => {
                items.push(item.clone());
                item_sources.push(file.item_source(index));
            }
            Item::Test(_) => {}
            _ => {
                items.push(item.clone());
                item_sources.push(file.item_source(index));
            }
        }
    }
    SourceFile {
        items,
        item_sources,
        sources: file.sources.clone(),
        inner_doc: file.inner_doc.clone(),
        frontmatter: file.frontmatter.clone(),
    }
}

fn strip_generated_dispatcher(cpp: &str) -> Result<String, EmitError> {
    let marker = "\nint main(int argc, char** argv) {";
    let Some(idx) = cpp.rfind(marker) else {
        return Err(EmitError(
            "internal error: generated single-test C++ did not contain dispatcher main()".into(),
        ));
    };
    let mut out = cpp[..idx].trim_end().to_string();
    out.push('\n');
    Ok(out)
}

fn emit_split_dispatcher(test_names: &[String]) -> String {
    let mut out = String::new();
    writeln!(out, "// Auto-generated by harc — do not edit.").ok();
    writeln!(out, "// HARC split-test dispatcher.").ok();
    writeln!(out).ok();
    writeln!(out, "#include <cstring>").ok();
    writeln!(out, "#include \"harc_log_rt.h\"").ok();
    writeln!(out).ok();
    for name in test_names {
        writeln!(out, "extern int run_{name}(int argc, char** argv);").ok();
    }
    writeln!(out).ok();
    writeln!(out, "int main(int argc, char** argv) {{").ok();
    writeln!(
        out,
        "{INDENT}const char* test_sel = harc_rt::log::harc_select_test(argc, argv);"
    )
    .ok();
    let first_test = &test_names[0];
    writeln!(
        out,
        "{INDENT}if (!test_sel) return run_{first_test}(argc, argv);"
    )
    .ok();
    for name in test_names {
        writeln!(
            out,
            "{INDENT}if (std::strcmp(test_sel, \"{name}\") == 0) return run_{name}(argc, argv);"
        )
        .ok();
    }
    let avail_csv = test_names.join(", ");
    writeln!(
        out,
        "{INDENT}harc_rt::log::harc_report_unknown_test(test_sel, \"{avail_csv}\");"
    )
    .ok();
    writeln!(out, "{INDENT}return 1;").ok();
    writeln!(out, "}}").ok();
    out
}

fn validate_split_tests_share_dut(
    file: &SourceFile,
    test_names: &[String],
) -> Result<(), EmitError> {
    let mut shared_dut_type: Option<&str> = None;
    for item in &file.items {
        let Item::Test(test) = item else { continue };
        if !test_names.iter().any(|name| name == &test.name.name) {
            continue;
        }
        for test_item in &test.items {
            let TestItem::Let(l) = test_item else {
                continue;
            };
            if l.name.name != "dut" {
                continue;
            }
            let ty_name = type_simple_name(l.ty.as_ref()).ok_or_else(|| {
                EmitError("`let dut : <Type>` must use a simple named type".into())
            })?;
            match shared_dut_type {
                Some(prev) if prev != ty_name => {
                    return Err(EmitError(format!(
                        "multi-DUT tests in one split binary are out of scope; \
                         test `{}` uses `{}`, but a previous test used `{}`",
                        test.name.name, ty_name, prev,
                    )));
                }
                _ => shared_dut_type = Some(ty_name),
            }
        }
    }
    if shared_dut_type.is_none() {
        return Err(EmitError(
            "expected `let dut : <Type>` declaration in test body".into(),
        ));
    }
    Ok(())
}

/// Build a *focused* `Emitter` for the TB-IR backend's randomize seam.
///
/// The tbir backend (`codegen/tbir`) lowers `randomize` through
/// `Terminator::Randomize`, but the actual Z3-solve emission is v1's
/// (`emit_constraint_solver_block` & friends) — "the constraint runtime
/// is shared; only the call site moves to the IR backend". This builds
/// the minimal Emitter those methods read from: transaction field
/// metadata, enum domains, relation declarations, and the runtime
/// problem-id table. Everything else is empty — the randomize emission
/// touches no other Emitter state. (Listing every field explicitly is
/// deliberate: a future field addition fails to compile here, forcing a
/// reviewer to decide whether the randomize path needs it.)
fn stable_identity_hash(domain: &str, parts: &[&[u8]]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    };
    feed(domain.as_bytes());
    for part in parts {
        feed(&(part.len() as u64).to_le_bytes());
        feed(part);
    }
    hash
}

fn global_const_cpp_name(name: &str) -> String {
    format!(
        "harc_user_const_{}_{:016x}",
        c_ident(name),
        stable_identity_hash("harc-user-const-v1", &[name.as_bytes()])
    )
}

fn global_callable_cpp_name(name: &str) -> String {
    format!(
        "harc_user_callable_{}_{:016x}",
        c_ident(name),
        stable_identity_hash("harc-user-callable-v1", &[name.as_bytes()])
    )
}

fn global_enum_variant_cpp_name(name: &str) -> String {
    format!(
        "harc_user_enum_{}_{:016x}",
        c_ident(name),
        stable_identity_hash("harc-user-enum-v1", &[name.as_bytes()])
    )
}

#[derive(Debug, Clone, Copy)]
struct StableSourceIdentity {
    tag: u64,
    site_seed: u64,
}

fn stable_source_identities(
    file: &SourceFile,
) -> std::collections::HashMap<SourceId, StableSourceIdentity> {
    file.sources
        .iter()
        .map(|source| {
            let parts = [source.name.as_bytes()];
            (
                source.id,
                StableSourceIdentity {
                    tag: stable_identity_hash("harc-source-tag-v2", &parts),
                    site_seed: stable_identity_hash("harc-source-site-v2", &parts),
                },
            )
        })
        .collect()
}

fn stable_component_randomize_site_id(
    source: StableSourceIdentity,
    owner: &str,
    span: Span,
) -> u64 {
    stable_identity_hash(
        "harc-component-randomize-site-v1",
        &[
            &source.site_seed.to_le_bytes(),
            owner.as_bytes(),
            &span.start.to_le_bytes(),
            &span.end.to_le_bytes(),
        ],
    )
}

fn component_randomize_site_owners(
    file: &SourceFile,
) -> std::collections::HashMap<SourceSite, String> {
    crate::solver::problem_table::build_component_scope_problem_table(file)
        .entries
        .into_iter()
        .filter_map(|entry| match entry.source {
            crate::solver::problem_table::TypedSolverProblemSource::RandomizeSite {
                context,
                source_id,
                span,
                ..
            } => Some((SourceSite::new(source_id, span), context)),
            _ => None,
        })
        .collect()
}

/// Everything `emit_with_opts` derives from the merged source before any
/// C++ text is produced. Shared by the legacy single-TU emitter and the
/// common-object split layout so both backends analyze identically.
struct SuitePrep<'a> {
    pub file: &'a SourceFile,
    pub tests: Vec<&'a TestDecl>,
    pub funcs: Vec<&'a FunctionDecl>,
    pub tseqs: Vec<&'a TseqDecl>,
    pub domains: std::collections::HashMap<String, i64>,
    pub dut_type: &'a str,
    pub aggregated_probes: std::collections::HashMap<String, ProbeAccessor>,
    pub shared_component_probes: std::collections::HashMap<String, ProbeAccessor>,
    pub shared_component_probe_widths: std::collections::HashMap<String, u32>,
    pub partial_component_probe_names: std::collections::HashSet<String>,
    pub component_order: Vec<usize>,
    pub runtime_problem_table: crate::solver::runtime::RuntimeProblemTable,
    pub qualified_clock_wait: bool,
    /// Seed state for the per-suite `Emitter` (all FILE-SCOPE shared
    /// maps). Per-test fields stay empty; they're reset+populated in the
    /// per-test emission loop.
    pub emitter_seed: EmitterSeed,
}

fn block_has_qualified_clock_wait(block: &Block) -> bool {
    block.stmts.iter().any(stmt_has_qualified_clock_wait)
}

fn stmt_has_qualified_clock_wait(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Wait { clock: Some(_), .. } => true,
        StmtKind::For(stmt) => block_has_qualified_clock_wait(&stmt.body),
        StmtKind::Repeat(stmt) => block_has_qualified_clock_wait(&stmt.body),
        StmtKind::Loop(body) | StmtKind::While { body, .. } => block_has_qualified_clock_wait(body),
        StmtKind::If(stmt) => {
            block_has_qualified_clock_wait(&stmt.then_block)
                || stmt
                    .elsifs
                    .iter()
                    .any(|(_, body)| block_has_qualified_clock_wait(body))
                || stmt
                    .else_block
                    .as_ref()
                    .is_some_and(block_has_qualified_clock_wait)
        }
        StmtKind::Fork(stmt) => stmt.branches.iter().any(block_has_qualified_clock_wait),
        StmtKind::Parallel(blocks) | StmtKind::Schedule(blocks) => {
            blocks.iter().any(block_has_qualified_clock_wait)
        }
        StmtKind::Select(arms) => arms
            .iter()
            .any(|arm| block_has_qualified_clock_wait(&arm.action)),
        StmtKind::On(handler) => block_has_qualified_clock_wait(&handler.body),
        StmtKind::After { body, .. } => block_has_qualified_clock_wait(body),
        _ => false,
    }
}

fn component_items_have_qualified_clock_wait(items: &[ComponentItem]) -> bool {
    items.iter().any(|item| match item {
        ComponentItem::OnHandler(handler) => block_has_qualified_clock_wait(&handler.body),
        ComponentItem::TargetTlmThread(thread) => block_has_qualified_clock_wait(&thread.body),
        ComponentItem::Hookable(method) => block_has_qualified_clock_wait(&method.body),
        ComponentItem::Lifecycle(_, body) => block_has_qualified_clock_wait(body),
        ComponentItem::Watchdog(watchdog) => block_has_qualified_clock_wait(&watchdog.body),
        _ => false,
    })
}

fn test_items_have_qualified_clock_wait(items: &[TestItem]) -> bool {
    items.iter().any(|item| match item {
        TestItem::Scope(scope) => [
            scope.setup.as_ref(),
            scope.run.as_ref(),
            scope.check.as_ref(),
            scope.teardown.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(block_has_qualified_clock_wait),
        TestItem::Stmt(stmt) => stmt_has_qualified_clock_wait(stmt),
        TestItem::Phase(_, body) => block_has_qualified_clock_wait(body),
        _ => false,
    })
}

fn items_have_qualified_clock_wait(items: &[Item]) -> bool {
    items.iter().any(|item| match item {
        Item::Package(package) => items_have_qualified_clock_wait(&package.items),
        Item::Tseq(tseq) => block_has_qualified_clock_wait(&tseq.body),
        Item::Agent(component)
        | Item::Env(component)
        | Item::Scoreboard(component)
        | Item::Sequencer(component) => component_items_have_qualified_clock_wait(&component.items),
        Item::Test(test) => test_items_have_qualified_clock_wait(&test.items),
        Item::Extend(extend) => match &extend.body {
            ExtendBody::Component(items) => component_items_have_qualified_clock_wait(items),
            ExtendBody::Test(items) => test_items_have_qualified_clock_wait(items),
            ExtendBody::TxnLike(_) => false,
        },
        Item::Function(function) => block_has_qualified_clock_wait(&function.body),
        Item::Transactor(transactor) => {
            component_items_have_qualified_clock_wait(&transactor.items)
                || transactor
                    .when_active
                    .as_deref()
                    .is_some_and(component_items_have_qualified_clock_wait)
        }
        _ => false,
    })
}

fn prepare_suite<'a>(
    file: &'a SourceFile,
    opts: &EmitOpts,
    suite_probe_catalog: Option<&crate::codegen::sv_stub::DutProbeCatalog>,
) -> Result<SuitePrep<'a>, EmitError> {
    // Desugar `impl <name> for <TbType>` tests into the classic
    // `test <name>` form before any other emission work runs. The
    // testbench-bound form (docs/test-ergonomics.md §3.3) folds a
    // testbench's fields + helper methods into the bound test's
    // scope; desugaring synthesizes the equivalent `let dut : ...` /
    // `let _tb : <TbType>` declarations at test scope and rewrites
    // bare-name references inside run/setup/check/teardown bodies
    // to `_tb.<field>` / `<TbType>_<method>(_tb, ...)`. Once
    // desugared, the test looks identical to a classic-form test
    // and threads through the existing pipeline unchanged.
    //
    // Callers pass an already-desugared file (`emit_with_opts` keeps
    // ownership of the desugared clone and forwards a reference), so
    // this function itself is pure analysis over `file`.
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
    let runtime_problem_table =
        crate::solver::runtime::RuntimeProblemTable::from_typed_solver_table(
            &typed_solver_problem_table,
        );

    // Collect ALL `test` items — every one becomes a `run_<TestName>`
    // function in the emitted binary, and the dispatcher `main()` at
    // the end picks one based on `--test <name>` or the `HARC_TEST`
    // env var (Phase 1b of docs/separate-compilation-plan.md).
    // Multi-test in one binary lets `harc sim --test foo` then
    // `harc sim --test bar` share Verilator output (the .cpp is
    // byte-identical across selectors).
    let tests: Vec<&TestDecl> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Test(t) => Some(t),
            _ => None,
        })
        .collect();
    if tests.is_empty() {
        return Err(EmitError("no `test` declaration found".into()));
    }

    // Validate the suite's DUT/probe contract once, using the same catalog
    // that drives the bind stub and TB-IR's shared component lowering.
    let local_probe_catalog = crate::codegen::sv_stub::collect_suite_probes(file)
        .map_err(|error| EmitError(error.to_string()))?;
    let semantic_probe_catalog = suite_probe_catalog.unwrap_or(&local_probe_catalog);
    let mut shared_dut_type = None;
    for test in &tests {
        for item in &test.items {
            let TestItem::Let(let_stmt) = item else {
                continue;
            };
            if let_stmt.name.name != "dut" {
                continue;
            }
            let current = type_simple_name(let_stmt.ty.as_ref()).ok_or_else(|| {
                EmitError("`let dut : <Type>` must use a simple named type".into())
            })?;
            if let Some(previous) = shared_dut_type {
                if previous != current {
                    return Err(EmitError(format!(
                        "multi-DUT tests in one binary are out of scope for v0; \
                         test `{}` uses `{}`, but a previous test used `{}`",
                        test.name.name, current, previous
                    )));
                }
            } else {
                shared_dut_type = Some(current);
            }
        }
    }
    if let Some(probe_dut_type) = local_probe_catalog.dut_type.as_deref() {
        debug_assert_eq!(shared_dut_type, Some(probe_dut_type));
    }
    if let Some(probe_dut_type) = semantic_probe_catalog.dut_type.as_deref() {
        debug_assert_eq!(shared_dut_type, Some(probe_dut_type));
    }
    let aggregated_probes = local_probe_catalog
        .probes
        .iter()
        .map(|probe| {
            (
                probe.name.name.clone(),
                ProbeAccessor {
                    read: crate::codegen::sv_stub::mangled_accessor(
                        local_probe_catalog
                            .dut_type
                            .as_deref()
                            .expect("a probe declaration always has a DUT type"),
                        &probe.name.name,
                    ),
                    force: probe.force,
                },
            )
        })
        .collect();
    let shared_component_probes = semantic_probe_catalog
        .shared_component_probes
        .iter()
        .map(|probe| {
            (
                probe.name.name.clone(),
                ProbeAccessor {
                    read: crate::codegen::sv_stub::mangled_accessor(
                        semantic_probe_catalog
                            .dut_type
                            .as_deref()
                            .expect("a probe declaration always has a DUT type"),
                        &probe.name.name,
                    ),
                    force: probe.force,
                },
            )
        })
        .collect();
    let shared_component_probe_widths = semantic_probe_catalog
        .shared_component_probes
        .iter()
        .filter_map(|probe| {
            crate::codegen::sv_stub::probe_width_bits(&probe.ty)
                .map(|width| (probe.name.name.clone(), width))
        })
        .collect();

    // Per-test metadata (custom_phases, other_lets, ...) is derived
    // inside the per-test emission loop further down. The few
    // file-scope code paths that previously referenced `test_decl`
    // (e.g. when computing tseq/funcs/etc.) keep working off any of
    // the tests — those collections walk `file.items`, not
    // `test.items`. We bind `test_decl` to the alphabetically-first
    // test only so leftover single-test references compile during
    // the staged migration.
    let _test_decl = tests[0];

    // Collect top-level functions — emitted as lambdas inside main() so they
    // can capture `dut` and `tick` lexically.
    let funcs: Vec<&FunctionDecl> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Function(f) => Some(f),
            _ => None,
        })
        .collect();

    // Collect `tseq` declarations — same hoisting strategy as functions,
    // emitted as `std::function`-shaped lambdas returning `std::vector<T>`
    // built up via `yield`.
    let tseqs: Vec<&TseqDecl> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Tseq(t) => Some(t),
            _ => None,
        })
        .collect();

    // Index `domain Foo freq_mhz: N end domain Foo` decls so a `clock X =
    // Foo` reference can resolve N to a wall-clock period (1/N µs → ps).
    let mut domains: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for it in &file.items {
        if let Item::Domain(d) = it {
            for f in &d.fields {
                if f.name.name == "freq_mhz" {
                    if let ExprKind::Int(s) = &*f.value.kind {
                        if let Ok(n) = s.replace('_', "").parse::<i64>() {
                            // Period in ps = 1_000_000 / freq_mhz.
                            if n > 0 {
                                domains.insert(d.name.name.clone(), 1_000_000 / n);
                            }
                        }
                    }
                }
            }
        }
    }

    // Per-test metadata (dut_type for V<top>* construction, other_lets,
    // bare_stmts, explicit_run, clocks, probe_accessors) is derived
    // inside the per-test emission loop further down (search for
    // `for test in &tests`). The shared `dut_type` resolved above is
    // used by file-scope code paths that need the DUT module name.

    // Collect records + enums + scoreboards + monitors for typed-let
    // emission. Transactions and structs both lower as value-records;
    // transaction-specific behavior (keeps, transactor-facing metadata)
    // is layered on top.
    let mut structs = std::collections::HashSet::new();
    let mut transactions = std::collections::HashSet::new();
    let mut scoreboards = std::collections::HashSet::new();
    let mut components: std::collections::HashMap<String, ComponentDecl> =
        std::collections::HashMap::new();
    let mut covergroups: std::collections::HashMap<String, CovergroupDecl> =
        std::collections::HashMap::new();
    let mut buses: std::collections::HashMap<String, BusDecl> = std::collections::HashMap::new();
    let mut transactors: std::collections::HashMap<String, TransactorDecl> =
        std::collections::HashMap::new();
    // RAL regblocks indexed by name. Populated in the pre-pass below;
    // consulted at codegen time to (a) emit one POD mirror struct +
    // constexpr address table per regblock at file scope, and (b)
    // lower `regs.NAME` field accesses against the bound helper.
    let mut regblocks: std::collections::HashMap<String, RegblockDecl> =
        std::collections::HashMap::new();
    // RAL addrmaps indexed by name. Chip-level container of regblock
    // instances at distinct base addresses. See docs/ral-support.md §4.
    let mut addrmaps: std::collections::HashMap<String, AddrmapDecl> =
        std::collections::HashMap::new();
    let mut txn_fields: std::collections::HashMap<String, Vec<TxnFieldInfo>> =
        std::collections::HashMap::new();
    let mut record_fields: std::collections::HashMap<String, Vec<Field>> =
        std::collections::HashMap::new();
    let mut record_bodies: std::collections::HashMap<String, Vec<TxnBodyItem>> =
        std::collections::HashMap::new();
    // Per-transaction `keep` constraints (spec §4 — "constraints are
    // relations, not directives"). Collected here so `randomize(t)`
    // and `randomize(t) with …` both route them through the Z3 solver
    // path. Without this collection the keep blocks parse but emit
    // zero C++, a silent footgun: `keep len in [1..256]` would
    // randomize freely up to the field's full width.
    let mut txn_keeps: std::collections::HashMap<String, Vec<Expr>> =
        std::collections::HashMap::new();
    let mut enums = std::collections::HashMap::new();
    let mut enum_domains: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    // Global variant-name → index map. Used by the Z3 constraint
    // translator to resolve bare `WRAP` / `INCR` / etc. into their
    // numeric encoding. v0 assumes variant names are globally
    // unique; collisions take the first-declared mapping (warned
    // about in the parser if we add that). Maps to i64 so negative
    // signed values fit naturally; the solver widens at use site.
    let mut enum_variants: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    let const_elab = crate::constraints::elaborate_constraints(file);
    let consts: std::collections::HashMap<_, _> = const_elab.consts.into_iter().collect();
    // Relation declarations indexed by name (spec §4.2). At constraint-
    // emit time, any `Call(Ident(R), args)` whose name is in this map
    // is inlined: the formal parameters substitute into R's body and
    // each expression becomes its own constraint added to the Z3
    // solver block. Recursive expansion handles relation-aliases-of-
    // relations (`relation A(t) = B(t) && t.x == 0`).
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
    for it in &file.items {
        match it {
            Item::Relation(r) => {
                relations.insert(r.name.name.clone(), r.clone());
            }
            _ => {}
        }
    }
    for it in &file.items {
        if let Item::Enum(e) = it {
            enums.insert(e.name.name.clone(), e.variants.len());
            enum_domains.insert(
                e.name.name.clone(),
                e.variants.iter().map(|v| v.name.clone()).collect(),
            );
            for (i, v) in e.variants.iter().enumerate() {
                enum_variants.entry(v.name.clone()).or_insert(i as i64);
            }
        }
    }
    for it in &file.items {
        match it {
            Item::Struct(s) => {
                record_fields.insert(s.name.name.clone(), s.fields.clone());
                record_bodies.insert(s.name.name.clone(), s.body.clone());
            }
            Item::Transaction(t) => {
                record_fields.insert(t.name.name.clone(), txn_direct_fields(&t.body));
                record_bodies.insert(t.name.name.clone(), t.body.clone());
            }
            Item::Scoreboard(c) => {
                scoreboards.insert(c.name.name.clone());
                components.insert(c.name.name.clone(), c.clone());
            }
            Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) => {
                components.insert(c.name.name.clone(), c.clone());
            }
            Item::Covergroup(g) => {
                covergroups.insert(g.name.name.clone(), g.clone());
            }
            Item::Bus(b) => {
                buses.insert(b.name.name.clone(), b.clone());
            }
            Item::Transactor(t) => {
                transactors.insert(t.name.name.clone(), t.clone());
            }
            Item::Regblock(r) => {
                regblocks.insert(r.name.name.clone(), r.clone());
            }
            Item::Addrmap(a) => {
                addrmaps.insert(a.name.name.clone(), a.clone());
            }
            _ => {}
        }
    }

    // Validate addrmap structure: alias targets resolve, no chained
    // aliases, instance windows don't overlap (for pairs with size,
    // skipping any pair where one side aliases the other or both
    // alias the same target).
    for a in addrmaps.values() {
        if let Some(err) = check_addrmap_aliases(a) {
            return Err(EmitError(err));
        }
        if let Some(err) = check_addrmap_overlap(a) {
            return Err(EmitError(err));
        }
    }

    // Enforce: a transactor's always-on body cannot drive DUT signals.
    // Drive-side hookables / on-handlers must live inside `when active`.
    // Without this check, a `passive` instance — which has its
    // `when_active` body elided — would still emit the drive code and
    // could wire an observer to the bus (block-level→chip-level TB
    // reuse footgun). See `check_transactor_no_drive_in_always_on_body`
    // for the full rationale and detection model.
    for t in transactors.values() {
        if let Some(err) = check_transactor_no_drive_in_always_on_body(
            t,
            &transactors,
            &components,
            &scoreboards,
            &covergroups,
            &buses,
            &regblocks,
            &addrmaps,
        ) {
            return Err(EmitError(err));
        }
    }

    for it in &file.items {
        match it {
            Item::Transaction(t) => {
                transactions.insert(t.name.name.clone());
                let fields =
                    flatten_txn_body_field_infos(&t.body, &record_fields, &enums, &enum_domains);
                txn_fields.insert(t.name.name.clone(), fields);
                // Collect transaction-level `keep` constraints. Keeps nested
                // inside `when` subtype bodies lower as guarded implications:
                // `when G { keep K }` contributes `(!G) || K`, so the
                // constraint participates only when the discriminator selects
                // that subtype. Full tagged-ADT solver modeling remains a
                // future backend step; this keeps the current flat solver path
                // semantically honest for keeps over already-visible fields.
                let keeps = collect_record_keeps(&t.body, &record_bodies, &record_fields);
                if !keeps.is_empty() {
                    txn_keeps.insert(t.name.name.clone(), keeps);
                }
            }
            Item::Struct(s) => {
                structs.insert(s.name.name.clone());
                let fields =
                    flatten_record_field_infos(&s.fields, &record_fields, &enums, &enum_domains);
                txn_fields.insert(s.name.name.clone(), fields);
                let keeps = collect_record_keeps(&s.body, &record_bodies, &record_fields);
                if !keeps.is_empty() {
                    txn_keeps.insert(s.name.name.clone(), keeps);
                }
            }
            Item::Enum(e) => {
                enums.insert(e.name.name.clone(), e.variants.len());
            }
            _ => {}
        }
    }

    // Seed let_types from test-level `let` decls so `randomize(t)` works
    // when the let appears at test scope (above any scope sim / bare stmts).
    // Also seed let_modes from any `: T active`/`: T passive` annotation;
    // the call-site passive-mode check (further down, in emit_expr) walks
    // this map to determine the effective mode of `<env>.<sub>...<xact>`
    // paths whose root resolves to a let-bound name.
    // Build the property name → body table.
    let mut properties = std::collections::HashMap::new();
    for it in &file.items {
        if let Item::Property(p) = it {
            properties.insert(p.name.name.clone(), p.body.clone());
        }
    }

    // Construct the emitter seed with all FILE-SCOPE shared state.
    // Per-test fields (let_types, let_modes, pointer_vars, probes,
    // clock_names, bus_bindings, bus_remap, covers, actor_threads,
    // field_subs, driver_bus_for_hookables, let_helper) are
    // intentionally left empty here — they're reset+populated inside
    // the per-test emission loop of whichever backend consumes the
    // seed.
    let emitter_seed = EmitterSeed {
        vec_lane_widths: opts.effective_vec_lane_widths(),
        transactions,
        structs,
        scoreboards,
        components,
        covergroups,
        txn_fields,
        record_fields,
        enums,
        enum_variants,
        consts,
        const_widths,
        properties,
        function_names: funcs.iter().map(|f| f.name.name.clone()).collect(),
        extern_function_names: file
            .items
            .iter()
            .filter_map(|item| match item {
                Item::ExternFn(function) => Some(function.name.name.clone()),
                _ => None,
            })
            .collect(),
        tseq_names: tseqs.iter().map(|t| t.name.name.clone()).collect(),
        buses,
        dut_bus_port_overrides: opts.dut_bus_port_overrides.clone(),
        mt: opts.mt,
        transactors,
        txn_keeps,
        relations,
        regblocks,
        addrmaps,
        runtime_randomize_problem_ids,
        source_identities: stable_source_identities(file),
        randomize_site_owners: component_randomize_site_owners(file),
    };
    let dut_type: &'a str = shared_dut_type
        .ok_or_else(|| EmitError("expected `let dut : <Type>` declaration in test body".into()))?;
    Ok(SuitePrep {
        file,
        tests,
        funcs,
        tseqs,
        domains,
        dut_type,
        aggregated_probes,
        shared_component_probes,
        shared_component_probe_widths,
        partial_component_probe_names: semantic_probe_catalog.partial_component_probe_names.clone(),
        component_order: topo_sort_component_indices(file),
        runtime_problem_table,
        qualified_clock_wait: items_have_qualified_clock_wait(&file.items),
        emitter_seed,
    })
}

/// FILE-SCOPE analysis results moved into an `Emitter`. Split out of the
/// `Emitter` literal so `prepare_suite` can produce it for both the legacy
/// single-TU backend and the common-object split layout.
#[derive(Clone)]
struct EmitterSeed {
    vec_lane_widths: std::collections::HashMap<String, u32>,
    transactions: std::collections::HashSet<String>,
    structs: std::collections::HashSet<String>,
    scoreboards: std::collections::HashSet<String>,
    components: std::collections::HashMap<String, ComponentDecl>,
    covergroups: std::collections::HashMap<String, CovergroupDecl>,
    txn_fields: std::collections::HashMap<String, Vec<TxnFieldInfo>>,
    record_fields: std::collections::HashMap<String, Vec<Field>>,
    enums: std::collections::HashMap<String, usize>,
    enum_variants: std::collections::HashMap<String, i64>,
    consts: std::collections::HashMap<String, crate::constraints::IntegerConstSchema>,
    const_widths: std::collections::HashMap<String, u32>,
    properties: std::collections::HashMap<String, crate::ast::Expr>,
    function_names: std::collections::HashSet<String>,
    extern_function_names: std::collections::HashSet<String>,
    tseq_names: std::collections::HashSet<String>,
    buses: std::collections::HashMap<String, BusDecl>,
    dut_bus_port_overrides:
        std::collections::HashMap<String, std::collections::HashMap<String, i64>>,
    mt: bool,
    transactors: std::collections::HashMap<String, TransactorDecl>,
    txn_keeps: std::collections::HashMap<String, Vec<Expr>>,
    relations: std::collections::HashMap<String, RelationDecl>,
    regblocks: std::collections::HashMap<String, RegblockDecl>,
    addrmaps: std::collections::HashMap<String, AddrmapDecl>,
    runtime_randomize_problem_ids: std::collections::HashMap<SourceSite, u64>,
    source_identities: std::collections::HashMap<SourceId, StableSourceIdentity>,
    randomize_site_owners: std::collections::HashMap<SourceSite, String>,
}

fn empty_emitter_seed() -> EmitterSeed {
    EmitterSeed {
        vec_lane_widths: Default::default(),
        transactions: Default::default(),
        structs: Default::default(),
        scoreboards: Default::default(),
        components: Default::default(),
        covergroups: Default::default(),
        txn_fields: Default::default(),
        record_fields: Default::default(),
        enums: Default::default(),
        enum_variants: Default::default(),
        consts: Default::default(),
        const_widths: Default::default(),
        properties: Default::default(),
        function_names: Default::default(),
        extern_function_names: Default::default(),
        tseq_names: Default::default(),
        buses: Default::default(),
        dut_bus_port_overrides: Default::default(),
        mt: false,
        transactors: Default::default(),
        txn_keeps: Default::default(),
        relations: Default::default(),
        regblocks: Default::default(),
        addrmaps: Default::default(),
        runtime_randomize_problem_ids: Default::default(),
        source_identities: Default::default(),
        randomize_site_owners: Default::default(),
    }
}

/// One per-instance hook registry (pre/post callback vector) owned by
/// the suite runtime under the common layout. `vector` is the generated
/// C++ member/field name, identical to the historical run-scope variable
/// so capsule registrations compile unchanged.
#[derive(Debug, Clone)]
pub(crate) struct HookRegistryDesc {
    pub comp_ty: String,
    pub method: String,
    pub watchdog: bool,
    pub pre: bool,
}
impl HookRegistryDesc {
    pub fn vector(&self) -> String {
        if self.watchdog {
            format!("{}_watchdog_{}", self.comp_ty, self.side_name())
        } else {
            format!("{}_{}_{}", self.comp_ty, self.method, self.side_name())
        }
    }
    fn side_name(&self) -> &'static str {
        if self.pre {
            "pre"
        } else {
            "post"
        }
    }
}

/// Enumerate every hookable method / watchdog across the dependency-
/// sorted component list. Deterministic: derived purely from source AST
/// order via `component_order`, independent of test set or emission
/// job count.
fn collect_hook_registries(file: &SourceFile, component_order: &[usize]) -> Vec<HookRegistryDesc> {
    let mut out = Vec::new();
    for &idx in component_order {
        match &file.items[idx] {
            Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) | Item::Scoreboard(c) => {
                for ci in &c.items {
                    if let ComponentItem::Hookable(h) = ci {
                        for pre in [true, false] {
                            out.push(HookRegistryDesc {
                                comp_ty: c.name.name.clone(),
                                method: h.name.name.clone(),
                                watchdog: false,
                                pre,
                            });
                        }
                    }
                    if let ComponentItem::Watchdog(w) = ci {
                        if w.disabled {
                            continue;
                        }
                        for pre in [true, false] {
                            out.push(HookRegistryDesc {
                                comp_ty: c.name.name.clone(),
                                method: String::new(),
                                watchdog: true,
                                pre,
                            });
                        }
                    }
                }
            }
            Item::Transactor(t) => {
                let synth = synth_component_from_transactor(t, /*include_active*/ true);
                for ci in &synth.items {
                    if let ComponentItem::Hookable(h) = ci {
                        for pre in [true, false] {
                            out.push(HookRegistryDesc {
                                comp_ty: synth.name.name.clone(),
                                method: h.name.name.clone(),
                                watchdog: false,
                                pre,
                            });
                        }
                    }
                    if let ComponentItem::Watchdog(w) = ci {
                        if w.disabled {
                            continue;
                        }
                        for pre in [true, false] {
                            out.push(HookRegistryDesc {
                                comp_ty: synth.name.name.clone(),
                                method: String::new(),
                                watchdog: true,
                                pre,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn emitter_from_seed(seed: EmitterSeed) -> Emitter {
    Emitter {
        out: String::new(),
        errors: Vec::new(),
        declared_value_names: Default::default(),
        internal_reserved_names: Default::default(),
        pointer_vars: std::collections::HashSet::new(),
        let_types: std::collections::HashMap::new(),
        let_widths: std::collections::HashMap::new(),
        vec_lane_widths: seed.vec_lane_widths,
        let_modes: std::collections::HashMap::new(),
        transactions: seed.transactions,
        structs: seed.structs,
        scoreboards: seed.scoreboards,
        components: seed.components,
        covergroups: seed.covergroups,
        txn_fields: seed.txn_fields,
        record_fields: seed.record_fields,
        enums: seed.enums,
        enum_variants: seed.enum_variants,
        unsafe_variants: Default::default(),
        consts: seed.consts,
        const_widths: seed.const_widths,
        properties: seed.properties,
        prop_subs: std::collections::HashMap::new(),
        event_types: std::collections::HashMap::new(),
        field_subs: std::collections::HashMap::new(),
        covers: Vec::new(),
        clock_names: Vec::new(),
        current_yield_target: None,
        current_unique_tseq_registry: None,
        function_names: seed.function_names,
        extern_function_names: seed.extern_function_names,
        tseq_names: seed.tseq_names,
        buses: seed.buses,
        bus_bindings: std::collections::HashMap::new(),
        bus_param_envs: std::collections::HashMap::new(),
        dut_bus_port_overrides: seed.dut_bus_port_overrides,
        bus_remap: std::collections::HashMap::new(),
        pending_tlm_forks: Vec::new(),
        next_tlm_fork_tag: std::collections::HashMap::new(),
        in_coroutine: false,
        flavor: Flavor::Legacy,
        per_run_cells: Vec::new(),
        glue_callees: std::collections::HashSet::new(),
        actor_threads: Vec::new(),
        mt: seed.mt,
        driver_bus_for_hookables: std::collections::HashMap::new(),
        transactors: seed.transactors,
        current_component_instance: None,
        current_component_method: None,
        txn_keeps: seed.txn_keeps,
        relations: seed.relations,
        probes: std::collections::HashMap::new(),
        probe_widths: std::collections::HashMap::new(),
        partial_probe_names: std::collections::HashSet::new(),
        probe_receivers: std::collections::HashSet::new(),
        probe_shadow_scopes: Vec::new(),
        probe_dut_type: None,
        shadowed_lets: std::collections::HashSet::new(),
        regblocks: seed.regblocks,
        addrmaps: seed.addrmaps,
        let_helper: std::collections::HashMap::new(),
        runtime_randomize_problem_ids: seed.runtime_randomize_problem_ids,
        source_identities: seed.source_identities,
        randomize_site_owners: seed.randomize_site_owners,
        current_source_id: SourceId::default(),
        randomize_target_sentinel: None,
        randomize_context_sentinel: None,
        randomize_state_sentinel: None,
        randomize_context_ident: "ctx".to_string(),
        capsule_state_ident: "_harc_randomize_state".to_string(),
    }
}

pub fn emit_with_opts(file: &SourceFile, opts: EmitOpts) -> Result<String, EmitError> {
    emit_with_opts_and_probe_catalog(file, opts, None)
}

fn emit_with_opts_and_probe_catalog(
    file: &SourceFile,
    opts: EmitOpts,
    suite_probe_catalog: Option<&crate::codegen::sv_stub::DutProbeCatalog>,
) -> Result<String, EmitError> {
    // Desugar `impl <name> for <TbType>` tests into the classic
    // `test <name>` form before any other emission work runs. The
    // testbench-bound form (docs/test-ergonomics.md §3.3) folds a
    // testbench's fields + helper methods into the bound test's
    // scope; desugaring synthesizes the equivalent `let dut : ...` /
    // `let _tb : <TbType>` declarations at test scope and rewrites
    // bare-name references inside run/setup/check/teardown bodies
    // to `_tb.<field>` / `<TbType>_<method>(_tb, ...)`. Once
    // desugared, the test looks identical to a classic-form test
    // and threads through the existing pipeline unchanged.
    let desugared = desugar_impl_for_test_in_file(file);
    // Route `Vec<Record, N>` fields through the scalar-`Vec` emission
    // path (see `normalize_vec_record_elem_fields`, harc#522).
    let normalized = normalize_vec_record_elem_fields(&desugared);
    let file = &normalized;
    let mut prep = prepare_suite(file, &opts, suite_probe_catalog)?;
    let tests = &prep.tests;
    let seed = std::mem::replace(&mut prep.emitter_seed, empty_emitter_seed());
    let mut cell_plan = emitter_from_seed(seed.clone());
    for test in tests.iter() {
        cell_plan.emit_test_run(&prep, test)?;
    }
    if !cell_plan.errors.is_empty() {
        return Err(EmitError(cell_plan.errors.join("\n")));
    }
    cell_plan
        .per_run_cells
        .sort_by(|lhs, rhs| lhs.tag.cmp(&rhs.tag));
    cell_plan
        .per_run_cells
        .dedup_by(|lhs, rhs| lhs.tag == rhs.tag && lhs.ctype == rhs.ctype && lhs.init == rhs.init);
    for pair in cell_plan.per_run_cells.windows(2) {
        if pair[0].tag == pair[1].tag {
            return Err(EmitError(format!(
                "self-contained per-run cell `{}` has conflicting declarations",
                pair[0].tag
            )));
        }
    }
    let mut e = emitter_from_seed(seed);
    e.per_run_cells = cell_plan.per_run_cells;

    e.emit_file_scope_scaffolding(&prep)?;

    // Per-test entry points. Each `Item::Test` in source gets its
    // own `int run_<TestName>(argc, argv)` function. The dispatcher
    // `int main()` below picks one based on `--test <name>` /
    // `HARC_TEST` env. Each run function owns its DUT pointer,
    // scheduler, `_checkers` vector — tests run against fresh
    // Verilator instances, sequentially (one binary, dispatched).
    // See docs/separate-compilation-plan.md for the future per-test
    // `.o` direction (Phase 2).
    for test in tests.iter() {
        e.emit_test_run(&prep, test)?;
    }

    // Dispatcher `main()` (Phase 1b). One branch per test: the
    // dispatcher reads `--test <name>` from argv (preferred) or the
    // `HARC_TEST` env var, then `return run_<TestName>(argc, argv)`.
    // When unset, dispatches to the alphabetically-first test
    // (deterministic — matches the sort key in merge_for_sim).
    writeln!(e.out, "int main(int argc, char** argv) {{").ok();
    writeln!(
        e.out,
        "{INDENT}const char* test_sel = harc_rt::log::harc_select_test(argc, argv);"
    )
    .ok();
    // Default (no --test, no HARC_TEST) → run the first test.
    let first_test = &tests[0].name.name;
    writeln!(
        e.out,
        "{INDENT}if (!test_sel) return run_{first_test}(argc, argv);"
    )
    .ok();
    // Branches for every test.
    for t in tests.iter() {
        let n = &t.name.name;
        writeln!(
            e.out,
            "{INDENT}if (std::strcmp(test_sel, \"{n}\") == 0) return run_{n}(argc, argv);"
        )
        .ok();
    }
    // Build the human-readable "available tests" list for the
    // unknown-test error message.
    let avail: Vec<&str> = tests.iter().map(|t| t.name.name.as_str()).collect();
    let avail_csv = avail.join(", ");
    writeln!(
        e.out,
        "{INDENT}harc_rt::log::harc_report_unknown_test(test_sel, \"{avail_csv}\");"
    )
    .ok();
    writeln!(e.out, "{INDENT}return 1;").ok();
    writeln!(e.out, "}}").ok();

    if !e.errors.is_empty() {
        return Err(EmitError(e.errors.join("\n")));
    }
    Ok(e.out)
}

/// Build a synthetic `ComponentDecl` view of a transactor for codegen
/// reuse. The transactor's always-present `items` + (optionally) its
/// `when_active` items become a single component-shaped item list that
/// existing emit paths (`emit_component_struct`,
/// `emit_bound_monitor_actors`, `try_emit_bound_driver_actor`) can
/// consume without modification. `include_active = false` produces the
/// passive view: only the always-present body, no `when active` fields
/// or handlers — which is what gets used for `let xact : T passive`
/// instantiations so passive instances don't even attempt to spawn the
/// active driver actor.
///
/// `kind: ComponentKind::Driver` is a placeholder — the existing
/// component codegen paths only consult `kind` for the tag prefix in
/// diagnostic strings; correctness doesn't depend on the choice.
fn synth_component_from_transactor(t: &TransactorDecl, include_active: bool) -> ComponentDecl {
    let mut items = t.items.clone();
    if include_active {
        if let Some(active) = &t.when_active {
            items.extend(active.clone());
        }
    }
    ComponentDecl {
        kind: ComponentKind::Transactor,
        name: t.name.clone(),
        params: t.params.clone(),
        bound_to: t.bound_to.clone(),
        items,
        span: t.span,
        doc: t.doc.clone(),
        inner_doc: t.inner_doc.clone(),
    }
}

fn type_simple_name(t: Option<&TypeExpr>) -> Option<&str> {
    match t? {
        TypeExpr::Named { name, .. } => name.segments.last().map(|s| s.name.as_str()),
        _ => None,
    }
}

/// Collect every method-name string that appears as the callee of
/// a `Call { callee: Field { name, .. }, .. }` expression anywhere
/// inside the given block (recursively). Used by
/// `topo_sort_component_indices` to discover cross-component
/// hookable calls that don't go through a field-typed receiver — see
/// arch-com#447 §8.
fn collect_called_method_names(block: &Block, out: &mut std::collections::HashSet<String>) {
    fn visit_expr(e: &Expr, out: &mut std::collections::HashSet<String>) {
        if let ExprKind::Call { callee, args } = &*e.kind {
            if let ExprKind::Field { name, .. } = &*callee.kind {
                out.insert(name.name.clone());
            }
            visit_expr(callee, out);
            for a in args {
                let arg_expr = match a {
                    CallArg::Expr(ex) => ex,
                    CallArg::Named { value, .. } => value,
                };
                visit_expr(arg_expr, out);
            }
            return;
        }
        match &*e.kind {
            ExprKind::Field { target, .. }
            | ExprKind::Cast { expr: target, .. }
            | ExprKind::Unary { expr: target, .. }
            | ExprKind::HashHash { expr: target, .. }
            | ExprKind::SeqRepeat { expr: target, .. }
            | ExprKind::ForkCall { call: target } => {
                visit_expr(target, out);
            }
            ExprKind::Index { target, index } => {
                visit_expr(target, out);
                visit_expr(index, out);
            }
            ExprKind::BitSlice { target, hi, lo } => {
                visit_expr(target, out);
                visit_expr(hi, out);
                visit_expr(lo, out);
            }
            ExprKind::Send { target, value } => {
                visit_expr(target, out);
                visit_expr(value, out);
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                visit_expr(lhs, out);
                visit_expr(rhs, out);
            }
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => {
                visit_expr(cond, out);
                visit_expr(then_branch, out);
                visit_expr(else_branch, out);
            }
            ExprKind::RangeLit { lo, hi } => {
                if let Some(e) = lo {
                    visit_expr(e, out);
                }
                if let Some(e) = hi {
                    visit_expr(e, out);
                }
            }
            // Leaf / unsupported-for-our-purposes kinds — no nested
            // `Expr` children that could host a call we'd care about,
            // or non-call shapes (literals, idents, etc.).
            _ => {}
        }
    }
    fn visit_block(b: &Block, out: &mut std::collections::HashSet<String>) {
        for s in &b.stmts {
            visit_stmt(s, out);
        }
    }
    fn visit_stmt(s: &Stmt, out: &mut std::collections::HashSet<String>) {
        match &s.kind {
            StmtKind::Let(l) => {
                if let Some(e) = &l.value {
                    visit_expr(e, out);
                }
            }
            StmtKind::Assign { target, value } | StmtKind::Send { target, value } => {
                visit_expr(target, out);
                visit_expr(value, out);
            }
            StmtKind::For(f) => {
                visit_expr(&f.iter, out);
                visit_block(&f.body, out);
            }
            StmtKind::Repeat(r) => {
                visit_expr(&r.count, out);
                visit_block(&r.body, out);
            }
            StmtKind::Loop(b) => visit_block(b, out),
            StmtKind::While { cond, body, .. } => {
                visit_expr(cond, out);
                visit_block(body, out);
            }
            StmtKind::If(i) => {
                visit_expr(&i.cond, out);
                visit_block(&i.then_block, out);
                for (c, b) in &i.elsifs {
                    visit_expr(c, out);
                    visit_block(b, out);
                }
                if let Some(b) = &i.else_block {
                    visit_block(b, out);
                }
            }
            StmtKind::Fork(f) => {
                for b in &f.branches {
                    visit_block(b, out);
                }
            }
            StmtKind::Parallel(bs) | StmtKind::Schedule(bs) => {
                for b in bs {
                    visit_block(b, out);
                }
            }
            StmtKind::Select(arms) => {
                for arm in arms {
                    visit_expr(&arm.event, out);
                    visit_block(&arm.action, out);
                }
            }
            StmtKind::Emit { args, .. }
            | StmtKind::Log { args, .. }
            | StmtKind::LogF { args, .. } => {
                for a in args {
                    let e = match a {
                        CallArg::Expr(ex) => ex,
                        CallArg::Named { value, .. } => value,
                    };
                    visit_expr(e, out);
                }
            }
            StmtKind::Yield(e) | StmtKind::Release(e) => visit_expr(e, out),
            StmtKind::Return(opt) => {
                if let Some(e) = opt {
                    visit_expr(e, out);
                }
            }
            StmtKind::Randomize {
                target, with_body, ..
            } => {
                visit_expr(target, out);
                for e in with_body {
                    visit_expr(e, out);
                }
            }
            StmtKind::Expr(e) => visit_expr(e, out),
            StmtKind::After { duration, body, .. } => {
                visit_expr(duration, out);
                visit_block(body, out);
            }
            StmtKind::Assert(v) | StmtKind::Assume(v) | StmtKind::Cover(v) => {
                if let Some(e) = &v.expr {
                    visit_expr(e, out);
                }
            }
            StmtKind::On(h) => {
                visit_block(&h.body, out);
            }
            // `JoinAll`, `Break`, `Continue`, `Apply` have no Expr/Block
            // children we need to visit for call discovery; ditto for
            // any future variant — when a new variant lands the topo
            // sort safely under-covers it (existing field-type edges
            // still apply).
            _ => {}
        }
    }
    visit_block(block, out);
}

/// Topologically sort the indices of `file.items` that hold a
/// component-shaped declaration (`Agent` / `Env` / `Sequencer` /
/// `Scoreboard` / `Transactor`) so that every by-value field-type
/// reference between two items in the set names a type emitted
/// earlier in the returned order. Items whose types are not part of
/// the set (DUT modules, transactions, structs, enums, covergroups)
/// are ignored for dependency purposes — those have their own emit
/// ordering before this set lands. Non-matching items are dropped
/// entirely from the result; the caller iterates and pattern-matches
/// to dispatch (consumers do `match` on `Item::Agent(_) | Item::Env(_)
/// | Item::Sequencer(_) | Item::Scoreboard(_) | Item::Transactor(_)`).
///
/// Ordering rules (Kahn-style with source-order tie-breaking):
/// 1. By-value field types: an item appears before another item iff
///    the second's field list references the first's type by simple
///    name.
/// 2. Hookable-call targets (arch-com#447 §8): if an item's body
///    contains a call `<recv>.<method>()` and exactly one *other*
///    eligible item declares a `hookable` named `<method>`, an edge
///    from that item to the caller is added. This catches call
///    dependencies that don't manifest as field-type references —
///    e.g. a transactor body resolving the receiver via an outer
///    scope or a future cross-scope binding. Rule (1) already
///    transitively covers the field-rooted call chains; rule (2) is
///    the safety net for the non-field case the existing graph
///    didn't model.
///
/// Source order breaks ties so fixtures with no cross-dependencies
/// still emit in the order users wrote them — diffs against the
/// pre-sort emitter stay minimal.
///
/// Cycle handling: a by-value cycle (`A { b : B }` ↔ `B { a : A }`)
/// is structurally invalid C++ (infinite size) and rejected by the
/// language-level check elsewhere; if one slips through, the
/// remaining nodes fall through in source order so codegen still
/// emits something and the C++ compiler surfaces the cycle as a
/// downstream error rather than us silently dropping items.
///
/// A transactor field may name another transactor defined later in the
/// source list, so both its C++ struct and hookable-method lambda require
/// dependency order rather than source order.
pub fn topo_sort_component_indices(file: &SourceFile) -> Vec<usize> {
    // First pass: enumerate eligible items and build the name → index
    // map. Eligibility = anything we emit a C++ `struct` and/or
    // hookable-method lambdas for.
    let mut name_to_idx: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut eligible: Vec<usize> = Vec::new();
    for (i, it) in file.items.iter().enumerate() {
        let name = match it {
            Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) | Item::Scoreboard(c) => {
                c.name.name.clone()
            }
            Item::Transactor(t) => t.name.name.clone(),
            _ => continue,
        };
        name_to_idx.insert(name, i);
        eligible.push(i);
    }
    // Build `method_owners`: hookable-method-name → set of eligible
    // item indices that declare a `hookable` (or `function` —
    // `is_hookable=false`) with that name. Consulted by the
    // call-target edge rule below. Includes `when_active` items for
    // transactors so an active-only hookable still counts.
    let mut method_owners: std::collections::HashMap<String, std::collections::HashSet<usize>> =
        std::collections::HashMap::new();
    for &i in &eligible {
        let items: &[ComponentItem] = match &file.items[i] {
            Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) | Item::Scoreboard(c) => &c.items,
            Item::Transactor(t) => &t.items,
            _ => continue,
        };
        let when_active_items: Option<&[ComponentItem]> = match &file.items[i] {
            Item::Transactor(t) => t.when_active.as_deref(),
            _ => None,
        };
        for ci in items.iter().chain(when_active_items.into_iter().flatten()) {
            if let ComponentItem::Hookable(h) = ci {
                method_owners
                    .entry(h.name.name.clone())
                    .or_default()
                    .insert(i);
            }
        }
    }
    // Second pass: collect outgoing edges. Two rules:
    //  (1) field-type rule — i depends on j when item i has a field
    //      whose simple type name matches item j's name. This is the
    //      structural cause: a by-value field needs the callee
    //      `struct` complete at the use site.
    //  (2) call-target rule (arch-com#447 §8) — i depends on j when
    //      i's body contains a call `<recv>.<method>()` and exactly
    //      one *other* eligible item j declares a `hookable` named
    //      `<method>`. This catches cross-item call edges that don't
    //      manifest as field-type references — without it, source
    //      order is the only thing keeping the C++ lambdas in
    //      dependency order. Ambiguous methods (multiple owners)
    //      are skipped, since which owner the call actually resolves
    //      to is decided at emit time by `resolve_component_method_call`
    //      against the current scope, and we don't have scope here.
    //      An ambiguous case will still resolve correctly if the
    //      receiver's type is reachable through fields (rule 1).
    let mut deps: std::collections::HashMap<usize, std::collections::HashSet<usize>> =
        std::collections::HashMap::new();
    for &i in &eligible {
        let items: &[ComponentItem] = match &file.items[i] {
            Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) | Item::Scoreboard(c) => &c.items,
            Item::Transactor(t) => &t.items,
            _ => continue,
        };
        // Transactors also have an active-only body; field references
        // there matter too (e.g. an `active` driver might hold a
        // helper transactor instance).
        let when_active_items: Option<&[ComponentItem]> = match &file.items[i] {
            Item::Transactor(t) => t.when_active.as_deref(),
            _ => None,
        };
        let mut entry = std::collections::HashSet::new();
        let record = |t: &TypeExpr, entry: &mut std::collections::HashSet<usize>| {
            if let Some(n) = type_simple_name(Some(t)) {
                if let Some(&j) = name_to_idx.get(n) {
                    if j != i {
                        entry.insert(j);
                    }
                }
            }
        };
        let mut called_methods: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for ci in items.iter().chain(when_active_items.into_iter().flatten()) {
            match ci {
                ComponentItem::Field(f) => {
                    record(&f.ty, &mut entry);
                }
                ComponentItem::Hookable(h) => {
                    collect_called_method_names(&h.body, &mut called_methods);
                }
                ComponentItem::OnHandler(h) => {
                    collect_called_method_names(&h.body, &mut called_methods);
                }
                ComponentItem::TargetTlmThread(t) => {
                    collect_called_method_names(&t.body, &mut called_methods);
                }
                ComponentItem::Lifecycle(_phase, body) => {
                    collect_called_method_names(body, &mut called_methods);
                }
                ComponentItem::Watchdog(w) => {
                    collect_called_method_names(&w.body, &mut called_methods);
                }
                ComponentItem::Connect(_) | ComponentItem::Apply(_) => {}
            }
        }
        // Apply rule (2): for each method called, if exactly one
        // *other* component owns a hookable by that name, add the
        // edge. Self-edges (same i) are skipped — same-component
        // calls are intra-item and don't reorder structs.
        //
        // TODO(arch-com#463 §8): the ambiguous-owner branch
        // (`externals.len() > 1`) currently drops the edge
        // silently. This is conservative-correct today because
        // `resolve_component_method_call` picks the receiver-
        // type-matching owner at emit time and the field rule
        // will have already added an edge to that owner — provided
        // the receiver's type is field-reachable. If the receiver
        // is NOT field-reachable (e.g. obtained via another
        // hookable call), the dependency degrades to source order.
        // Two design choices on the table:
        //   (a) widen rule 2 to resolve the receiver's type at
        //       graph-build time and add the resolved owner's
        //       edge, OR
        //   (b) emit a compile error on ambiguous unrooted calls.
        // Pinned by `transactor_topo_sort_skips_ambiguous_hookable_call_edges`
        // in `tests/codegen.rs`; pick a direction before any
        // codegen feature lands that legitimately wants the
        // edge added.
        for m in &called_methods {
            if let Some(owners) = method_owners.get(m) {
                let externals: Vec<usize> = owners.iter().copied().filter(|&j| j != i).collect();
                if externals.len() == 1 {
                    entry.insert(externals[0]);
                }
            }
        }
        deps.insert(i, entry);
    }
    // Kahn's algorithm with source-order tie-breaking. Indegrees are
    // taken from the reverse of `deps` (an edge j → i means j must
    // be emitted before i). We seed the work-list with indegree-0
    // nodes in source order; popping always takes the smallest-index
    // ready node so unrelated items keep their source position.
    let mut indegree: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for &i in &eligible {
        indegree.insert(i, 0);
    }
    for &i in &eligible {
        if let Some(d) = deps.get(&i) {
            // `deps[i]` is the set of items i depends on (i must be
            // emitted after each j ∈ deps[i]). Treating those as edges
            // j → i, i's indegree is exactly |deps[i]|.
            *indegree.entry(i).or_insert(0) += d.len();
        }
    }
    // Build the reverse adjacency: for each j, which i's depend on j?
    let mut reverse: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    for &i in &eligible {
        if let Some(d) = deps.get(&i) {
            for &j in d {
                reverse.entry(j).or_default().push(i);
            }
        }
    }
    let mut ready: Vec<usize> = eligible
        .iter()
        .copied()
        .filter(|i| indegree.get(i).copied().unwrap_or(0) == 0)
        .collect();
    ready.sort();
    let mut order: Vec<usize> = Vec::with_capacity(eligible.len());
    while let Some(pos) = ready
        .iter()
        .enumerate()
        .min_by_key(|(_, &idx)| idx)
        .map(|(p, _)| p)
    {
        let i = ready.remove(pos);
        order.push(i);
        if let Some(children) = reverse.get(&i) {
            for &c in children {
                if let Some(deg) = indegree.get_mut(&c) {
                    if *deg > 0 {
                        *deg -= 1;
                        if *deg == 0 {
                            ready.push(c);
                        }
                    }
                }
            }
        }
    }
    if order.len() < eligible.len() {
        // Cycle in the field-type graph. Recover by appending any
        // still-unprocessed items in source order so emission isn't
        // silently truncated; the resulting C++ will surface the
        // cycle as an "incomplete type" error at the offending field
        // — which is the structurally correct diagnostic for a
        // by-value cycle.
        for &i in &eligible {
            if !order.contains(&i) {
                order.push(i);
            }
        }
    }
    order
}

/// One registered cover point — what gets reported at end of test.
#[derive(Debug, Clone)]
struct CoverInfo {
    tag: String,
    label: String,
}

struct ConnectHookableSink {
    comp_ty: String,
    instance: String,
    method: String,
}

#[derive(Debug, Clone)]
struct PendingTlmFork {
    root: String,
    component: String,
    bus: String,
    method: String,
    sig_prefix: String,
    ret_var: Option<String>,
    ret_ty: Option<TypeExpr>,
    tag: Option<u64>,
}

fn stmt_contains_return(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Return(_) => true,
        StmtKind::For(s) => s.body.stmts.iter().any(stmt_contains_return),
        StmtKind::Repeat(s) => s.body.stmts.iter().any(stmt_contains_return),
        StmtKind::Loop(b) => b.stmts.iter().any(stmt_contains_return),
        StmtKind::While { body, .. } => body.stmts.iter().any(stmt_contains_return),
        StmtKind::If(i) => {
            i.then_block.stmts.iter().any(stmt_contains_return)
                || i.elsifs
                    .iter()
                    .any(|(_, b)| b.stmts.iter().any(stmt_contains_return))
                || i.else_block
                    .as_ref()
                    .is_some_and(|b| b.stmts.iter().any(stmt_contains_return))
        }
        StmtKind::Fork(f) => f
            .branches
            .iter()
            .any(|b| b.stmts.iter().any(stmt_contains_return)),
        StmtKind::Parallel(branches) | StmtKind::Schedule(branches) => branches
            .iter()
            .any(|b| b.stmts.iter().any(stmt_contains_return)),
        StmtKind::Select(arms) => arms
            .iter()
            .any(|a| a.action.stmts.iter().any(stmt_contains_return)),
        StmtKind::On(h) => h.body.stmts.iter().any(stmt_contains_return),
        StmtKind::After { body, .. } => body.stmts.iter().any(stmt_contains_return),
        _ => false,
    }
}

enum WaitCondition<'a> {
    Expr(&'a Expr),
    Idle { instance: String, cycles: &'a Expr },
}

impl<'a> WaitCondition<'a> {
    fn label(&self) -> String {
        match self {
            WaitCondition::Expr(e) => expr_source_str(e),
            WaitCondition::Idle { instance, cycles } => {
                format!("{instance}.idle({})", expr_source_str(cycles))
            }
        }
    }

    fn emit(&self, e: &mut Emitter) {
        match self {
            WaitCondition::Expr(expr) => e.emit_expr(expr),
            WaitCondition::Idle { instance, cycles } => {
                e.emit_idle_predicate(instance, "idle", cycles);
            }
        }
    }
}

/// Width-typed transaction field info used by the Z3 solver lowering.
#[derive(Debug, Clone)]
struct TxnFieldInfo {
    name: String,
    path: Vec<String>,
    width: u32,
    signed: bool,
    enum_variants: Option<usize>,
    enum_variant_labels: Option<Vec<String>>,
    list: Option<ListFieldInfo>,
    /// `!` prefix on a transaction field — pinned to the current value during
    /// solver-backed randomize and skipped during model assignment.
    non_random: bool,
    attrs: Vec<Attr>,
    when_guard: Option<Expr>,
}

#[derive(Debug, Clone)]
struct ListFieldInfo {
    declared_max_len: Option<usize>,
    elem_width: u32,
    elem_signed: bool,
}

fn txn_field_info_from_field_path(
    f: &Field,
    path: Vec<String>,
    when_guard: Option<Expr>,
    enums: &std::collections::HashMap<String, usize>,
    enum_domains: &std::collections::HashMap<String, Vec<String>>,
) -> TxnFieldInfo {
    let list = list_field_info(&f.ty);
    let (width, signed, enum_variants, enum_variant_labels) = match &f.ty {
        TypeExpr::Builtin { name, args, .. } => match name {
            BuiltinTy::UInt | BuiltinTy::Bits | BuiltinTy::UIntCap => {
                (type_arg_width(args).unwrap_or(64), false, None, None)
            }
            BuiltinTy::SInt | BuiltinTy::SIntCap => {
                (type_arg_width(args).unwrap_or(64), true, None, None)
            }
            BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => (1, false, None, None),
            BuiltinTy::Int => (32, true, None, None),
            _ => (64, false, None, None),
        },
        TypeExpr::Named { name, .. } => {
            let last = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
            if let Some(&n) = enums.get(last) {
                (
                    enum_width(n),
                    false,
                    Some(n),
                    enum_domains.get(last).cloned(),
                )
            } else {
                (0, false, None, None)
            }
        }
    };
    TxnFieldInfo {
        name: path.join("."),
        path,
        width,
        signed,
        enum_variants,
        enum_variant_labels,
        list,
        non_random: f.non_random,
        attrs: f.attrs.clone(),
        when_guard,
    }
}

fn flatten_txn_body_field_infos(
    items: &[TxnBodyItem],
    record_fields: &std::collections::HashMap<String, Vec<Field>>,
    enums: &std::collections::HashMap<String, usize>,
    enum_domains: &std::collections::HashMap<String, Vec<String>>,
) -> Vec<TxnFieldInfo> {
    let mut out = Vec::new();
    flatten_txn_body_field_infos_inner(
        items,
        Vec::new(),
        None,
        record_fields,
        enums,
        enum_domains,
        &mut out,
    );
    out
}

fn flatten_txn_body_field_infos_inner(
    items: &[TxnBodyItem],
    prefix: Vec<String>,
    when_guard: Option<Expr>,
    record_fields: &std::collections::HashMap<String, Vec<Field>>,
    enums: &std::collections::HashMap<String, usize>,
    enum_domains: &std::collections::HashMap<String, Vec<String>>,
    out: &mut Vec<TxnFieldInfo>,
) {
    for item in items {
        match item {
            TxnBodyItem::Field(f) => flatten_record_field_info_one(
                f,
                prefix.clone(),
                when_guard.clone(),
                record_fields,
                enums,
                enum_domains,
                out,
            ),
            TxnBodyItem::When(w) => {
                let next_guard = match &when_guard {
                    Some(g) => and_join(&[g.clone(), w.discriminant.clone()], w.span),
                    None => w.discriminant.clone(),
                };
                flatten_txn_body_field_infos_inner(
                    &w.items,
                    prefix.clone(),
                    Some(next_guard),
                    record_fields,
                    enums,
                    enum_domains,
                    out,
                );
            }
            TxnBodyItem::Keep(_) => {}
        }
    }
}

fn flatten_record_field_infos(
    fields: &[Field],
    record_fields: &std::collections::HashMap<String, Vec<Field>>,
    enums: &std::collections::HashMap<String, usize>,
    enum_domains: &std::collections::HashMap<String, Vec<String>>,
) -> Vec<TxnFieldInfo> {
    let mut out = Vec::new();
    flatten_record_field_infos_inner(
        fields,
        Vec::new(),
        record_fields,
        enums,
        enum_domains,
        &mut out,
    );
    out
}

fn flatten_record_field_infos_inner(
    fields: &[Field],
    prefix: Vec<String>,
    record_fields: &std::collections::HashMap<String, Vec<Field>>,
    enums: &std::collections::HashMap<String, usize>,
    enum_domains: &std::collections::HashMap<String, Vec<String>>,
    out: &mut Vec<TxnFieldInfo>,
) {
    for f in fields {
        flatten_record_field_info_one(
            f,
            prefix.clone(),
            None,
            record_fields,
            enums,
            enum_domains,
            out,
        );
    }
}

fn flatten_record_field_info_one(
    f: &Field,
    prefix: Vec<String>,
    when_guard: Option<Expr>,
    record_fields: &std::collections::HashMap<String, Vec<Field>>,
    enums: &std::collections::HashMap<String, usize>,
    enum_domains: &std::collections::HashMap<String, Vec<String>>,
    out: &mut Vec<TxnFieldInfo>,
) {
    let mut path = prefix;
    path.push(f.name.name.clone());
    if list_field_info(&f.ty).is_none() {
        if let Some(record) = record_type_name(&f.ty).and_then(|name| record_fields.get(name)) {
            if !enums.contains_key(record_type_name(&f.ty).unwrap_or_default()) {
                flatten_record_field_infos_inner(
                    record,
                    path,
                    record_fields,
                    enums,
                    enum_domains,
                    out,
                );
                return;
            }
        }
    }
    out.push(txn_field_info_from_field_path(
        f,
        path,
        when_guard,
        enums,
        enum_domains,
    ));
}

#[derive(Debug, Clone)]
struct AutoCoverageGoal {
    field: String,
    c_field: String,
    values: Vec<AutoCoverageValue>,
}

#[derive(Debug, Clone)]
struct AutoCoverageValue {
    label: String,
    c_expr: String,
    words: Vec<u32>,
}

impl AutoCoverageValue {
    fn unsigned(value: u64) -> Self {
        Self {
            label: value.to_string(),
            c_expr: format!("{value}ULL"),
            words: vec![value as u32, (value >> 32) as u32],
        }
    }

    fn signed(value: i64) -> Self {
        Self {
            label: value.to_string(),
            c_expr: if value == i64::MIN {
                "INT64_MIN".to_string()
            } else if value < 0 {
                format!("{value}LL")
            } else {
                format!("{value}LL")
            },
            words: vec![value as u32, ((value as u64) >> 32) as u32],
        }
    }

    fn from_words(label: String, words: Vec<u32>) -> Self {
        let c_expr = if words.len() <= 2 {
            let lo = words.first().copied().unwrap_or(0) as u64;
            let hi = words.get(1).copied().unwrap_or(0) as u64;
            format!("{}ULL", lo | (hi << 32))
        } else if words.len() <= 4 {
            let mut terms = Vec::new();
            for (idx, word) in words.iter().enumerate() {
                if *word == 0 {
                    continue;
                }
                terms.push(format!("((_harc_u128)0x{word:08x}ULL << {})", idx * 32));
            }
            if terms.is_empty() {
                "(_harc_u128)0".to_string()
            } else {
                format!("({})", terms.join(" | "))
            }
        } else {
            format!(
                "harc_rt::HarcWide<{}>({{{}}})",
                words.len(),
                words
                    .iter()
                    .map(|w| format!("0x{w:08x}u"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        Self {
            label,
            c_expr,
            words,
        }
    }
}

const COVERGROUP_AUTO_CROSS_BIN_CAP: usize = 64;
const COVERGROUP_CROSS_MISSING_DETAIL_LIMIT: usize = 16;
const AUTO_COVERAGE_WALKING_BIT_CAP: usize = 16;

struct DeclaredCoverCross<'a> {
    storage: String,
    label: String,
    points: Vec<&'a CoverPoint>,
    total_bins: usize,
}

fn covergroup_binned_points(g: &CovergroupDecl) -> Vec<&CoverPoint> {
    g.items
        .iter()
        .filter_map(|it| match it {
            CoverItem::Point(p) if !p.bins.is_empty() => Some(p),
            _ => None,
        })
        .collect()
}

fn covergroup_auto_crosses(g: &CovergroupDecl) -> Vec<(&CoverPoint, &CoverPoint)> {
    let points = covergroup_binned_points(g);
    let declared_pairs: std::collections::BTreeSet<(String, String)> = g
        .items
        .iter()
        .filter_map(|it| match it {
            CoverItem::Cross(c) if c.points.len() == 2 => {
                let mut names = [c.points[0].name.clone(), c.points[1].name.clone()];
                names.sort();
                Some((names[0].clone(), names[1].clone()))
            }
            _ => None,
        })
        .collect();
    let mut crosses = Vec::new();
    for i in 0..points.len() {
        for j in (i + 1)..points.len() {
            let mut names = [points[i].name.name.clone(), points[j].name.name.clone()];
            names.sort();
            if declared_pairs.contains(&(names[0].clone(), names[1].clone())) {
                continue;
            }
            let bins = points[i].bins.len() * points[j].bins.len();
            if bins <= COVERGROUP_AUTO_CROSS_BIN_CAP {
                crosses.push((points[i], points[j]));
            }
        }
    }
    crosses
}

fn covergroup_declared_crosses(g: &CovergroupDecl) -> Result<Vec<DeclaredCoverCross<'_>>, String> {
    let mut crosses = Vec::new();
    for (cross_idx, item) in g.items.iter().enumerate() {
        let CoverItem::Cross(cross) = item else {
            continue;
        };
        if cross.points.len() < 2 {
            return Err(format!(
                "covergroup `{}` cross must name at least two coverpoints",
                g.name.name
            ));
        }
        let mut points = Vec::new();
        for ident in &cross.points {
            let point = g.items.iter().find_map(|item| match item {
                CoverItem::Point(p) if p.name.name == ident.name => Some(p),
                _ => None,
            });
            let Some(point) = point else {
                return Err(format!(
                    "covergroup `{}` cross references unknown coverpoint `{}`",
                    g.name.name, ident.name
                ));
            };
            if point.bins.is_empty() {
                return Err(format!(
                    "covergroup `{}` cross references coverpoint `{}` with no bins",
                    g.name.name, point.name.name
                ));
            }
            points.push(point);
        }
        let mut total_bins = 1usize;
        for p in &points {
            total_bins = total_bins.checked_mul(p.bins.len()).ok_or_else(|| {
                format!(
                    "covergroup `{}` cross `{}` has too many bins",
                    g.name.name,
                    cross
                        .points
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect::<Vec<_>>()
                        .join(" x ")
                )
            })?;
        }
        let label = points
            .iter()
            .map(|p| p.name.name.as_str())
            .collect::<Vec<_>>()
            .join(" x ");
        let storage = format!(
            "_cross_{}_{}",
            cross_idx,
            points
                .iter()
                .map(|p| p.name.name.as_str())
                .collect::<Vec<_>>()
                .join("__")
        );
        crosses.push(DeclaredCoverCross {
            storage,
            label,
            points,
            total_bins,
        });
    }
    Ok(crosses)
}

fn declared_cross_index_expr(cross: &DeclaredCoverCross<'_>) -> String {
    let mut expr = "_i0".to_string();
    for (idx, point) in cross.points.iter().enumerate().skip(1) {
        expr = format!("({expr} * {} + _i{idx})", point.bins.len());
    }
    expr
}

fn declared_cross_bin_labels(cross: &DeclaredCoverCross<'_>) -> Vec<(usize, String)> {
    fn walk(
        cross: &DeclaredCoverCross<'_>,
        depth: usize,
        index: usize,
        labels: &mut Vec<String>,
        out: &mut Vec<(usize, String)>,
    ) {
        if depth == cross.points.len() {
            out.push((index, labels.join(" x ")));
            return;
        }
        let point = cross.points[depth];
        for (bin_idx, bin) in point.bins.iter().enumerate() {
            labels.push(format!("{}.{}", point.name.name, bin.name.name));
            walk(
                cross,
                depth + 1,
                index * point.bins.len() + bin_idx,
                labels,
                out,
            );
            labels.pop();
        }
    }

    let mut out = Vec::new();
    walk(cross, 0, 0, &mut Vec::new(), &mut out);
    out
}

fn field_attr_range(f: &TxnFieldInfo) -> Option<(&Expr, &Expr)> {
    f.attrs.iter().find_map(|a| {
        if a.name.name != "range" || a.args.len() < 2 {
            return None;
        }
        match (&a.args[0], &a.args[1]) {
            (AttrArg::Expr(lo), AttrArg::Expr(hi)) => Some((lo, hi)),
            _ => None,
        }
    })
}

fn field_attr_dist_entries(f: &TxnFieldInfo) -> Option<&Vec<DistEntry>> {
    f.attrs.iter().find_map(|a| {
        if a.name.name != "dist" {
            return None;
        }
        a.args.iter().find_map(|arg| match arg {
            AttrArg::Dist(entries) => Some(entries),
            _ => None,
        })
    })
}

fn field_attr_unique(f: &TxnFieldInfo) -> bool {
    f.attrs.iter().any(|a| a.name.name == "unique")
}

fn field_attr_unique_scope(f: &TxnFieldInfo) -> Option<&str> {
    f.attrs
        .iter()
        .find(|a| a.name.name == "unique")
        .and_then(|a| {
            a.args.iter().find_map(|arg| match arg {
                AttrArg::WithinScope(scope) => Some(scope.name.as_str()),
                _ => None,
            })
        })
}

fn push_auto_coverage_value_unique(values: &mut Vec<AutoCoverageValue>, value: AutoCoverageValue) {
    if !values.iter().any(|v| v.words == value.words) {
        values.push(value);
    }
}

fn walking_auto_coverage_bit_positions(width: u32) -> Vec<u32> {
    if width == 0 {
        return Vec::new();
    }
    let width_usize = width as usize;
    let count = width_usize.min(AUTO_COVERAGE_WALKING_BIT_CAP);
    if count == width_usize {
        return (0..width).collect();
    }

    let mut positions = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let max_pos = width_usize - 1;
    let max_idx = count - 1;
    for i in 0..count {
        let pos = (i * max_pos + (max_idx / 2)) / max_idx;
        if seen.insert(pos) {
            positions.push(pos as u32);
        }
    }
    positions
}

fn unsigned_max_words(width: u32) -> Vec<u32> {
    let word_count = (width as usize).div_ceil(32).max(1);
    let mut words = vec![0xffff_ffffu32; word_count];
    let rem = width % 32;
    if rem != 0 {
        if let Some(last) = words.last_mut() {
            *last = (1u32 << rem) - 1;
        }
    }
    words
}

fn solver_unsigned_mask_expr(width: u32) -> String {
    let words = unsigned_max_words(width);
    if words.len() <= 2 {
        let lo = words.first().copied().unwrap_or(0) as u64;
        let hi = words.get(1).copied().unwrap_or(0) as u64;
        format!("(uint64_t)0x{:016x}ULL", lo | (hi << 32))
    } else if words.len() <= 4 {
        let mut terms = Vec::new();
        for (idx, word) in words.iter().enumerate() {
            if *word != 0 {
                terms.push(format!("((_harc_u128)0x{word:08x}ULL << {})", idx * 32));
            }
        }
        if terms.is_empty() {
            "(_harc_u128)0".to_string()
        } else {
            format!("({})", terms.join(" | "))
        }
    } else {
        format!(
            "harc_rt::HarcWide<{}>({{{}}})",
            words.len(),
            words
                .iter()
                .map(|w| format!("0x{w:08x}u"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

fn signed_min_words(width: u32) -> Vec<u32> {
    let mut words = vec![0u32; (width as usize).div_ceil(32).max(1)];
    let sign_bit = width.saturating_sub(1);
    words[(sign_bit / 32) as usize] = 1u32 << (sign_bit % 32);
    words
}

fn signed_max_words(width: u32) -> Vec<u32> {
    let mut words = unsigned_max_words(width);
    let sign_bit = width.saturating_sub(1);
    words[(sign_bit / 32) as usize] &= !(1u32 << (sign_bit % 32));
    words
}

fn decimal_label_for_auto_words(width: u32, words: &[u32]) -> String {
    if width <= 64 {
        let lo = words.first().copied().unwrap_or(0) as u64;
        let hi = words.get(1).copied().unwrap_or(0) as u64;
        return (lo | (hi << 32)).to_string();
    }
    if words.iter().all(|w| *w == 0) {
        return "0".to_string();
    }
    if words.iter().enumerate().all(|(idx, w)| {
        let max = unsigned_max_words(width);
        max.get(idx).copied().unwrap_or(0) == *w
    }) {
        return format!("2^{}-1", width);
    }
    if let Some((idx, word)) = words
        .iter()
        .enumerate()
        .find(|(_, word)| **word != 0 && word.count_ones() == 1)
    {
        if words
            .iter()
            .enumerate()
            .all(|(other_idx, other)| other_idx == idx || *other == 0)
        {
            let bit = idx as u32 * 32 + word.trailing_zeros();
            return format!("2^{bit}");
        }
    }
    if words.iter().enumerate().all(|(idx, word)| {
        let mut max = unsigned_max_words(width);
        let diff = max.get_mut(idx).map(|m| {
            let d = *m ^ *word;
            d != 0 && d.count_ones() == 1
        });
        let this_is_diff = diff.unwrap_or(false);
        this_is_diff
            || max
                .get(idx)
                .copied()
                .map(|m| m == *word)
                .unwrap_or(*word == 0)
    }) {
        let max = unsigned_max_words(width);
        for (idx, (a, b)) in max.iter().zip(words.iter()).enumerate() {
            let diff = *a ^ *b;
            if diff != 0 && diff.count_ones() == 1 {
                let bit = idx as u32 * 32 + diff.trailing_zeros();
                return format!("2^{}-1-2^{bit}", width);
            }
        }
    }
    "wide".to_string()
}

fn auto_coverage_unsigned_word(value_words: Vec<u32>, width: u32) -> AutoCoverageValue {
    let label = decimal_label_for_auto_words(width, &value_words);
    AutoCoverageValue::from_words(label, value_words)
}

fn auto_coverage_signed_word(value_words: Vec<u32>, width: u32, is_min: bool) -> AutoCoverageValue {
    let label = if is_min {
        format!("-2^{}", width.saturating_sub(1))
    } else {
        format!("2^{}-1", width.saturating_sub(1))
    };
    AutoCoverageValue::from_words(label, value_words)
}

fn signed_domain_bound_exprs(width: u32) -> (String, String) {
    if width == 64 {
        ("INT64_MIN".to_string(), "INT64_MAX".to_string())
    } else if width < 64 {
        (
            format!("(int64_t)(-(1LL << {}))", width.saturating_sub(1)),
            format!("(int64_t)((1LL << {}) - 1)", width.saturating_sub(1)),
        )
    } else {
        (
            AutoCoverageValue::from_words(String::new(), signed_min_words(width)).c_expr,
            AutoCoverageValue::from_words(String::new(), signed_max_words(width)).c_expr,
        )
    }
}

fn solver_bv_value_call(value: &str, signed: bool, value_width: u32, solver_width: u32) -> String {
    if signed {
        format!("harc_z3_bv_signed_value(_ctx, {value}, {value_width}, {solver_width})")
    } else {
        format!("harc_z3_bv_value(_ctx, {value}, {solver_width})")
    }
}

fn natural_auto_coverage_endpoints(f: &TxnFieldInfo) -> Vec<AutoCoverageValue> {
    if f.width == 0 {
        return Vec::new();
    }

    if f.signed {
        if f.width > 64 {
            return vec![
                auto_coverage_signed_word(signed_min_words(f.width), f.width, true),
                auto_coverage_signed_word(signed_max_words(f.width), f.width, false),
            ];
        }
        let shift = f.width.saturating_sub(1);
        let lo = if shift >= 63 {
            i64::MIN
        } else {
            -(1i64 << shift)
        };
        let hi = if shift >= 63 {
            i64::MAX
        } else {
            (1i64 << shift) - 1
        };
        let mut values = vec![AutoCoverageValue::signed(lo)];
        if hi != lo {
            values.push(AutoCoverageValue::signed(hi));
        }
        values
    } else {
        let hi_words = unsigned_max_words(f.width);
        let mut values = Vec::new();
        push_auto_coverage_value_unique(
            &mut values,
            auto_coverage_unsigned_word(vec![0; hi_words.len()], f.width),
        );
        push_auto_coverage_value_unique(
            &mut values,
            auto_coverage_unsigned_word(hi_words.clone(), f.width),
        );
        for bit in walking_auto_coverage_bit_positions(f.width) {
            let mut one = vec![0u32; hi_words.len()];
            one[(bit / 32) as usize] = 1u32 << (bit % 32);
            push_auto_coverage_value_unique(&mut values, auto_coverage_unsigned_word(one, f.width));
            let mut inv = hi_words.clone();
            inv[(bit / 32) as usize] ^= 1u32 << (bit % 32);
            push_auto_coverage_value_unique(&mut values, auto_coverage_unsigned_word(inv, f.width));
        }
        values
    }
}

/// A mathematical literal interval whose values agree with the emitted
/// bit-vector comparisons. Do not reinterpret unsigned literals as negative
/// signed bounds, or optimize expressions whose constant value is unknown.
fn literal_range_preference(f: &TxnFieldInfo, salt: usize, solver_width: u32) -> Option<String> {
    fn bound(e: &Expr, solver_width: u32) -> Option<i128> {
        match &*e.kind {
            ExprKind::Int(s) => {
                let value = parse_int_str(s)?;
                // Wide constraint literals currently pass through an int64_t
                // carrier and sign-extend. Do not optimize a different interval.
                if solver_width > 64 && value > i64::MAX as u64 {
                    return None;
                }
                Some(i128::from(value))
            }
            ExprKind::Paren(inner) => bound(inner, solver_width),
            ExprKind::Unary {
                op: UnaryOp::Neg,
                expr,
            } => bound(expr, solver_width)?.checked_neg(),
            _ => None,
        }
    }
    fn signed_literal(v: i128) -> String {
        if v == i64::MIN as i128 {
            "(-9223372036854775807LL - 1)".to_string()
        } else {
            format!("{v}LL")
        }
    }
    let (lo, hi) = field_attr_range(f)?;
    let (lo, hi) = (bound(lo, solver_width)?, bound(hi, solver_width)?);
    if f.width == 0 {
        return None;
    }
    if f.signed {
        if f.width > 64 || lo < i64::MIN as i128 || hi > i64::MAX as i128 {
            return None;
        }
        let half = 1i128 << (f.width - 1);
        let (lo, hi) = (lo.max(-half), hi.min(half - 1));
        if lo > hi {
            return None;
        }
        Some(format!(
            "harc_rt::random::harc_prefer_range_i64(_harc_rt_seed, {salt}, {}, {})",
            signed_literal(lo),
            signed_literal(hi)
        ))
    } else {
        if lo < 0 || hi > u64::MAX as i128 {
            return None;
        }
        let max = if f.width >= 64 {
            u64::MAX as i128
        } else {
            (1i128 << f.width) - 1
        };
        let hi = hi.min(max);
        if lo > hi {
            return None;
        }
        Some(format!(
            "harc_rt::random::harc_prefer_range_u64(_harc_rt_seed, {salt}, {lo}ULL, {hi}ULL)"
        ))
    }
}

fn auto_coverage_values(f: &TxnFieldInfo) -> Vec<AutoCoverageValue> {
    let mut values = Vec::new();
    if let Some(n) = f.enum_variants {
        let labels = f.enum_variant_labels.as_deref();
        for i in 0..n {
            if let Some(label) = labels.and_then(|labels| labels.get(i)) {
                values.push(AutoCoverageValue {
                    label: label.clone(),
                    c_expr: format!("{i}ULL"),
                    words: vec![i as u32, 0],
                });
            } else {
                values.push(AutoCoverageValue::unsigned(i as u64));
            }
        }
    } else if f.width == 1 && !f.signed {
        values.extend([0, 1].map(AutoCoverageValue::unsigned));
    } else if let Some((lo, hi)) = field_attr_range(f) {
        if f.signed {
            if let (Some(lo), Some(hi)) = (fold_signed_int_literal(lo), fold_signed_int_literal(hi))
            {
                values.push(AutoCoverageValue::signed(lo));
                if hi != lo {
                    values.push(AutoCoverageValue::signed(hi));
                }
            }
        } else if let (Some(lo), Some(hi)) = (fold_int_literal(lo), fold_int_literal(hi)) {
            values.push(AutoCoverageValue::unsigned(lo));
            if hi != lo {
                values.push(AutoCoverageValue::unsigned(hi));
            }
        }
    } else {
        values.extend(natural_auto_coverage_endpoints(f));
    }
    values
}

/// Bits needed to hold `v` — a literal is self-sized, and `0` is one bit.
fn value_bit_width(v: u64) -> u32 {
    if v == 0 {
        1
    } else {
        64 - v.leading_zeros()
    }
}

/// Self-width of a literal operand in a constraint, for the §2.4 wrap
/// mask. Everything unsized is sized by value through the statement
/// path's own `parse_int_literal`, so the same literal sizes identically
/// in both positions.
///
/// An ARCH sized literal states its width outright, and that width is
/// what §2.4 masks at — the same declared-width rule a `const` follows,
/// so the two agree about a token like `8'd300` that can be spelled
/// either way. The digits are not consulted, and since harc#565 they no
/// longer can EXCEED the declared width: the parser rejects a literal
/// whose value does not fit. The two are not equal — `8'h0F` declares 8
/// and needs 4, and the mask follows the 8 — but the declared width is
/// now guaranteed to be wide enough for the value, which is the property
/// that was missing. Before that fix this function had to choose,
/// and either choice was wrong somewhere — masking at the declared width
/// turned `keep (len +% 4'hFF) == 4'hFF` on a `uint<4>` field from
/// solvable into unsatisfiable, and masking at the value's width did the
/// same to `keep len +% 4'hFF == 15`.
///
/// Not parsing the digits also keeps a literal whose value overflows
/// `u64` (`128'hFFFF...`) resolvable, where its declared width is all
/// the mask ever needed.
fn literal_operand_bit_width(s: &str) -> Option<u32> {
    let Some(idx) = s.find('\'') else {
        return crate::ir::lower::parse_int_literal(s).map(value_bit_width);
    };
    // A zero declared width no longer parses (harc#565), so the old
    // `.max(1)` clamp is gone. It was load-bearing, not cosmetic: a zero
    // reaching `solver_unsigned_mask_expr` yields a bogus 32-bits-of-ones
    // mask. Assert the coupling rather than leaving a dead clamp that
    // hides it.
    let declared: u32 = s[..idx].replace('_', "").parse().ok()?;
    debug_assert!(
        declared > 0,
        "a zero-width sized literal must be rejected by the parser (harc#565)"
    );
    Some(declared)
}

/// Raw declared width of a width-carrying builtin type, for a `const`'s
/// wrap-operand width. Deliberately NOT `cast_relabel_width`: that helper
/// caps at 128 and returns `None` both for "not a width type" and for
/// "out of range", and a caller that falls back on `None` cannot tell
/// those apart — `const K : uint<200> = 10` would silently take the
/// initializer's 4-bit value width instead of being rejected as
/// unrepresentable. A non-width type still returns `None`, which is a
/// real "no declared width" and correctly falls back.
fn declared_type_bit_width(t: &TypeExpr) -> Option<u32> {
    let TypeExpr::Builtin { name, args, .. } = t else {
        return None;
    };
    match name {
        BuiltinTy::UInt
        | BuiltinTy::UIntCap
        | BuiltinTy::SInt
        | BuiltinTy::SIntCap
        | BuiltinTy::Bits => Some(type_arg_width(args).unwrap_or(64)),
        BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => Some(1),
        BuiltinTy::Int => Some(32),
        _ => None,
    }
}

fn txn_field_solver_width(f: &TxnFieldInfo) -> u32 {
    f.list
        .as_ref()
        .map(|list| list.elem_width)
        .unwrap_or(f.width)
}

fn txn_field_solver_c_type(f: &TxnFieldInfo) -> String {
    if f.signed {
        cpp_sint_for_width(Some(f.width))
    } else {
        cpp_uint_for_width(Some(f.width))
    }
}

fn solver_scalar_c_type(width: u32, signed: bool) -> String {
    if signed {
        cpp_sint_for_width(Some(width))
    } else {
        cpp_uint_for_width(Some(width))
    }
}

fn auto_value_array_type(
    goal: &AutoCoverageGoal,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
) -> String {
    field_info
        .get(&goal.field)
        .map(txn_field_solver_c_type)
        .unwrap_or_else(|| "uint64_t".to_string())
}

fn auto_value_initializer(values: &[AutoCoverageValue]) -> String {
    values
        .iter()
        .map(|v| v.c_expr.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

fn emit_random_pref_expr(f: &TxnFieldInfo, salt: usize) -> String {
    if f.width <= 128 {
        format!(
            "harc_rt::random::harc_prefer_u128(_harc_rt_seed, {}, {})",
            salt, f.width
        )
    } else {
        let words = f.width.div_ceil(32);
        format!(
            "harc_rt::random::harc_prefer_wide<{}>(_harc_rt_seed, {}, {})",
            words, salt, f.width
        )
    }
}

fn emit_random_unsigned_expr(width: u32) -> String {
    if width <= 64 {
        format!("harc_rt::random::harc_rng_uint(harc_rng_next, {width})")
    } else if width <= 128 {
        format!("harc_rt::random::harc_rng_u128(harc_rng_next, {width})")
    } else {
        let words = width.div_ceil(32);
        format!(
            "harc_rt::random::harc_rng_wide<{}>(harc_rng_next, {width})",
            words
        )
    }
}

fn emit_random_signed_expr(width: u32) -> String {
    if width < 63 {
        format!(
            "harc_rt::random::harc_rng_range(harc_rng_next, -(1LL << {}), (1LL << {}) - 1)",
            width.saturating_sub(1),
            width.saturating_sub(1)
        )
    } else if width == 63 {
        // The full sint<63> domain has a 2^63 span, which overflows the
        // signed span arithmetic in harc_rng_range. Draw and sign-extend.
        "static_cast<int64_t>(harc_rt::harc_sext_u128(static_cast<_harc_u128>(harc_rt::random::harc_rng_uint(harc_rng_next, 63)), 63, 64))".to_string()
    } else {
        emit_random_unsigned_expr(width)
    }
}

fn c_solver_int_literal(s: &str) -> String {
    let value = c_value_literal(s);
    if value.starts_with("harc_rt::HarcWide<") || value.contains("_harc_u128") {
        value
    } else {
        format!("(int64_t)({})", c_int_literal(s))
    }
}

fn randomize_target_field_name(
    target: &Expr,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
) -> Option<String> {
    let field = expr_field_path(target, target_root)?;
    field_info.contains_key(&field).then_some(field)
}

fn randomize_target_ident(target: &Expr) -> Option<&str> {
    match &*target.kind {
        ExprKind::Ident(id) => Some(id.name.as_str()),
        ExprKind::Paren(inner) => randomize_target_ident(inner),
        _ => None,
    }
}

/// Element signedness of `sum(<list>[lo..hi])`, or `false` for any other
/// call.
///
/// Recognition mirrors `try_emit_constraint_list_call`'s `sum` arm — same
/// callee name, same single `CallArg::Expr`, same `Index` target resolved
/// through `list_field_name_from_expr`. The two must agree about what
/// counts as a `sum` over a list: if this one is narrower the comparison
/// goes back to unsigned (harc#598), and if it is wider it claims
/// signedness for something the emitter lowers some other way. Deliberately
/// does NOT require the index to be a `RangeLit` — the emitter diagnoses a
/// non-range index itself, and a signedness answer for an expression that
/// is about to be rejected is irrelevant either way.
fn sum_call_elem_signed(
    e: &Expr,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
) -> bool {
    let ExprKind::Call { callee, args } = &*e.kind else {
        return false;
    };
    let ExprKind::Ident(name) = &*callee.kind else {
        return false;
    };
    if name.name != "sum" || args.len() != 1 {
        return false;
    }
    let CallArg::Expr(arg) = &args[0] else {
        return false;
    };
    // BOTH argument shapes the emitter accepts. It takes `sum(vals[lo..hi])`
    // through an `Index` and `sum(vals)` through a bare fall-through, and
    // handling only the first left `sum(vals)` over a signed list still
    // emitting `z3::ult` — harc#598 unfixed in its sibling syntax, which is
    // exactly the drift the doc above warns about.
    let list_expr = match &*arg.kind {
        ExprKind::Index { target, .. } => target,
        _ => arg,
    };
    list_field_name_from_expr(list_expr, field_info, target_root)
        .and_then(|field| field_info.get(&field))
        .and_then(|f| f.list.as_ref())
        .map(|l| l.elem_signed)
        .unwrap_or(false)
}

fn list_field_name_from_expr(
    e: &Expr,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
) -> Option<String> {
    let field = expr_field_path(e, target_root)?;
    field_info
        .get(&field)
        .and_then(|info| info.list.as_ref().map(|_| field))
}

fn expr_field_path(e: &Expr, target_root: Option<&str>) -> Option<String> {
    let mut parts = Vec::new();
    collect_expr_field_path(e, &mut parts)?;
    if parts.is_empty() {
        return None;
    }
    if let Some(root) = target_root {
        if parts.first().is_some_and(|part| part == root) {
            parts.remove(0);
        }
    }
    Some(parts.join("."))
}

fn expr_field_root(e: &Expr) -> Option<String> {
    let mut parts = Vec::new();
    collect_expr_field_path(e, &mut parts)?;
    parts.first().cloned()
}

fn collect_expr_field_path(e: &Expr, out: &mut Vec<String>) -> Option<()> {
    match &*e.kind {
        ExprKind::Ident(id) => out.push(id.name.clone()),
        ExprKind::ImplicitSelf => {}
        ExprKind::Field { target, name } => {
            collect_expr_field_path(target, out)?;
            out.push(name.name.clone());
        }
        ExprKind::Paren(inner) => collect_expr_field_path(inner, out)?,
        _ => return None,
    }
    Some(())
}

fn list_len_call_field_name(
    e: &Expr,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
) -> Option<String> {
    let ExprKind::Call { callee, args } = &*e.kind else {
        return None;
    };
    if !args.is_empty() {
        return None;
    }
    let ExprKind::Field { target, name } = &*callee.kind else {
        return None;
    };
    if name.name != "len" {
        return None;
    }
    list_field_name_from_expr(target, field_info, target_root)
}

fn const_usize_from_constraint_expr(e: &Expr) -> Option<usize> {
    const_usize_expr(e)
}

fn list_len_upper_bound_from_expr(
    e: &Expr,
    field: &str,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
) -> Option<usize> {
    match &*e.kind {
        ExprKind::Paren(inner) => {
            list_len_upper_bound_from_expr(inner, field, field_info, target_root)
        }
        ExprKind::Membership { expr, set } => {
            if list_len_call_field_name(expr, field_info, target_root).as_deref() != Some(field) {
                return None;
            }
            match &*set.kind {
                ExprKind::RangeLit { hi: Some(hi), .. } => const_usize_from_constraint_expr(hi),
                ExprKind::SetLit(items) => items
                    .iter()
                    .filter_map(|item| match &*item.kind {
                        ExprKind::RangeLit { hi: Some(hi), .. } => {
                            const_usize_from_constraint_expr(hi)
                        }
                        _ => const_usize_from_constraint_expr(item),
                    })
                    .max(),
                _ => const_usize_from_constraint_expr(set),
            }
        }
        ExprKind::Binary { op, lhs, rhs } => {
            if matches!(op, BinaryOp::OrOr) && guarded_constraint_guard(e).is_some() {
                return list_len_upper_bound_from_expr(rhs, field, field_info, target_root);
            }
            let lhs_is_len =
                list_len_call_field_name(lhs, field_info, target_root).as_deref() == Some(field);
            let rhs_is_len =
                list_len_call_field_name(rhs, field_info, target_root).as_deref() == Some(field);
            match op {
                BinaryOp::Le if lhs_is_len => const_usize_from_constraint_expr(rhs),
                BinaryOp::Lt if lhs_is_len => {
                    const_usize_from_constraint_expr(rhs).and_then(|v| v.checked_sub(1))
                }
                BinaryOp::Eq if lhs_is_len => const_usize_from_constraint_expr(rhs),
                BinaryOp::Ge if rhs_is_len => const_usize_from_constraint_expr(lhs),
                BinaryOp::Gt if rhs_is_len => {
                    const_usize_from_constraint_expr(lhs).and_then(|v| v.checked_sub(1))
                }
                BinaryOp::Eq if rhs_is_len => const_usize_from_constraint_expr(lhs),
                BinaryOp::AndAnd | BinaryOp::AndKw => {
                    let a = list_len_upper_bound_from_expr(lhs, field, field_info, target_root);
                    let b = list_len_upper_bound_from_expr(rhs, field, field_info, target_root);
                    match (a, b) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (Some(a), None) => Some(a),
                        (None, Some(b)) => Some(b),
                        (None, None) => None,
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn infer_list_unroll_bounds(
    fields: &[TxnFieldInfo],
    hard_constraints: &[&Expr],
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
) -> std::collections::HashMap<String, usize> {
    let mut out = std::collections::HashMap::new();
    for f in fields {
        let Some(list) = &f.list else {
            continue;
        };
        let mut bound = list.declared_max_len;
        for c in hard_constraints {
            if let Some(next) = list_len_upper_bound_from_expr(c, &f.name, field_info, target_root)
            {
                bound = Some(bound.map_or(next, |old| old.min(next)));
            }
        }
        if let Some(bound) = bound {
            out.insert(f.name.clone(), bound);
        }
    }
    out
}

fn collect_randomize_target_field_refs(
    e: &Expr,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
    out: &mut std::collections::HashSet<String>,
) {
    match &*e.kind {
        ExprKind::Ident(id) => {
            if field_info.contains_key(&id.name) {
                out.insert(id.name.clone());
            }
        }
        ExprKind::Field { target, .. } => {
            if let Some(field) = expr_field_path(e, target_root) {
                if field_info.contains_key(&field) {
                    out.insert(field);
                    return;
                }
            }
            collect_randomize_target_field_refs(target, field_info, target_root, out);
        }
        ExprKind::Index { target, index } => {
            collect_randomize_target_field_refs(target, field_info, target_root, out);
            collect_randomize_target_field_refs(index, field_info, target_root, out);
        }
        ExprKind::BitSlice { target, hi, lo } => {
            collect_randomize_target_field_refs(target, field_info, target_root, out);
            collect_randomize_target_field_refs(hi, field_info, target_root, out);
            collect_randomize_target_field_refs(lo, field_info, target_root, out);
        }
        ExprKind::Call { callee, args } => {
            collect_randomize_target_field_refs(callee, field_info, target_root, out);
            for arg in args {
                match arg {
                    CallArg::Expr(e) | CallArg::Named { value: e, .. } => {
                        collect_randomize_target_field_refs(e, field_info, target_root, out)
                    }
                }
            }
        }
        ExprKind::ForEachConstraint { iter, body, .. } => {
            collect_randomize_target_field_refs(iter, field_info, target_root, out);
            for clause in body {
                collect_randomize_target_field_refs(clause, field_info, target_root, out);
            }
        }
        ExprKind::Cast { expr, .. }
        | ExprKind::Unary { expr, .. }
        | ExprKind::Paren(expr)
        | ExprKind::ForkCall { call: expr }
        | ExprKind::HashHash { expr, .. }
        | ExprKind::SeqRepeat { expr, .. } => {
            collect_randomize_target_field_refs(expr, field_info, target_root, out);
        }
        ExprKind::Membership { expr, set } => {
            collect_randomize_target_field_refs(expr, field_info, target_root, out);
            collect_randomize_target_field_refs(set, field_info, target_root, out);
        }
        ExprKind::Send { target, value }
        | ExprKind::Binary {
            lhs: target,
            rhs: value,
            ..
        } => {
            collect_randomize_target_field_refs(target, field_info, target_root, out);
            collect_randomize_target_field_refs(value, field_info, target_root, out);
        }
        ExprKind::Ternary {
            cond,
            then_branch,
            else_branch,
        } => {
            collect_randomize_target_field_refs(cond, field_info, target_root, out);
            collect_randomize_target_field_refs(then_branch, field_info, target_root, out);
            collect_randomize_target_field_refs(else_branch, field_info, target_root, out);
        }
        ExprKind::RangeLit { lo, hi } => {
            if let Some(lo) = lo {
                collect_randomize_target_field_refs(lo, field_info, target_root, out);
            }
            if let Some(hi) = hi {
                collect_randomize_target_field_refs(hi, field_info, target_root, out);
            }
        }
        ExprKind::SetLit(items) => {
            for item in items {
                collect_randomize_target_field_refs(item, field_info, target_root, out);
            }
        }
        ExprKind::DistLit(entries) => {
            for entry in entries {
                collect_randomize_target_field_refs(&entry.value, field_info, target_root, out);
                collect_randomize_target_field_refs(&entry.weight, field_info, target_root, out);
            }
        }
        ExprKind::SystemCall { args, .. }
        | ExprKind::Randomize {
            with_body: args, ..
        } => {
            for arg in args {
                collect_randomize_target_field_refs(arg, field_info, target_root, out);
            }
        }
        ExprKind::SoftConstraint(sc) => {
            collect_randomize_target_field_refs(&sc.expr, field_info, target_root, out);
            if let Some(weight) = &sc.weight {
                collect_randomize_target_field_refs(weight, field_info, target_root, out);
            }
        }
        ExprKind::DistDirective { target, entries } => {
            collect_randomize_target_field_refs(target, field_info, target_root, out);
            for entry in entries {
                collect_randomize_target_field_refs(&entry.value, field_info, target_root, out);
                collect_randomize_target_field_refs(&entry.weight, field_info, target_root, out);
            }
        }
        ExprKind::NamedArg { value, .. } => {
            collect_randomize_target_field_refs(value, field_info, target_root, out);
        }
        ExprKind::StructLit { fields, .. } => {
            for field in fields {
                collect_randomize_target_field_refs(&field.value, field_info, target_root, out);
            }
        }
        ExprKind::CoverArrow { lhs, rhs, count } => {
            collect_randomize_target_field_refs(lhs, field_info, target_root, out);
            collect_randomize_target_field_refs(rhs, field_info, target_root, out);
            if let Some(count) = count {
                collect_hash_count_field_refs(count, field_info, target_root, out);
            }
        }
        ExprKind::SolveOrder { args } => {
            for arg in args {
                collect_randomize_target_field_refs(arg, field_info, target_root, out);
            }
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Time(_)
        | ExprKind::String(_)
        | ExprKind::Bool(_)
        | ExprKind::ImplicitSelf => {}
    }
}

fn collect_hash_count_field_refs(
    count: &HashCount,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
    out: &mut std::collections::HashSet<String>,
) {
    match count {
        HashCount::Const(e) => collect_randomize_target_field_refs(e, field_info, target_root, out),
        HashCount::Range { lo, hi } => {
            collect_randomize_target_field_refs(lo, field_info, target_root, out);
            collect_randomize_target_field_refs(hi, field_info, target_root, out);
        }
    }
}

/// Per-probe codegen state. The mangled read accessor is the bare
/// path `<TopModule>__DOT__harc_probes__DOT__<name>`. The `_drv`
/// and `_en` siblings (force probes only) are derived by appending
/// the suffix; see `ProbeAccessor::drive` / `enable`.
#[derive(Clone, Debug)]
struct ProbeAccessor {
    read: String,
    force: bool,
}

impl ProbeAccessor {
    fn drive(&self) -> String {
        format!("{}_drv", self.read)
    }
    fn enable(&self) -> String {
        format!("{}_en", self.read)
    }
}

struct Emitter {
    out: String,
    errors: Vec<String>,
    /// Transaction name → field metadata. Populated alongside `transactions`
    /// before main-body emission so the solver block can declare Z3 vars
    /// of the right widths and walk only the fields it should write back.
    txn_fields: std::collections::HashMap<String, Vec<TxnFieldInfo>>,
    /// Struct/transaction name → direct declared fields. Used for value-record
    /// C++ type lowering and packed TLM bridge helpers.
    record_fields: std::collections::HashMap<String, Vec<Field>>,
    /// Per-transaction `keep` constraints (spec §4). `randomize(t)`
    /// — bare or with `with …` — merges these into the Z3 solver
    /// block so the constraints are enforced. Empty entry / missing
    /// key means the transaction has no `keep`s; bare `randomize(t)`
    /// stays on the fast PRNG path.
    txn_keeps: std::collections::HashMap<String, Vec<Expr>>,
    /// Map from `probe_name` to the mangled accessor string used after
    /// `dut->rootp->`. Populated from the test's `let dut : T { probe ... }`
    /// block at emit start; consulted when lowering `dut.<X>` so that
    /// probe reads/writes route through the Verilator-bind stub instead of
    /// the (non-existent) top-level signal of the original DUT. Empty for
    /// probe-less tests — current 76 fixtures all sit in this bucket.
    probes: std::collections::HashMap<String, ProbeAccessor>,
    /// Declared bit-width of each `probe <name> : uint<W>` on the test's
    /// `let dut`. A probe read is the one `dut.<field>` shape that carries
    /// a static width, so it is the one wrapping-operator operand that
    /// resolves — TB-IR reads the same width off `PortRef::width`, and
    /// without this v1 rejected `dut.<probe> +% 1` as unknown-width while
    /// TB-IR wrapped it. Plain top-level ports stay width-erased in both.
    probe_widths: std::collections::HashMap<String, u32>,
    /// Probe names present in only part of the suite. Non-test shared
    /// callables must reject these instead of treating them as DUT ports.
    partial_probe_names: std::collections::HashSet<String>,
    /// Source identifiers that denote the suite DUT in the callable being
    /// emitted. Shared transactors may name their module handle something
    /// other than `dut`, so probe lookup cannot key on one spelling.
    probe_receivers: std::collections::HashSet<String>,
    /// Lexical source binders that shadow a probe receiver or a mangled
    /// file-scope value/callable. Each active block, test run, or callable
    /// parameter list owns one set.
    probe_shadow_scopes: Vec<std::collections::HashSet<String>>,
    /// Suite top-module type, used to distinguish DUT-handle component fields
    /// from other unknown named pointer fields.
    probe_dut_type: Option<String>,
    /// Names that more than one `let` in the current test declares.
    /// `let_widths` is keyed by bare source name with no scoping, so an
    /// inner shadow permanently clobbers the outer name's width. The
    /// direction checks have always lived with that; the narrowing check
    /// must not, or a legal `let b : uint<8> = a` after an inner
    /// `let a : uint<64>` is rejected on a width `a` never had at that
    /// point. Consulted only to suppress the narrowing check.
    shadowed_lets: std::collections::HashSet<String>,
    /// RAL regblock declarations indexed by type name. See ast.rs::
    /// `RegblockDecl`. Used to emit one POD mirror struct +
    /// `constexpr` address table per declared regblock at file scope,
    /// and to lower `regs.NAME` field accesses against the bound
    /// helper transactor at call sites.
    regblocks: std::collections::HashMap<String, RegblockDecl>,
    /// RAL addrmap declarations indexed by type name. Chip-level
    /// container; each instance owns one regblock at a distinct base
    /// address. See docs/ral-support.md §4.
    addrmaps: std::collections::HashMap<String, AddrmapDecl>,
    /// Per-test map from a `let regs : <RegblockType> = bind <helper>`
    /// variable name to its helper transactor variable name. Populated
    /// when emitting the test's `let`s; consulted when lowering
    /// `regs.<reg>` accessors so the emitted call resolves to
    /// `<helper>.write(addr, val)` / `<helper>.read(addr)`.
    let_helper: std::collections::HashMap<String, String>,
    /// Runtime problem id per concrete `randomize(...)` target source site.
    /// Emitted as metadata touches only during the scaffold phase; current
    /// behavior still uses the existing PRNG / inline Z3 solve paths.
    runtime_randomize_problem_ids: std::collections::HashMap<SourceSite, u64>,
    source_identities: std::collections::HashMap<SourceId, StableSourceIdentity>,
    randomize_site_owners: std::collections::HashMap<SourceSite, String>,
    current_source_id: SourceId,
    randomize_target_sentinel: Option<&'static str>,
    randomize_context_sentinel: Option<&'static str>,
    randomize_state_sentinel: Option<&'static str>,
    randomize_context_ident: String,
    capsule_state_ident: String,
    /// Free-standing `relation` declarations indexed by name (spec
    /// §4.2). `Call(Ident(R), args)` inside a constraint expression
    /// expands to R's body with formal parameters substituted by the
    /// call arguments. Block-body relations contribute one
    /// constraint per body expression; alias-body relations
    /// contribute one. Expansion is recursive so a relation that
    /// calls another relation flattens fully before reaching Z3.
    relations: std::collections::HashMap<String, RelationDecl>,
    /// Identifiers that are emitted as Verilator-class pointers (`VFoo*`).
    /// Field access on these uses `->` instead of `.`. Populated from
    /// `let x : <NamedType>` declarations and from function parameters
    /// whose type is a Named user type. Function-scoped: entries added by
    /// a function's params are popped when the function body finishes.
    pointer_vars: std::collections::HashSet<String>,
    /// Identifier → simple type name, for `randomize(t)` to resolve which
    /// `randomize_T()` function to call. Populated by `let t : T` and
    /// function parameters.
    let_types: std::collections::HashMap<String, String>,
    /// Per-test declared width AND signedness for `let X : uint<W>` (and
    /// sint/bits) declarations. Used by the width-method intrinsics
    /// (`.trunc<N>()` / `.zext<N>()` / `.sext<N>()` / `.resize<N>()`)
    /// to (a) reject wrong-direction casts at codegen time and (b)
    /// give sext the correct source-width for its shift-fill. Lets
    /// without an explicit type (`let x = expr`) don't populate this
    /// map — width-direction checks then fall back to best-effort
    /// inference on the RHS expression.
    ///
    /// The `signed` half is what lets a signed local compare signed inside
    /// a `blocking randomize` constraint (harc#563). It lives HERE, on the
    /// existing entry, rather than in a table of its own: harc#550 removed
    /// a flat `let_signed_widths` because a signed local in one function
    /// poisoned the name for every other, and a fourth parallel table keyed
    /// the same way would reintroduce exactly that failure. One entry per
    /// name cannot disagree with itself.
    let_widths: std::collections::HashMap<String, LetWidth>,
    /// DUT top-module port-name → per-lane bit-width for ports that
    /// flatten to a PACKED SystemVerilog vector (`Vec<Bus, N>` /
    /// multi-lane bus ports). Forwarded from `EmitOpts::vec_lane_widths`
    /// (built from the `--sv` port table). A `dut.port[i]` index whose
    /// port is in this map lowers via `harc_rt::harc_vec_lane_*<W>` so
    /// the lane access bit-extracts against Verilator's packed scalar
    /// while still indexing the ARCH native sim's C++ array.
    vec_lane_widths: std::collections::HashMap<String, u32>,
    /// Identifier → transactor mode, for let-bindings that carry a mode
    /// annotation (`let x : T active`, `let env : E passive`). The mode
    /// is recorded for the root let-name and forms the inheritance root
    /// for nested sub-component / sub-transactor fields; resolution at
    /// any path walks down field-by-field, with field-explicit modes
    /// overriding the inherited mode (same model as
    /// `emit_subcomponent_handler_registrations`). Used by the
    /// call-site passive-mode check (spec §8.1): a method call that
    /// resolves to a hookable inside `T.when_active` on a passive
    /// instance is a HARC-level error.
    let_modes: std::collections::HashMap<String, TransactorMode>,
    /// Set of transaction type names emitted in this file — guards against
    /// `randomize(t)` against a non-transaction type.
    transactions: std::collections::HashSet<String>,
    /// Set of plain struct record type names emitted in this file.
    /// Structs share the value-record C++ lowering with transactions;
    /// transactions add HVL/protocol semantics on top.
    structs: std::collections::HashSet<String>,
    /// Set of scoreboard type names emitted in this file. `let sb : X`
    /// where X is in this set lowers to `X sb;` (default-constructed)
    /// rather than the int64_t fallback.
    scoreboards: std::collections::HashSet<String>,
    /// Covergroup declarations indexed by name. `let cov : G` instantiates
    /// the struct and registers its `sample()` as a rising-edge `_checkers`
    /// closure on the primary clock. Bin counters live as `uint64_t` fields
    /// inside the struct; user-side access is `cov.cp_name.bin_name`.
    covergroups: std::collections::HashMap<String, CovergroupDecl>,
    /// Per-test list of registered `cover <expr>` points. Each entry pairs
    /// a unique tag (used to name the static hit counter in C++) with a
    /// human label for the report. Aggregated end-of-main report iterates
    /// the list and prints `[cover] H/T hit (X.X%)` plus per-point lines.
    covers: Vec<CoverInfo>,
    /// In-flight bare-identifier substitutions during monitor-body
    /// emission. Empty outside the monitor's handler emission. Lets the
    /// body reference its own fields by short name (`emit write_e(...)`)
    /// while the codegen prefixes with the let-instance name.
    field_subs: std::collections::HashMap<String, String>,
    /// Enum name → variant count; used when randomizing enum-typed fields.
    enums: std::collections::HashMap<String, usize>,
    /// Global enum-variant-name → numeric index. Used by the Z3
    /// constraint translator to resolve bare references to enum
    /// variants (e.g. `keep op != WRAP`) into their numeric encoding.
    enum_variants: std::collections::HashMap<String, i64>,
    /// Variant names that could NOT be given a file-scope `constexpr`
    /// definition, mapped to why. Two things disqualify a name: another
    /// emitted top-level C++ symbol already claims it (a transaction,
    /// struct, component, bus, ... or an `extern function`), or more than
    /// one enum declares it, so no single index is the right answer.
    ///
    /// Emitting anyway is not an option in either case. A `constexpr` that
    /// shadows a struct name makes every later `Foo f;` a hard C++ error
    /// ("must use 'struct' tag"), and an ambiguous variant would silently
    /// resolve to the first enum's index — `enum WrResp { SLVERR, OKAY }`
    /// reading `OKAY` as 0. Silence is the worse failure of the two, so
    /// the definition is suppressed and a use of the name is diagnosed.
    ///
    /// Consulted at the USE site rather than the declaration: constraint
    /// position resolves variants through `enum_variants` under its own
    /// documented first-wins rule and keeps working, so merely DECLARING
    /// a duplicate or a collision stays legal, as it is today.
    unsafe_variants: std::collections::HashMap<String, String>,
    /// Every name the emitter has bound as a local or a parameter so far.
    /// Consulted ONLY to suppress the `unsafe_variants` diagnostic: a
    /// `let OKAY : uint<8> = 7` beside an ambiguous variant `OKAY` is a
    /// perfectly good program — C++ scoping hides the file-scope name, and
    /// on `main` it compiled — so diagnosing it would reject working code.
    ///
    /// Deliberately permissive and never scope-popped. A stale entry costs
    /// a MISSED diagnostic (the user still gets the C++ error this backend
    /// produced before), while a missing entry costs a REJECTED working
    /// program. Those are not symmetric, so the set errs toward silence.
    declared_value_names: std::collections::HashSet<String>,
    /// File-scope names unavailable to compiler-generated locals. Separate
    /// from `declared_value_names`, which only suppresses unsafe-enum
    /// diagnostics for local/parameter shadowing.
    internal_reserved_names: std::collections::HashSet<String>,
    /// Folded file-scope scalar constants. Randomize constraints may reference
    /// these by source name; emission lowers them to typed Z3 values.
    consts: std::collections::HashMap<String, crate::constraints::IntegerConstSchema>,
    /// Declared bit-width of a `const`, cached beside its folded schema.
    /// §2.4 masks a wrap at the operands' declared widths, so sizing a
    /// `const` by the value of its initializer (`const BUMP : uint<16> = 10`
    /// → 4 bits) masks too narrowly and solves to a value the source rejects.
    const_widths: std::collections::HashMap<String, u32>,
    /// Property name → body expression. Populated up-front from
    /// `Item::Property` declarations so `assert property NAME` can be
    /// resolved at the call site without a separate pass.
    properties: std::collections::HashMap<String, Expr>,
    /// In-flight substitutions for temporal SystemCall expressions during
    /// property emission. Keyed by source site so equal offsets in different
    /// files remain distinct. Set/cleared around each property check.
    prop_subs: std::collections::HashMap<SourceSite, String>,
    /// Event name → C++ inner type (e.g. `uint64_t`). Populated when
    /// emitting `let e : event<T>`; consulted when emitting `emit e(arg)`
    /// and `on e(arg) ... end on` so the lambda gets the right param type.
    event_types: std::collections::HashMap<String, String>,
    /// Names of declared clocks in declaration order. Empty under
    /// single-clock backward-compat. Used by `wait N cycles on <clock>`
    /// to look up the clock's index in the runtime `clocks_` vector.
    clock_names: Vec<String>,
    /// Name of the C++ `std::vector` accumulator collecting `yield`-ed
    /// values inside the current `tseq` body. `Some("_result")` while
    /// emitting a tseq; `None` everywhere else (so a stray `yield`
    /// outside a tseq surfaces as a compile error rather than silently
    /// pushing into nothing).
    current_yield_target: Option<String>,
    current_unique_tseq_registry: Option<String>,
    /// Set of declared top-level function names. Together with
    /// `tseq_names`, this lets expression emission distinguish a global
    /// callable from a lexically shadowing local before selecting its
    /// collision-proof C++ spelling.
    function_names: std::collections::HashSet<String>,
    /// Extern callables retain their public C symbol. Bare calls are emitted
    /// with global qualification so generated run-scope binders cannot hide
    /// them.
    extern_function_names: std::collections::HashSet<String>,
    /// Set of declared tseq names. Used by `let x = TseqName(...)` to
    /// pick `auto` over `int64_t` for the local's type — a tseq call
    /// returns `std::vector<T>`, not an integer.
    tseq_names: std::collections::HashSet<String>,
    /// Component declarations indexed by name. Covers `driver`,
    /// `agent`, `env`, `sequencer`, `scoreboard` — anything with
    /// fields + `hookable` methods. Scoreboards are also registered
    /// here (in addition to the legacy `scoreboards` set) so method
    /// dispatch and field-substitution work uniformly.
    components: std::collections::HashMap<String, ComponentDecl>,
    /// Bus declarations indexed by name. Populated from inline
    /// `bus Name { ... }` items + (future) `use Name` extern import.
    /// Consulted at let-time for `let var : BusName = bind <dut-expr>`
    /// to track the bus binding, and at expression-emit time for
    /// `var.signal` and `var.channel.signal` flat-naming.
    buses: std::collections::HashMap<String, BusDecl>,
    /// Active bus bindings in the current emit context. Each entry
    /// maps a source-level identifier to `(bus_decl, dut_root_expr,
    /// signal_prefix)`:
    ///   * `bus_decl`     — bus declaration whose `signals` /
    ///                      `handshakes` constrain typed access
    ///   * `dut_root_expr` — C++ expression text for the DUT root
    ///                       (typically `"dut"`)
    ///   * `signal_prefix` — name used for flat signal lookup on the
    ///                       DUT (e.g. `"axil"` for `dut->axil_aw_addr`)
    ///
    /// For the test-scope `let axil : BusAxiLite = bind dut`, the
    /// identifier and prefix are both `"axil"`. For the driver-bound
    /// alias `let drv = bind axil` (Phase 2), inside the driver's
    /// on-handler body the bare identifier `"bus"` maps to the same
    /// `(bus_decl, root, prefix)` tuple as `"axil"` — the prefix stays
    /// `"axil"` so signal access still resolves to the DUT-level flat
    /// names. Distinguishing the lookup key from the prefix is what
    /// makes the alias work.
    bus_bindings: std::collections::HashMap<String, (BusDecl, String, String)>,
    /// Effective const-param environment per bus bind, keyed by the same
    /// binding name as `bus_bindings`. Computed once at the real bind site
    /// from the bus's param defaults overlaid with the bind-site generic
    /// overrides (`BusAxi4#(READ=0)`). Consulted when resolving a bus signal
    /// access to evaluate the signal's `generate_if` gate — a gated-OFF
    /// signal is absent (matches `arch build`'s flatten). Missing entry ⇒
    /// fall back to the bus's defaults via `bus_param_env(&bus, None)`.
    bus_param_envs: std::collections::HashMap<String, std::collections::HashMap<String, i64>>,
    /// DUT-port-level bus param overrides, keyed by DUT port name (== bind
    /// name == SV signal prefix). Forwarded from `EmitOpts::dut_bus_port_overrides`,
    /// which is filled by parsing the DUT's `.arch`/`.archi` interface. Layered
    /// onto the bus defaults + bind-site generic when computing `bus_param_envs`
    /// for a bind whose name matches a DUT port — the port's own override is
    /// authoritative for which `generate_if`-gated channels `arch build`
    /// actually flattened. See `bus_param_env_with_port_override`.
    dut_bus_port_overrides:
        std::collections::HashMap<String, std::collections::HashMap<String, i64>>,
    /// Per-bind per-signal name override table populated from
    /// `bind <dut> with { ch.sig: "port" }` clauses. Outer key is
    /// the bind variable name (matching the `sig_prefix` field of
    /// `bus_bindings`); inner key is `(channel, signal)` tuple.
    /// Lookups consult this map first; missing entries fall through
    /// to the `<prefix>_<channel>_<signal>` convention. Empty for
    /// binds without a `with { ... }` clause — most fixtures sit
    /// in this bucket.
    bus_remap:
        std::collections::HashMap<String, std::collections::HashMap<(String, String), String>>,
    /// Pending RHS-fork TLM calls in the current run/method body. Populated
    /// by `let x = fork bus.method(...)` or discarded `fork bus.method(...)`
    /// and drained by `join_all`.
    pending_tlm_forks: Vec<PendingTlmFork>,
    /// Per `(bus prefix, method)` tag counter for RHS-fork calls to
    /// `tlm_method ... : out_of_order tags N`.
    next_tlm_fork_tag: std::collections::HashMap<(String, String), u64>,
    /// True while emitting statements *directly inside the test's run
    /// coroutine body* (the `scope sim/run` block plus bare test-level
    /// stmts). When set, `wait N cycles`, bare `tick()` (the bus
    /// handshake spin loops), and bus.send/recv lowerings emit
    /// `co_await harc_rt::wait_cycles(_slot, ...)` instead of the
    /// synchronous `for (...) tick();` form.
    ///
    /// Component method bodies, `on`-event-handler closures, tseq
    /// lambdas, and free functions all run synchronously between
    /// coroutine yields — they keep the sync `tick()` lowering. This
    /// works because they only execute while the run coroutine is
    /// "running" (between its co_awaits), so a sync `tick()` from
    /// inside a method does not race the scheduler.
    in_coroutine: bool,
    /// Common-object layout flavor. [`Flavor::Legacy`] keeps the
    /// historical self-contained byte stream; [`Flavor::CommonImpl`] and
    /// [`Flavor::Capsule`] route shared-callable references through the
    /// generated per-run glue object (`_glue.` prefix) and move
    /// function-local mutable statics into per-run runtime storage so
    /// two sequential runs in one process never share state.
    flavor: Flavor,
    /// Mutable per-run cells recorded while emitting. Legacy puts the
    /// complete set on `HarcTestContext`; common shared-callable cells live
    /// on `HarcSuiteRuntime`, while capsule-owned cells are declared inside
    /// that capsule's run function.
    per_run_cells: Vec<PerRunCell>,
    /// Names of shared callables (suite functions, tseqs, hookable
    /// methods, watchdogs) that live as members of the generated glue
    /// object. Under the common layout, plain-name calls to these emit a
    /// `_glue.` prefix.
    glue_callees: std::collections::HashSet<String>,
    /// Phase 3a: list of `(scheduler_var, slot_var)` pairs for every
    /// actor coroutine that should run on its own OS thread. Each
    /// entry corresponds to a bound-driver or bound-monitor actor.
    /// At main-loop emission time, if non-empty, we wire up
    /// `_start_barrier` / `_end_barrier` (sized to `1 + len`) plus
    /// one `std::thread` per actor running the actor's
    /// per-os-thread `ThreadScheduler::tick()` between the barriers.
    /// Only populated when `mt = true`; in cooperative mode actor
    /// slots are pushed directly into the global `sched`.
    actor_threads: Vec<(String, String)>,
    /// True when `--mt` opt-in is set: emit per-actor schedulers,
    /// dual barriers, and worker thread spawns. False (default):
    /// actors join the global `sched` and tick cooperatively on
    /// the main thread. The cooperative path is faster on typical
    /// fixtures (per-cycle barrier sync exceeds per-cycle actor
    /// work on Apple Silicon — see the bench measurements in PR
    /// #16's commit message).
    mt: bool,
    /// Driver/agent type name → bus binding to use when emitting
    /// hookable method bodies on that type. Populated by a pre-scan
    /// of the test's `let X : Drv = bind axil` statements before
    /// hookable emission, so `bus.<ch>.send/recv` inside a hookable
    /// body resolves to the parent driver's bound bus. Single-
    /// instance per driver type (the first binding encountered);
    /// multi-instance support requires per-instance hookable
    /// emission and is deferred.
    driver_bus_for_hookables: std::collections::HashMap<String, (BusDecl, String, String)>,
    /// Transactor declarations indexed by name (spec §8.1). Looked
    /// up at `let xact : T mode = bind axil` time; codegen
    /// composes a synthetic ComponentDecl from `T.items` plus
    /// `T.when_active` (only when mode == Active) and reuses the
    /// existing bound-driver-actor + bound-monitor-actor lowering.
    transactors: std::collections::HashMap<String, TransactorDecl>,
    /// While we're emitting statements *inside a component instance's
    /// own body* — an `on <event>` subscriber, a bound-driver actor
    /// coroutine, a bound-monitor handshake actor — this names the
    /// fully-qualified instance path (`"ag"`, `"topenv.ag"`, etc.).
    /// `None` outside such bodies (test run code, free functions,
    /// hookable method bodies which use `self` instead).
    ///
    /// Used by the activity-tracking lowering to bump
    /// `<instance>._last_in_cycle` / `_last_out_cycle` at the sites
    /// where the framework knows an in/out has just happened:
    /// `emit ev(arg)`, `bus.<ch>.send(...)`, `bus.<ch>.recv()`. The
    /// in-handler entry bump uses the static `instance` parameter
    /// directly (already in scope), but `emit`/`send`/`recv` can
    /// appear nested inside arbitrary expressions and need this
    /// context to know whose heartbeat to bump.
    current_component_instance: Option<String>,
    /// While emitting a component method body, this records the HARC
    /// component type, C++ receiver expression, method name, and
    /// whether the method lives in `when active`, for bare sibling
    /// method calls such as `helper()` -> `Type_helper(self)`.
    current_component_method: Option<(String, String, String, bool)>,
}

/// RAII guard that temporarily overrides a single
/// `Option<(String, String, String, bool)>` slot — specifically
/// `Emitter::current_component_method` — and restores the prior
/// value on drop. Makes the install/restore invariant
/// compiler-enforced: any control-flow exit from the scope holding
/// the guard — `continue`, `return`, panic, `?`, fall-through —
/// runs the restore.
///
/// Used in `emit_component_handler_registrations_bound` where each
/// per-handler iteration has multiple exit paths (the
/// event-subscription `continue` arm and the fall-through
/// cycle-trigger arm), so that a future maintainer adding a third
/// exit doesn't have to remember a manual restore line.
///
/// The guard owns `&mut Emitter` and exposes it via
/// `Deref`/`DerefMut`, so the caller can keep using all of
/// `Emitter`'s API for the lifetime of the override. The per-handler
/// emit work is factored into a sibling helper (`emit_single_on_handler_bound`)
/// that takes `&mut self`; the caller drives that helper through the
/// guard, and restoration happens on drop after the helper returns
/// (or panics, or `?`s).
struct CurrentMethodGuard<'a> {
    emitter: &'a mut Emitter,
    saved: Option<(String, String, String, bool)>,
}

impl<'a> CurrentMethodGuard<'a> {
    fn new(emitter: &'a mut Emitter, new_value: Option<(String, String, String, bool)>) -> Self {
        let saved = std::mem::replace(&mut emitter.current_component_method, new_value);
        Self { emitter, saved }
    }
}

impl<'a> std::ops::Deref for CurrentMethodGuard<'a> {
    type Target = Emitter;
    fn deref(&self) -> &Emitter {
        self.emitter
    }
}

impl<'a> std::ops::DerefMut for CurrentMethodGuard<'a> {
    fn deref_mut(&mut self) -> &mut Emitter {
        self.emitter
    }
}

impl<'a> Drop for CurrentMethodGuard<'a> {
    fn drop(&mut self) {
        self.emitter.current_component_method = self.saved.take();
    }
}

impl Emitter {
    fn pad(&mut self, depth: usize) {
        for _ in 0..depth {
            self.out.push_str(INDENT);
        }
    }

    fn randomize_context_ref(&self) -> &str {
        self.randomize_context_sentinel
            .unwrap_or(&self.randomize_context_ident)
    }

    fn select_randomize_context_ident_from_texts<'b>(
        &mut self,
        texts: impl IntoIterator<Item = &'b str>,
    ) {
        let source_idents = texts
            .into_iter()
            .filter_map(|text| crate::lexer::tokenize(text).ok())
            .flatten()
            .filter_map(|token| match token.kind {
                crate::lexer::TokenKind::Ident(name) => Some(name),
                _ => None,
            })
            .collect::<std::collections::HashSet<_>>();
        let unique = |preferred: &str, fallback: &str| {
            if !source_idents.contains(preferred) {
                return preferred.to_string();
            }
            let mut ident = fallback.to_string();
            let mut suffix = 0u32;
            while source_idents.contains(&ident) {
                suffix += 1;
                ident = format!("{fallback}_{suffix}");
            }
            ident
        };
        self.randomize_context_ident = unique("ctx", "_harc_randomize_context");
        self.capsule_state_ident = unique("_harc_randomize_state", "_harc_randomize_capsule_state");
    }

    fn select_randomize_context_ident(&mut self, file: &SourceFile) {
        self.select_randomize_context_ident_from_texts(
            file.sources.iter().map(|source| source.text.as_ref()),
        );
    }

    fn select_randomize_context_ident_for_capsule(
        &mut self,
        file: &SourceFile,
        selected_test: &str,
    ) {
        let mut relevant = SourceFile {
            items: Vec::new(),
            item_sources: Vec::new(),
            sources: Vec::new(),
            inner_doc: None,
            frontmatter: None,
        };
        for (index, item) in file.items.iter().enumerate() {
            if matches!(item, Item::Test(test) if test.name.name != selected_test) {
                continue;
            }
            relevant.items.push(item.clone());
            relevant.item_sources.push(file.item_source(index));
        }
        let text = crate::pretty::print(&relevant);
        self.select_randomize_context_ident_from_texts([text.as_str()]);
    }

    fn randomize_state_ref(&self) -> String {
        self.randomize_state_sentinel
            .map(str::to_string)
            .unwrap_or_else(|| {
                if self.flavor == Flavor::Capsule {
                    self.capsule_state_ident.clone()
                } else {
                    format!("{}._cells", self.randomize_context_ref())
                }
            })
    }
}

fn list_max_len(t: &TypeExpr) -> Option<usize> {
    let args = list_type_args(t)?;
    for arg in args {
        if let TypeArg::Named { name, value } = arg {
            if name.name == "max" {
                return const_usize_expr(value);
            }
        }
    }
    args.get(1).and_then(type_arg_const_usize)
}

fn scalar_field_shape(t: &TypeExpr) -> (u32, bool) {
    match t {
        TypeExpr::Builtin { name, args, .. } => match name {
            BuiltinTy::UInt | BuiltinTy::Bits | BuiltinTy::UIntCap => {
                (type_arg_width(args).unwrap_or(64), false)
            }
            BuiltinTy::SInt | BuiltinTy::SIntCap => (type_arg_width(args).unwrap_or(64), true),
            BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => (1, false),
            BuiltinTy::Int => (32, true),
            _ => (64, false),
        },
        _ => (64, false),
    }
}

fn list_field_info(t: &TypeExpr) -> Option<ListFieldInfo> {
    if !is_list_type(t) {
        return None;
    }
    let elem = list_elem_type(t)?;
    let declared_max_len = list_max_len(t);
    let (elem_width, elem_signed) = scalar_field_shape(elem);
    Some(ListFieldInfo {
        declared_max_len,
        elem_width,
        elem_signed,
    })
}

/// Default initialiser for a transaction field. Uses the user's `default
/// <expr>` clause when given; falls back to type-appropriate zero / false.
/// Default exprs are restricted to simple forms (int / bool / ident / float)
/// in v0 — anything more complex falls back to `0`.
fn field_default(f: &Field) -> String {
    if let Some(d) = &f.default {
        return format_simple_expr(d);
    }
    if is_list_type(&f.ty) || fixed_vec_type_args(&f.ty).is_some() {
        return "{}".into();
    }
    match &f.ty {
        TypeExpr::Builtin { name, .. } => match name {
            BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => "false".into(),
            _ => "0".into(),
        },
        _ => "0".into(),
    }
}

fn format_simple_expr(e: &Expr) -> String {
    match &*e.kind {
        ExprKind::Int(s) => c_int_literal(s),
        ExprKind::Ident(id) => id.name.clone(),
        ExprKind::Bool(b) => (if *b { "true" } else { "false" }).into(),
        ExprKind::Float(s) => s.clone(),
        _ => "0".into(),
    }
}

/// Extract a width from a builtin's first type-arg if it's a literal (e.g.
/// `uint<32>` → Some(32)). Returns None if the arg isn't a parseable int.
fn type_arg_width(args: &[TypeArg]) -> Option<u32> {
    let arg = args.first()?;
    match arg {
        TypeArg::Expr(e) => match &*e.kind {
            ExprKind::Int(s) => s.replace('_', "").parse().ok(),
            _ => None,
        },
        _ => None,
    }
}

fn enum_width(variant_count: usize) -> u32 {
    if variant_count <= 1 {
        1
    } else {
        usize::BITS - (variant_count - 1).leading_zeros()
    }
}

/// Translate a HARC type expression to a C++ type for function parameters
/// and return types. Best-effort and conservative — integer-shaped HARC
/// types collapse to `uint64_t` / `int64_t`, since the actual width on the
/// Verilator side is determined by the DUT port type and we just shovel
/// values through. Bool stays bool. Unknown named types emit the bare
/// identifier (likely won't compile — surfaces the gap to the user).
/// Render a field-access chain like `seq.dispatched` or just `req`
/// as a dot-separated string. Returns `None` for shapes the connect
/// codegen can't handle in v0 (anything other than nested
/// `Field { Field { Ident, _ }, _ }` chains rooted at a bare ident).
fn expr_path_str(e: &Expr) -> Option<String> {
    match &*e.kind {
        ExprKind::Ident(id) => Some(id.name.clone()),
        ExprKind::Field { target, name } => {
            let head = expr_path_str(target)?;
            Some(format!("{head}.{}", name.name))
        }
        _ => None,
    }
}

/// If `event` is the trigger of an `on event_name(arg)` handler,
/// returns `(event_name, arg_binding_name)`. Anything else (a bool
/// expression like `on dut.x && y`, or a malformed call) returns
/// `None`. The binding name falls back to `_v` when the user wrote
/// `on event(_)` or omitted the arg.
/// Detect the `<bus>.<channel>.handshake(<arg>)` form used by bound
/// monitors (Phase 2c). Returns `Some((channel_name, arg_name))`
/// when the expression matches AND the outer `<bus>` ident matches
/// the expected binding name (typically `"bus"`); `None` otherwise.
///
/// The expression is a 3-level tree:
///   ExprKind::Call {
///     callee: Field { target: Field { target: Ident(bus), name: ch },
///                     name: "handshake" },
///     args:   [Expr::Ident(arg)] }
fn extract_bus_handshake_event(event: &Expr, bus_ident: &str) -> Option<(String, String)> {
    let ExprKind::Call { callee, args } = &*event.kind else {
        return None;
    };
    let ExprKind::Field {
        target,
        name: method,
    } = &*callee.kind
    else {
        return None;
    };
    if method.name != "handshake" {
        return None;
    }
    let ExprKind::Field {
        target: outer,
        name: ch,
    } = &*target.kind
    else {
        return None;
    };
    let ExprKind::Ident(id) = &*outer.kind else {
        return None;
    };
    if id.name != bus_ident {
        return None;
    }
    let arg_name = match args.first() {
        Some(CallArg::Expr(e)) => match &*e.kind {
            ExprKind::Ident(id) => id.name.clone(),
            _ => "_v".into(),
        },
        _ => "_v".into(),
    };
    Some((ch.name.clone(), arg_name))
}

fn extract_event_subscription(event: &Expr) -> Option<(String, String)> {
    let ExprKind::Call { callee, args } = &*event.kind else {
        return None;
    };
    let event_name = match &*callee.kind {
        ExprKind::Ident(id) => id.name.clone(),
        _ => return None,
    };
    let arg_name = match args.first() {
        Some(CallArg::Expr(e)) => match &*e.kind {
            ExprKind::Ident(id) => id.name.clone(),
            _ => "_v".into(),
        },
        _ => "_v".into(),
    };
    Some((event_name, arg_name))
}

/// True when `let x = EXPR` should use `auto` rather than `int64_t`
/// for the local's declared type. Picks `auto` whenever the rhs is a
/// call (which can return `std::vector<T>` from a tseq, or a
/// transaction by value from `queue<T>::pop()`, etc.) and falls back
/// to `int64_t` for everything else (DUT signal reads, plain numeric
/// expressions — `int64_t` zero-extends them safely for comparisons
/// against int literals). The `tseq_names` arg is kept for future use
/// but the current decision is broader than tseq alone.
fn rhs_wants_auto(e: &Expr, _tseq_names: &std::collections::HashSet<String>) -> bool {
    match &*e.kind {
        ExprKind::Call { .. } => true,
        // A wrapping operator's residue is UNSIGNED (spec §2.4), and the
        // emitted mask is already spelled `((uint64_t)(…))`. Defaulting
        // the local to `int64_t` reinterpreted that residue as signed, so
        // `let y = a -% 1` on a `uint<64>` gave `y > 0` false under v1 and
        // true under TB-IR, whose local is `uint64_t`. Letting `auto`
        // deduce from the mask puts the two backends on the same type.
        ExprKind::Binary { op, .. } => matches!(
            op,
            BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap
        ),
        // `(a -% 1)` is the same wrap: without this the width-64
        // signedness divergence survived a single pair of parentheses.
        ExprKind::Paren(inner) => rhs_wants_auto(inner, _tseq_names),
        _ => false,
    }
}

/// Evaluate a HARC expression as a compile-time integer width. Used by
/// the width-method intrinsics (`.trunc<N>()` / `.zext<N>()` /
/// `.sext<N>()` / `.resize<N>()`) to extract `N`. Today recognizes only
/// integer literals (the common case); a future pass could fold `const
/// NAME : <Ty> = <expr>` references too.
fn eval_const_width(e: &Expr) -> Option<u32> {
    match &*e.kind {
        ExprKind::Paren(inner) => eval_const_width(inner),
        ExprKind::Int(s) => {
            let stripped = s.replace('_', "");
            if let Some(rest) = stripped
                .strip_prefix("0x")
                .or_else(|| stripped.strip_prefix("0X"))
            {
                u32::from_str_radix(rest, 16).ok()
            } else if let Some(rest) = stripped
                .strip_prefix("0b")
                .or_else(|| stripped.strip_prefix("0B"))
            {
                u32::from_str_radix(rest, 2).ok()
            } else {
                stripped.parse::<u32>().ok()
            }
        }
        _ => None,
    }
}

/// Whether an expression reads a name that more than one `let` declares.
/// Such a name has no single recorded width (see `Emitter::shadowed_lets`),
/// so any width derived from it is not trustworthy for a rejection.
fn references_shadowed_let(e: &Emitter, v: &Expr) -> bool {
    match &*v.kind {
        ExprKind::Ident(id) => e.shadowed_lets.contains(&id.name),
        ExprKind::Paren(inner) => references_shadowed_let(e, inner),
        ExprKind::Binary { lhs, rhs, .. } => {
            references_shadowed_let(e, lhs) || references_shadowed_let(e, rhs)
        }
        ExprKind::Unary { expr, .. } => references_shadowed_let(e, expr),
        // A CAST is where the recursion stops: `wrap_operand_width` takes
        // a cast operand's width from the cast target, not from whatever
        // is inside it, so a shadowed name under one contributes nothing
        // to the width and must not suppress the check. Recursing here
        // let `let b : uint<4> = (a as uint<8>) +% 1` through under v1
        // while TB-IR rejected it.
        ExprKind::Cast { .. } => false,
        _ => false,
    }
}

/// Width of an initializer for the narrowing check, or `None` when the
/// shape carries no width worth comparing.
///
/// A literal that fits in 64 bits is exempt, matching TB-IR, which types
/// one as widthless (`UInt(None)`) and treats that as an assignment
/// wildcard — counting its minimum *value* width would make v1 reject
/// `let b : uint<8> = 300` alone. A literal that does NOT fit is a
/// different case: TB-IR types it as `WideLiteral` and does check it, and
/// v1 was silently storing the low 64 bits of an 80-bit value into a
/// local the user declared 8 bits wide.
fn narrowing_source_width(e: &Emitter, value: &Expr) -> Option<u32> {
    match &*value.kind {
        ExprKind::Paren(inner) => narrowing_source_width(e, inner),
        ExprKind::Int(s) => wide_literal_bit_width(s),
        // A name several `let`s declare has no single recorded width —
        // `let_widths` keeps the last one seen, which may belong to an
        // inner scope that has already closed. Decline rather than
        // reject on a width the value may never have had.
        ExprKind::Ident(id) if e.shadowed_lets.contains(&id.name) => None,
        // A wrap's residue is `max(W(lhs), W(rhs))` bits wide, so it is a
        // width-carrying source: `let x : uint<4> = a +% 1` on an 8-bit
        // `a` narrows. TB-IR sees this (the wrap's implicit mask is a
        // `WidthCast`, which its check covers); v1 accepted it and stored
        // the unmasked 8-bit residue in a 4-bit-declared local.
        ExprKind::Binary { op, lhs, rhs }
            if matches!(
                op,
                BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap
            ) =>
        {
            // The operands go through `let_widths` too, so the
            // shadowed-name guard above has to cover them or it is simply
            // bypassed: `let b : uint<8> = a +% 1` after an inner
            // `let a : uint<64>` was rejected on a width `a` never had.
            if references_shadowed_let(e, lhs) || references_shadowed_let(e, rhs) {
                return None;
            }
            e.wrap_result_width(*op, lhs, rhs).ok()
        }
        _ => e.infer_expr_width_best_effort(value),
    }
}

/// `Some(bits)` when an integer literal's text denotes a value wider than
/// 64 bits, using the same bit count TB-IR's `wide_literal_bits` reports
/// so the two backends quote the same number in the same diagnostic.
///
/// `None` means "not a narrowing source": the value fits in 64 bits, or
/// the text is a form this function does not decode. Never guess a width
/// for text that failed to parse — an earlier version returned a
/// hard-coded 129 for any parse failure, which rejected every ARCH sized
/// literal (`8'hFF`, `4'b1010`), a form v1 fully supports and TB-IR
/// explicitly delegates to v1.
fn wide_literal_bit_width(s: &str) -> Option<u32> {
    let t = s.replace('_', "");
    // Sized ARCH literals (`8'hFF`) carry their own width and are v1-only;
    // exempt them exactly as a bare literal that fits is exempt.
    if t.contains('\'') {
        return None;
    }
    let (digits, bits_per_digit) =
        if let Some(r) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
            (r, 4)
        } else if let Some(r) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
            (r, 1)
        } else {
            // Decimal: no digit-count shortcut, so bound it with u128 and
            // decline to guess beyond that rather than invent a width.
            return match t.parse::<u128>() {
                Ok(v) if v > u64::MAX as u128 => Some(128 - v.leading_zeros()),
                _ => None,
            };
        };
    // Radix-aligned: the exact width is derivable from the digit count, so
    // this stays correct past 128 bits where `u128` would overflow.
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_digit(1 << bits_per_digit)) {
        return None;
    }
    let lead = u32::from_str_radix(&trimmed[..1], 1 << bits_per_digit).ok()?;
    let lead_bits = 32 - lead.leading_zeros();
    let bits = (trimmed.len() as u32 - 1) * bits_per_digit + lead_bits;
    (bits > 64).then_some(bits)
}

/// Declared width and signedness of a typed `let`. See `Emitter::let_widths`
/// for why signedness rides on this entry rather than in its own table.
#[derive(Clone, Copy, Debug)]
struct LetWidth {
    bits: u32,
    signed: bool,
}

/// Signedness used by native solver operators. Untyped literals participate
/// in the usual arithmetic conversion of their other operand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConstraintOperandSignedness {
    Signed,
    Unsigned,
    UntypedLiteral,
}

fn scalar_let_width(ty: &TypeExpr) -> Option<LetWidth> {
    let TypeExpr::Builtin { name, args, .. } = ty else {
        return None;
    };
    match name {
        BuiltinTy::Bit | BuiltinTy::Bool | BuiltinTy::BoolLower => Some(LetWidth {
            bits: 1,
            signed: false,
        }),
        BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits => {
            type_arg_width(args).map(|bits| LetWidth {
                bits,
                signed: false,
            })
        }
        BuiltinTy::SInt | BuiltinTy::SIntCap => {
            type_arg_width(args).map(|bits| LetWidth { bits, signed: true })
        }
        _ => None,
    }
}

fn component_field_type(items: &[ComponentItem], name: &str) -> Option<TypeExpr> {
    items.iter().find_map(|item| match item {
        ComponentItem::Field(field) if field.name.name == name => Some(field.ty.clone()),
        _ => None,
    })
}

/// A one-bit UNSIGNED scalar field, the only field shape that reads as a
/// condition on its own (`keep flag` means `flag != 0`). A signed one-bit
/// field, an enum field and a list are all excluded.
fn is_flag_field(f: &TxnFieldInfo) -> bool {
    f.list.is_none() && f.enum_variants.is_none() && f.width == 1 && !f.signed
}

/// HARC spelling of an operator, for diagnostics. Distinct from
/// `c_binary_op`, which gives the C++ spelling and so collapses `+%` onto
/// `+` — an error message quoting the operator back to the author has to
/// use the one they wrote.
///
/// Every arm is spelled out rather than delegating the tail to
/// `c_binary_op`. That delegation was safe only by luck: `c_binary_op`
/// renders the temporal and membership operators as
/// `"/* unsupported-op */ ,"`, so the first person to add `in` or `|->` to
/// the caller's reject list would have shipped that string quoted inside a
/// user-facing diagnostic. Now the compiler makes them choose a spelling.
fn constraint_op_spelling(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Mod => "%",
        AddWrap => "+%",
        SubWrap => "-%",
        MulWrap => "*%",
        BitAnd => "&",
        BitOr => "|",
        BitXor => "^",
        Shl => "<<",
        Shr => ">>",
        Eq => "==",
        Ne => "!=",
        Lt => "<",
        Le => "<=",
        Gt => ">",
        Ge => ">=",
        AndAnd | AndKw => "&&",
        OrOr | OrKw => "||",
        In => "in",
        Inside => "inside",
        PipeImplies => "|->",
        PipeImpliesNext => "|=>",
        Throughout => "throughout",
        Within => "within",
        Intersect => "intersect",
    }
}

fn c_binary_op(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        Add | AddWrap => "+",
        Sub | SubWrap => "-",
        Mul | MulWrap => "*",
        Div => "/",
        Mod => "%",
        Eq => "==",
        Ne => "!=",
        Lt => "<",
        Le => "<=",
        Gt => ">",
        Ge => ">=",
        AndAnd | AndKw => "&&",
        OrOr | OrKw => "||",
        BitAnd => "&",
        BitOr => "|",
        BitXor => "^",
        Shl => "<<",
        Shr => ">>",
        // Temporal / membership operators have no direct C++ equivalent — they
        // shouldn't appear in v0 cpp_tb input. Emit a placeholder rather than
        // fail hard so the whole emit doesn't abort on one stray operator.
        PipeImplies | PipeImpliesNext | Throughout | Within | Intersect | In | Inside => {
            "/* unsupported-op */ ,"
        }
    }
}

/// Constant-fold a base/size expression to `u64` for the addrmap
/// overlap check. Handles plain integer literals only (hex/decimal/
/// underscored). Non-literal expressions fall back to `None` — the
/// overlap check skips pairs whose bounds aren't computable, which
/// keeps the static analysis honest without surfacing false
/// positives.
fn fold_int_literal(e: &Expr) -> Option<u64> {
    match &*e.kind {
        ExprKind::Int(s) => parse_int_str(s),
        _ => None,
    }
}

fn fold_signed_int_literal(e: &Expr) -> Option<i64> {
    match &*e.kind {
        ExprKind::Int(s) => i64::try_from(parse_int_str(s)?).ok(),
        ExprKind::Unary {
            op: UnaryOp::Neg,
            expr,
        } => i64::try_from(fold_int_literal(expr)?).ok().map(|v| -v),
        ExprKind::Paren(inner) => fold_signed_int_literal(inner),
        _ => None,
    }
}

fn parse_int_str(s: &str) -> Option<u64> {
    let cleaned: String = s.chars().filter(|c| *c != '_').collect();
    if let Some(idx) = cleaned.find('\'') {
        let (_, tail) = cleaned.split_at(idx + 1);
        let kind = tail.chars().next()?;
        let digits: &str = &tail[1..];
        match kind {
            'h' | 'H' => u64::from_str_radix(digits, 16).ok(),
            'b' | 'B' => u64::from_str_radix(digits, 2).ok(),
            'o' | 'O' => u64::from_str_radix(digits, 8).ok(),
            _ => digits.parse::<u64>().ok(),
        }
    } else if let Some(hex) = cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).ok()
    } else if let Some(bin) = cleaned
        .strip_prefix("0b")
        .or_else(|| cleaned.strip_prefix("0B"))
    {
        u64::from_str_radix(bin, 2).ok()
    } else {
        cleaned.parse::<u64>().ok()
    }
}

/// Validates `alias of` targets: target instance must exist in the
/// same addrmap, must not itself be aliased (chained aliases are
/// out of scope for Phase 1g), and must have the same regblock
/// type as the alias (otherwise the shared mirror would be a
/// type error).
fn check_addrmap_aliases(a: &AddrmapDecl) -> Option<String> {
    for inst in &a.instances {
        let Some(target_name) = &inst.alias_of else {
            continue;
        };
        let target = a.instances.iter().find(|i| i.name.name == target_name.name);
        let Some(target) = target else {
            return Some(format!(
                "addrmap `{}`: instance `{}` aliases `{}`, but no such instance exists in this addrmap",
                a.name.name, inst.name.name, target_name.name,
            ));
        };
        if target.alias_of.is_some() {
            return Some(format!(
                "addrmap `{}`: instance `{}` aliases `{}`, which is itself an alias — chained aliases are not supported",
                a.name.name, inst.name.name, target_name.name,
            ));
        }
        if target.regblock_ty.name != inst.regblock_ty.name {
            return Some(format!(
                "addrmap `{}`: instance `{}` (type `{}`) aliases `{}` (type `{}`) — alias target must share the regblock type",
                a.name.name, inst.name.name, inst.regblock_ty.name,
                target_name.name, target.regblock_ty.name,
            ));
        }
    }
    None
}

/// Walks an addrmap's instance list and returns an error message
/// when any two sized instances have overlapping windows. Windows
/// are half-open `[base, base + size)`. Skips pairs where either
/// instance lacks `size`, or where either instance aliases the
/// other (or both alias the same target — RFC §4 explicitly
/// permits this).
fn pair_is_aliased(a: &InstanceDecl, b: &InstanceDecl) -> bool {
    if let Some(t) = &a.alias_of {
        if t.name == b.name.name {
            return true;
        }
    }
    if let Some(t) = &b.alias_of {
        if t.name == a.name.name {
            return true;
        }
    }
    // Both alias the same third instance.
    match (&a.alias_of, &b.alias_of) {
        (Some(ta), Some(tb)) if ta.name == tb.name => true,
        _ => false,
    }
}

fn check_addrmap_overlap(a: &AddrmapDecl) -> Option<String> {
    let sized: Vec<(usize, &InstanceDecl, u64, u64)> = a
        .instances
        .iter()
        .enumerate()
        .filter_map(|(i, inst)| {
            let base = fold_int_literal(&inst.base_addr)?;
            let size = fold_int_literal(inst.size.as_ref()?)?;
            Some((i, inst, base, base.saturating_add(size)))
        })
        .collect();
    for w in 0..sized.len() {
        for x in (w + 1)..sized.len() {
            let (_i_a, a_inst, a_lo, a_hi) = &sized[w];
            let (_i_b, b_inst, b_lo, b_hi) = &sized[x];
            if pair_is_aliased(a_inst, b_inst) {
                continue;
            }
            // Half-open overlap test.
            if *a_lo < *b_hi && *b_lo < *a_hi {
                return Some(format!(
                    "addrmap `{}`: instance `{}` [0x{:x}, 0x{:x}) overlaps instance `{}` [0x{:x}, 0x{:x})",
                    a.name.name,
                    a_inst.name.name, a_lo, a_hi,
                    b_inst.name.name, b_lo, b_hi,
                ));
            }
        }
    }
    None
}

/// Enforce: a transactor's always-on body (anything not under
/// `when active { ... }`) must not drive DUT signals. Drive-capable
/// hookables / on-handlers must live inside `when active`.
///
/// Rationale (spec §8.1): a `passive` instance literally has its
/// `when active` body elided at codegen (see
/// `synth_component_from_transactor`). If drive code lived in the
/// always-on portion, a passive instance would still emit that code
/// and could end up wiring an observer to the bus — exactly the
/// footgun this check prevents. The common block-level→chip-level
/// TB reuse pattern (passive instance at chip level monitoring a bus
/// the chip already drives) silently miscompiles otherwise.
///
/// "Drive" is:
/// - assignment whose LHS is a field-access chain rooted at a
///   non-HARC-inner-typed field (treated as a DUT pointer), or
///   rooted at the implicit `bus` alias of a `bound to BusType`
///   transactor.
/// - call to `<bus>.<ch>.send(...)` or `<bus>.<ch>.recv()` — both
///   drive the channel's valid (send) or ready (recv) line.
/// - `release <expr>` — pairs with `probe force` writes.
/// - `<lhs> <- <rhs>` channel-send statement on a DUT-rooted target.
///
/// Returns an error message string on the first violation (caller
/// converts to `EmitError`).
fn check_transactor_no_drive_in_always_on_body(
    t: &TransactorDecl,
    transactors: &std::collections::HashMap<String, TransactorDecl>,
    components: &std::collections::HashMap<String, ComponentDecl>,
    scoreboards: &std::collections::HashSet<String>,
    covergroups: &std::collections::HashMap<String, CovergroupDecl>,
    buses: &std::collections::HashMap<String, BusDecl>,
    regblocks: &std::collections::HashMap<String, RegblockDecl>,
    addrmaps: &std::collections::HashMap<String, AddrmapDecl>,
) -> Option<String> {
    // 1) Decide which of T's fields are HARC inner constructs (and so
    //    safe to read/write at the field level) versus DUT pointers
    //    (member-write counts as a drive). Builtins (uint/sint/bool/
    //    queue/event/...) are excluded — they're plain inner state.
    let mut dut_field_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for it in &t.items {
        if let ComponentItem::Field(f) = it {
            let TypeExpr::Named { name, .. } = &f.ty else {
                continue; // builtin / event / queue → not a DUT pointer
            };
            let Some(simple) = name.segments.last().map(|s| s.name.as_str()) else {
                continue;
            };
            let is_harc_inner = transactors.contains_key(simple)
                || components.contains_key(simple)
                || scoreboards.contains(simple)
                || covergroups.contains_key(simple)
                || regblocks.contains_key(simple)
                || addrmaps.contains_key(simple)
                || buses.contains_key(simple);
            if !is_harc_inner {
                dut_field_names.insert(f.name.name.clone());
            }
        }
    }
    let is_bound_to_bus = t.bound_to.is_some();

    // 2) Walk every hookable + on-handler in the always-on body
    //    (skipping `t.when_active`, which is the legitimate home for
    //    drive code).
    for it in &t.items {
        match it {
            ComponentItem::Hookable(h) => {
                if let Some(violation) =
                    find_drive_in_block(&h.body, &dut_field_names, is_bound_to_bus)
                {
                    return Some(format!(
                        "transactor `{}` hookable `{}` drives a DUT signal ({}) from the always-on body. \
                         Passive instances would still execute this code. Move the hookable into a \
                         `when active ... end when` block, or remove the drive. See spec §8.1.",
                        t.name.name, h.name.name, violation,
                    ));
                }
            }
            ComponentItem::OnHandler(h) => {
                if let Some(violation) =
                    find_drive_in_block(&h.body, &dut_field_names, is_bound_to_bus)
                {
                    return Some(format!(
                        "transactor `{}` `on`-handler drives a DUT signal ({}) from the always-on body. \
                         Passive instances would still execute this code. Move the handler into a \
                         `when active ... end when` block, or remove the drive. See spec §8.1.",
                        t.name.name, violation,
                    ));
                }
            }
            _ => {}
        }
    }
    None
}

/// Returns the offending source snippet describing the first drive
/// statement found in `block`, or `None` if the block is drive-free.
fn find_drive_in_block(
    block: &Block,
    dut_fields: &std::collections::HashSet<String>,
    bound_to_bus: bool,
) -> Option<String> {
    for s in &block.stmts {
        if let Some(v) = find_drive_in_stmt(s, dut_fields, bound_to_bus) {
            return Some(v);
        }
    }
    None
}

fn find_drive_in_stmt(
    s: &Stmt,
    dut_fields: &std::collections::HashSet<String>,
    bound_to_bus: bool,
) -> Option<String> {
    match &s.kind {
        StmtKind::Assign { target, value: _ } | StmtKind::Send { target, value: _ } => {
            if let Some(root) = expr_root_ident(target) {
                if has_field_access(target) {
                    if dut_fields.contains(&root) {
                        return Some(format!("`{}` = ...", expr_source_str(target)));
                    }
                    if bound_to_bus && root == "bus" {
                        return Some(format!("`{}` = ...", expr_source_str(target)));
                    }
                }
            }
            None
        }
        StmtKind::Release(e) => Some(format!("release {}", expr_source_str(e))),
        StmtKind::Expr(e) => find_drive_in_expr(e, dut_fields, bound_to_bus),
        StmtKind::Let(l) => l
            .value
            .as_ref()
            .and_then(|v| find_drive_in_expr(v, dut_fields, bound_to_bus)),
        StmtKind::For(f) => find_drive_in_block(&f.body, dut_fields, bound_to_bus),
        StmtKind::Repeat(r) => find_drive_in_block(&r.body, dut_fields, bound_to_bus),
        StmtKind::Loop(b) => find_drive_in_block(b, dut_fields, bound_to_bus),
        StmtKind::While { body, .. } => find_drive_in_block(body, dut_fields, bound_to_bus),
        StmtKind::If(ifs) => find_drive_in_if(ifs, dut_fields, bound_to_bus),
        StmtKind::Fork(fk) => fk
            .branches
            .iter()
            .find_map(|b| find_drive_in_block(b, dut_fields, bound_to_bus)),
        StmtKind::Parallel(blocks) | StmtKind::Schedule(blocks) => blocks
            .iter()
            .find_map(|b| find_drive_in_block(b, dut_fields, bound_to_bus)),
        StmtKind::Select(arms) => arms
            .iter()
            .find_map(|a| find_drive_in_block(&a.action, dut_fields, bound_to_bus)),
        StmtKind::On(h) => find_drive_in_block(&h.body, dut_fields, bound_to_bus),
        StmtKind::After { body, .. } => find_drive_in_block(body, dut_fields, bound_to_bus),
        _ => None,
    }
}

fn find_drive_in_if(
    ifs: &IfStmt,
    dut_fields: &std::collections::HashSet<String>,
    bound_to_bus: bool,
) -> Option<String> {
    if let Some(v) = find_drive_in_block(&ifs.then_block, dut_fields, bound_to_bus) {
        return Some(v);
    }
    for (_cond, body) in &ifs.elsifs {
        if let Some(v) = find_drive_in_block(body, dut_fields, bound_to_bus) {
            return Some(v);
        }
    }
    if let Some(else_b) = &ifs.else_block {
        if let Some(v) = find_drive_in_block(else_b, dut_fields, bound_to_bus) {
            return Some(v);
        }
    }
    None
}

fn find_drive_in_expr(
    e: &Expr,
    dut_fields: &std::collections::HashSet<String>,
    bound_to_bus: bool,
) -> Option<String> {
    // The only expression-position drive we flag is a method call
    // `<bus>.<ch>.send(...)` or `<bus>.<ch>.recv()` — both drive a
    // handshake line.
    if let ExprKind::Call { callee, args } = &*e.kind {
        if let ExprKind::Field { target, name } = &*callee.kind {
            if (name.name == "send" || name.name == "recv") && bound_to_bus {
                if let ExprKind::Field {
                    target: inner,
                    name: _,
                } = &*target.kind
                {
                    if let ExprKind::Ident(id) = &*inner.kind {
                        if id.name == "bus" {
                            return Some(format!("{}(...)", expr_source_str(callee)));
                        }
                    }
                }
            }
        }
        // Recurse into args in case a deeply-nested call drives.
        for a in args {
            let arg_expr = match a {
                CallArg::Expr(x) => x,
                CallArg::Named { value, .. } => value,
            };
            if let Some(v) = find_drive_in_expr(arg_expr, dut_fields, bound_to_bus) {
                return Some(v);
            }
        }
    }
    None
}

fn expr_root_ident(e: &Expr) -> Option<String> {
    let mut cur = e;
    loop {
        match &*cur.kind {
            ExprKind::Ident(id) => return Some(id.name.clone()),
            ExprKind::Field { target, .. } => cur = target,
            ExprKind::Index { target, .. } => cur = target,
            ExprKind::Paren(inner) => cur = inner,
            _ => return None,
        }
    }
}

fn has_field_access(e: &Expr) -> bool {
    matches!(&*e.kind, ExprKind::Field { .. })
        || matches!(&*e.kind, ExprKind::Index { target, .. } if has_field_access(target))
        || matches!(&*e.kind, ExprKind::Paren(inner) if has_field_access(inner))
}

/// Width in bits of a RAL field declared as `field name : <ty>`.
/// `bit` / `bool` collapse to 1; `uint<N>` / `sint<N>` / `bits<N>` /
/// `UInt<N>` / `SInt<N>` use the type argument. Falls back to 1 for
/// shapes the parser shouldn't have accepted (caller reports a clean
/// error path).
fn field_bit_width(t: &TypeExpr) -> u32 {
    match t {
        TypeExpr::Builtin { name, args, .. } => match name {
            BuiltinTy::Bit | BuiltinTy::Bool | BuiltinTy::BoolLower => 1,
            BuiltinTy::UInt
            | BuiltinTy::SInt
            | BuiltinTy::Bits
            | BuiltinTy::UIntCap
            | BuiltinTy::SIntCap => type_arg_width(args).unwrap_or(1),
            _ => 1,
        },
        _ => 1,
    }
}

/// Right-aligned bit mask for a `width`-bit field, suitable for emit
/// as `0x{mask:x}u`. Regblock validation accepts fields up to 64 bits,
/// and mirrors wider than 32 bits use `uint64_t`.
fn field_mask_literal(width: u32) -> u64 {
    match width {
        0 => 0,
        1..=63 => (1u64 << width) - 1,
        _ => u64::MAX,
    }
}

/// C++ unsigned integer type wide enough to hold a register of `width`
/// bits. Phase 1a caps at 64; wider registers (e.g. AES blocks) would
/// need the `harc_rt::harc_u128` story used elsewhere in this file.
fn mirror_field_c_type(width: u32) -> &'static str {
    match width {
        1..=8 => "uint8_t",
        9..=16 => "uint16_t",
        17..=32 => "uint32_t",
        _ => "uint64_t",
    }
}

/// Render an integer-literal `ExprKind` as a C++ integer literal.
/// Non-integer expressions fall back to `0` so the emitted code still
/// compiles; the parser path that calls this only fires on `Int(...)`
/// today because reset values and addresses are parsed with
/// `parse_expr` and the codegen surfaces dynamic expressions through
/// the main emit path. Phase 1a callers always supply Int kinds.
fn c_int_literal_from(k: &ExprKind) -> String {
    match k {
        ExprKind::Int(s) => c_int_literal(s),
        _ => "0".to_string(),
    }
}

/// An ARCH-style sized literal (`8'hAB`, `4'd3`, `1'b1`) rewritten as
/// the plain C++ spelling this backend folds it to (`0xAB`, `3`, `0b1`)
/// — width prefix dropped, base letter turned into a C prefix,
/// underscores stripped. An unsized literal only loses its underscores.
///
/// `pub(crate)` because the width prefix is NOT the value: `4'd3` folds
/// to a correct `3`, while `128'hFF..FF` folds to a 128-bit composite
/// that a 64-bit member truncates. The TB-IR lowering classifies a
/// default it cannot read by asking what this produces and whether the
/// result fits, rather than by splitting on the apostrophe.
pub(crate) fn normalized_int_literal(s: &str) -> String {
    if let Some(idx) = s.find('\'') {
        let (_, tail) = s.split_at(idx + 1);
        let kind = tail.chars().next().unwrap_or('d');
        let digits: String = tail[1..].chars().filter(|c| *c != '_').collect();
        match kind {
            'h' | 'H' => format!("0x{digits}"),
            'b' | 'B' => format!("0b{digits}"),
            _ => digits,
        }
    } else {
        s.replace('_', "")
    }
}

fn c_int_literal(s: &str) -> String {
    // ARCH-style sized literals (`8'hAB`) → C++ `0xAB`. Then the
    // unsized-literal post-pass below catches any >64-bit cases.
    let normalized = normalized_int_literal(s);

    // Hex literals wider than 64 bits (>16 hex digits) overflow C++'s
    // unsigned long long. Lower as a composite `_harc_u128` shifted-OR
    // so the value flows through `harc_assign` / `harc_read` and 128-
    // bit comparisons. Two halves are sufficient for the v0 cap of
    // 128 bits; literals above that are clamped to the low 128 bits.
    let (prefix, digits) = if let Some(d) = normalized.strip_prefix("0x") {
        ("0x", d)
    } else if let Some(d) = normalized.strip_prefix("0X") {
        ("0x", d)
    } else {
        ("", normalized.as_str())
    };
    if !prefix.is_empty() && digits.len() > 16 && digits.len() <= 32 {
        // 65..128 bits — fits in `_harc_u128`. Split into two 64-bit
        // halves and emit the composite shifted-OR.
        let hex_lo_start = digits.len() - 16;
        let lo = &digits[hex_lo_start..];
        let hi = &digits[..hex_lo_start];
        return format!("(((_harc_u128)0x{hi}ULL << 64) | (_harc_u128)0x{lo}ULL)");
    }
    // > 128 bits: handled by `c_wide_lit_words` at the assign / equality
    // call sites — they emit `harc_assign_words` / `harc_eq_words` with
    // an `std::initializer_list<uint32_t>` instead. If we got here with
    // a > 128b literal it means the literal escaped into a context that
    // can't take a word-list (e.g. used as an arithmetic operand);
    // fall back to a clamped composite to keep the output compilable
    // with a clear narrowing warning rather than a crash.
    if !prefix.is_empty() && digits.len() > 32 {
        let lo = &digits[digits.len() - 16..];
        let hi = &digits[digits.len() - 32..digits.len() - 16];
        return format!("(((_harc_u128)0x{hi}ULL << 64) | (_harc_u128)0x{lo}ULL)");
    }
    normalized
}

fn c_value_literal(s: &str) -> String {
    if let Some(words) = c_wide_lit_words(s) {
        let mut out = format!("harc_rt::HarcWide<{}>({{", words.len());
        for (i, w) in words.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(w);
        }
        out.push_str("})");
        return out;
    }
    c_int_literal(s)
}

/// If the integer literal `s` is hex with > 32 hex digits (i.e. >
/// 128 bits), return its decomposition into 32-bit words in LSB-
/// first order — `vec!["0x...", "0x...", ...]`. Each string is a
/// `uint32_t` hex literal. Returns `None` for narrower or non-hex
/// literals; callers fall back to `c_int_literal` for those.
///
/// Used by the assign and equality lowering paths: a literal that
/// can't fit in `_harc_u128` flows through `harc_assign_words` /
/// `harc_eq_words` instead, which take `std::initializer_list<uint32_t>`.
fn c_wide_lit_words(s: &str) -> Option<Vec<String>> {
    let normalized = if let Some(idx) = s.find('\'') {
        let (_, tail) = s.split_at(idx + 1);
        let kind = tail.chars().next().unwrap_or('d');
        let digits: String = tail[1..].chars().filter(|c| *c != '_').collect();
        match kind {
            'h' | 'H' => format!("0x{digits}"),
            _ => return None,
        }
    } else {
        s.replace('_', "")
    };
    let hex = normalized
        .strip_prefix("0x")
        .or_else(|| normalized.strip_prefix("0X"))?;
    if hex.len() <= 32 {
        return None;
    }
    // Split into 8-hex-digit (32-bit) chunks, LSB-first.
    let mut words = Vec::new();
    let mut remaining = hex.len();
    while remaining > 0 {
        let start = remaining.saturating_sub(8);
        let chunk = &hex[start..remaining];
        words.push(format!("0x{chunk}u"));
        remaining = start;
    }
    Some(words)
}

/// Number of 32-bit words the *value* of a `c_wide_lit_words` word list
/// actually needs — i.e. one past the index of the highest non-zero
/// word (minimum 1). This is the value-based "required width" used by
/// the over-width assignment guard: leading-zero high words (a literal
/// written wider than necessary) do NOT inflate the count, so a value
/// that fits the port is never flagged, while a value with set bits
/// above the port width is.
///
/// `words` are LSB-first `uint32_t` hex literals like `"0x1800u"`, as
/// produced by `c_wide_lit_words`.
fn significant_word_count(words: &[String]) -> usize {
    for (i, w) in words.iter().enumerate().rev() {
        let digits = w
            .trim_end_matches(['u', 'U'])
            .trim_start_matches("0x")
            .trim_start_matches("0X");
        if u32::from_str_radix(digits, 16).unwrap_or(0) != 0 {
            return i + 1;
        }
    }
    1
}

/// Returns true if `e` is an `Int` literal whose value is wider than
/// 128 bits — i.e. the assign / equality call site should route
/// through `harc_assign_words` / `harc_eq_words` with the word-list
/// from `c_wide_lit_words` rather than the `_harc_u128` composite
/// from `c_int_literal`.
fn is_wide_int_literal(e: &Expr) -> Option<Vec<String>> {
    if let ExprKind::Int(s) = &*e.kind {
        c_wide_lit_words(s)
    } else {
        None
    }
}

/// Convert a HARC time literal (`5ns`, `20ns`, `100ps`, `2us`, `1ms`, `1s`)
/// to picoseconds. Returns `Err(msg)` on unrecognised units. The `cycles`
/// suffix is rejected here — clock periods must be wall-clock time.
fn time_literal_to_ps(s: &str) -> Result<i64, String> {
    let digits: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '_')
        .collect();
    let unit: String = s.chars().skip(digits.len()).collect();
    let factor: i64 = match unit.as_str() {
        "ps" => 1,
        "ns" => 1_000,
        "us" => 1_000_000,
        "ms" => 1_000_000_000,
        "s" => 1_000_000_000_000,
        other => {
            return Err(format!(
                "unsupported time unit `{other}` in `{s}` (expected ps/ns/us/ms/s)"
            ));
        }
    };
    // i64 picoseconds caps each unit at i64::MAX / factor (e.g. 9_223_372 s).
    let overflow = || {
        format!(
            "time literal `{s}` overflows the picosecond range (max {}{unit})",
            i64::MAX / factor
        )
    };
    let n: i64 = digits.replace('_', "").parse().map_err(|e| {
        if matches!(
            std::num::ParseIntError::kind(&e),
            std::num::IntErrorKind::PosOverflow
        ) {
            overflow()
        } else {
            format!("bad number in time literal `{s}`")
        }
    })?;
    n.checked_mul(factor).ok_or_else(overflow)
}

fn escape_c(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

fn guarded_constraint_guard(expr: &Expr) -> Option<&Expr> {
    let ExprKind::Binary {
        op: BinaryOp::OrOr,
        lhs,
        ..
    } = &*expr.kind
    else {
        return None;
    };
    let ExprKind::Unary {
        op: UnaryOp::Not,
        expr,
    } = &*lhs.kind
    else {
        return None;
    };
    match &*expr.kind {
        ExprKind::Paren(inner) => Some(inner),
        _ => Some(expr),
    }
}

/// Combine a list of constraint expressions into one
/// `Binary(AndAnd, …)` chain. Used by `expand_relation_subtree` when a
/// block-form relation appears in a position that expects a single
/// expression (e.g. nested inside another `Binary &&`). Empty input
/// returns a literal `true`. Single-element input returns the element
/// unchanged.
/// Walk `expr` and replace each `ExprKind::Ident(name)` whose name is
/// in `subst` with the corresponding substitute expression (cloned in
/// place). Used by `expand_relation_calls` to bind each relation's
/// formal parameter to the actual call argument before the body is
/// added to the Z3 solver block.
///
/// The substitute can be any expression — a bare ident (the common
/// case: `R(pkt)` → `pkt`), a field access (`R(env.agent.txn)`), or
/// a deeper subtree. Field-access names (`Field { name, .. }`) are
/// attribute references, not bindings, so they're never substituted.
fn substitute_idents(expr: &Expr, subst: &std::collections::HashMap<String, Expr>) -> Expr {
    let span = expr.span;
    let new_kind: ExprKind = match &*expr.kind {
        ExprKind::Ident(id) => {
            if let Some(replacement) = subst.get(&id.name) {
                // Splice the replacement's *kind* in at the current
                // span. Keeping the original ident's span preserves
                // source attribution in any future diagnostic.
                return Expr::new((*replacement.kind).clone(), span);
            }
            ExprKind::Ident(id.clone())
        }
        ExprKind::Field { target, name } => ExprKind::Field {
            target: substitute_idents(target, subst),
            name: name.clone(),
        },
        ExprKind::Index { target, index } => ExprKind::Index {
            target: substitute_idents(target, subst),
            index: substitute_idents(index, subst),
        },
        ExprKind::BitSlice { target, hi, lo } => ExprKind::BitSlice {
            target: substitute_idents(target, subst),
            hi: substitute_idents(hi, subst),
            lo: substitute_idents(lo, subst),
        },
        ExprKind::Call { callee, args } => ExprKind::Call {
            callee: substitute_idents(callee, subst),
            args: args
                .iter()
                .map(|a| match a {
                    CallArg::Expr(e) => CallArg::Expr(substitute_idents(e, subst)),
                    CallArg::Named { name, value } => CallArg::Named {
                        name: name.clone(),
                        value: substitute_idents(value, subst),
                    },
                })
                .collect(),
        },
        ExprKind::ForEachConstraint { var, iter, body } => {
            let mut scoped = subst.clone();
            scoped.remove(&var.name);
            ExprKind::ForEachConstraint {
                var: var.clone(),
                iter: substitute_idents(iter, subst),
                body: body.iter().map(|e| substitute_idents(e, &scoped)).collect(),
            }
        }
        ExprKind::Cast { expr, ty } => ExprKind::Cast {
            expr: substitute_idents(expr, subst),
            ty: ty.clone(),
        },
        ExprKind::Unary { op, expr } => ExprKind::Unary {
            op: *op,
            expr: substitute_idents(expr, subst),
        },
        ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
            op: *op,
            lhs: substitute_idents(lhs, subst),
            rhs: substitute_idents(rhs, subst),
        },
        ExprKind::Ternary {
            cond,
            then_branch,
            else_branch,
        } => ExprKind::Ternary {
            cond: substitute_idents(cond, subst),
            then_branch: substitute_idents(then_branch, subst),
            else_branch: substitute_idents(else_branch, subst),
        },
        ExprKind::Paren(inner) => ExprKind::Paren(substitute_idents(inner, subst)),
        ExprKind::Membership { expr, set } => ExprKind::Membership {
            expr: substitute_idents(expr, subst),
            set: substitute_idents(set, subst),
        },
        ExprKind::SetLit(items) => {
            ExprKind::SetLit(items.iter().map(|x| substitute_idents(x, subst)).collect())
        }
        ExprKind::RangeLit { lo, hi } => ExprKind::RangeLit {
            lo: lo.as_ref().map(|x| substitute_idents(x, subst)),
            hi: hi.as_ref().map(|x| substitute_idents(x, subst)),
        },
        // Literals, ImplicitSelf, and the spec-§5 / §6 / §8 forms
        // (HashHash, SeqRepeat, DistLit, etc.) don't bind names and
        // aren't expected inside constraint bodies; pass through
        // verbatim. If a relation body happens to contain one of these
        // shapes, the downstream constraint translator will reject it
        // with a clear "not supported" error, same as it would for an
        // un-inlined body.
        other => other.clone(),
    };
    Expr::new(new_kind, span)
}

#[cfg(test)]
mod bus_gate_tests {
    use super::{bus_param_env, gate_passes};
    use crate::ast::{Item, TypeExpr};
    use crate::parser::parse_source;

    fn bus_of(src: &str) -> crate::ast::BusDecl {
        let f = parse_source(src).expect("parse");
        match f.items.into_iter().next().expect("one item") {
            Item::Bus(b) => b,
            other => panic!("expected bus, got {other:?}"),
        }
    }

    fn bind_ty(src: &str) -> TypeExpr {
        // Parse a component field `x : <Type>` so the generic args land in a
        // TypeExpr::Named we can hand to bus_param_env (mirrors the bind-site
        // type the real codegen reads off a `let`/field). `testbench` parses to
        // Item::Env (a ComponentDecl); its fields are ComponentItem::Field.
        let full = format!("testbench Tb\n  x : {src}\nend testbench Tb");
        let f = parse_source(&full).expect("parse tb");
        for it in &f.items {
            if let Item::Env(c) = it {
                for ci in &c.items {
                    if let crate::ast::ComponentItem::Field(fd) = ci {
                        return fd.ty.clone();
                    }
                }
            }
        }
        panic!("no field type found");
    }

    const GATED_BUS: &str = r#"bus BusAxiG
  param ADDR_W: const = 32;
  param READ: const = 1;
  param WRITE: const = 1;
  generate_if READ
    ar_valid: out Bool;
    r_valid:  in  Bool;
  end generate_if
  generate_if WRITE
    aw_valid: out Bool;
  end generate_if
  hready: in Bool;
end bus BusAxiG"#;

    fn gate_for(bus: &crate::ast::BusDecl, sig: &str) -> Option<crate::ast::Expr> {
        bus.signals
            .iter()
            .find(|s| s.name.name == sig)
            .and_then(|s| s.gate.clone())
    }

    #[test]
    fn defaults_keep_both_channels_present() {
        let bus = bus_of(GATED_BUS);
        let env = bus_param_env(&bus, None);
        assert_eq!(env.get("READ"), Some(&1));
        assert_eq!(env.get("WRITE"), Some(&1));
        // With READ=WRITE=1 defaults, every gated signal is present.
        assert!(gate_passes(gate_for(&bus, "ar_valid").as_ref(), &env));
        assert!(gate_passes(gate_for(&bus, "aw_valid").as_ref(), &env));
        // Ungated signal is always present.
        assert!(gate_passes(gate_for(&bus, "hready").as_ref(), &env));
    }

    #[test]
    fn read_zero_override_drops_read_channel() {
        let bus = bus_of(GATED_BUS);
        let ty = bind_ty("BusAxiG#(READ=0)");
        let env = bus_param_env(&bus, Some(&ty));
        assert_eq!(env.get("READ"), Some(&0));
        // READ=0 ⇒ the AR/R channel is gated OFF (absent), matching arch build.
        assert!(!gate_passes(gate_for(&bus, "ar_valid").as_ref(), &env));
        assert!(!gate_passes(gate_for(&bus, "r_valid").as_ref(), &env));
        // WRITE still defaults to 1 ⇒ AW channel present.
        assert!(gate_passes(gate_for(&bus, "aw_valid").as_ref(), &env));
        // Ungated signal unaffected.
        assert!(gate_passes(gate_for(&bus, "hready").as_ref(), &env));
    }

    #[test]
    fn write_zero_override_drops_write_channel() {
        let bus = bus_of(GATED_BUS);
        let ty = bind_ty("BusAxiG#(WRITE=0)");
        let env = bus_param_env(&bus, Some(&ty));
        assert!(!gate_passes(gate_for(&bus, "aw_valid").as_ref(), &env));
        assert!(gate_passes(gate_for(&bus, "ar_valid").as_ref(), &env));
    }

    #[test]
    fn default_off_param_drops_channel_with_no_override() {
        // A bus whose default gates a channel OFF must model it absent even
        // without any bind-site override.
        let bus = bus_of(
            r#"bus B
  param EN: const = 0;
  generate_if EN
    x: out Bool;
  end generate_if
  y: in Bool;
end bus B"#,
        );
        let env = bus_param_env(&bus, None);
        assert!(!gate_passes(gate_for(&bus, "x").as_ref(), &env));
        assert!(gate_passes(gate_for(&bus, "y").as_ref(), &env));
    }

    #[test]
    fn unfoldable_gate_is_conservatively_present() {
        // A gate referencing an unknown param can't fold; we keep the signal
        // present rather than silently dropping it.
        let bus = bus_of(
            r#"bus B
  generate_if UNKNOWN
    x: out Bool;
  end generate_if
end bus B"#,
        );
        let env = bus_param_env(&bus, None); // no UNKNOWN binding
        assert!(gate_passes(gate_for(&bus, "x").as_ref(), &env));
    }

    // ── DUT-port-level override ingestion (fix/bus-port-override-from-archi) ──

    use super::{
        add_arch_scalar_bit_lanes, bus_param_env_with_port_override,
        collect_port_overrides_from_src, collect_port_widths_from_src,
        dut_bus_port_overrides_from_files, dut_port_widths_from_files,
    };

    #[test]
    fn concrete_arch_scalar_port_widths_are_ingested() {
        let archi = "module M\n  port clk: in Clock<SysDomain>;\n  port data: out UInt<1024>;\n  port flag: in Bool;\n  port symbolic: out UInt<WIDTH>;\n  port reg legacy: out UInt<8>;\n  port multiline:\n    out UInt<16>;\n  port same_a: in Bool; port same_q: out UInt<8>;\n  port bus: target SomeBus;\nend module M\n";
        let mut widths = std::collections::HashMap::new();
        collect_port_widths_from_src(archi, "M", &mut widths);
        assert_eq!(widths.get("data"), Some(&1024));
        assert_eq!(widths.get("flag"), Some(&1));
        assert_eq!(widths.get("symbolic"), Some(&u32::MAX));
        assert_eq!(widths.get("legacy"), Some(&8));
        assert_eq!(widths.get("multiline"), Some(&16));
        assert_eq!(widths.get("same_a"), Some(&1));
        assert_eq!(widths.get("same_q"), Some(&8));
        assert_eq!(widths.get("bus"), None);
    }

    #[test]
    fn arch_scalar_ports_become_one_bit_lane_accesses() {
        let archi = "module Top\n  port count_out: out UInt<8>;\n  port symbolic: out UInt<WIDTH>;\n  port lanes: out Vec<UInt<8>, 4>;\n  port bus: target SomeBus;\nend module Top\n";
        let mut scalar_widths = std::collections::HashMap::new();
        collect_port_widths_from_src(archi, "Top", &mut scalar_widths);

        let mut lanes = std::collections::HashMap::new();
        lanes.insert("already_scanned".to_string(), 8);
        add_arch_scalar_bit_lanes(&mut lanes, &scalar_widths);

        assert_eq!(lanes.get("count_out"), Some(&1));
        assert_eq!(lanes.get("symbolic"), Some(&1));
        assert_eq!(lanes.get("lanes"), None);
        assert_eq!(lanes.get("bus"), None);
        assert_eq!(lanes.get("already_scanned"), Some(&8));
    }

    #[test]
    fn arch_port_widths_are_scoped_to_the_selected_top_and_unwrap_pipe_reg() {
        let archi = "module Helper\n  port data: out UInt<8>;\nend module Helper\nmodule M\n  port data: out pipe_reg<UInt<1024>, 1> reset rst => 0;\nend module M\n";
        let mut widths = std::collections::HashMap::new();
        collect_port_widths_from_src(archi, "M", &mut widths);
        assert_eq!(widths.get("data"), Some(&1024));
    }

    #[test]
    fn archi_scalar_width_is_preferred_over_arch_source() {
        let dir =
            std::env::temp_dir().join(format!("harc_archi_port_width_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let arch = dir.join("M.arch");
        std::fs::write(
            &arch,
            "module M\n  port data: out UInt<128>;\nend module M\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("M.archi"),
            "module M\n  port data: out UInt<1024>;\nend module M\n",
        )
        .unwrap();
        let widths = dut_port_widths_from_files(&[arch], "M");
        assert_eq!(widths.get("data"), Some(&1024));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn arch_source_is_fallback_when_archi_omits_selected_top() {
        let dir = std::env::temp_dir().join(format!(
            "harc_archi_missing_top_width_test_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let arch = dir.join("M.arch");
        std::fs::write(
            &arch,
            "module Primary\n  port data: out UInt<8>;\nend module Primary\nmodule Secondary\n  port data: out UInt<1024>;\nend module Secondary\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("M.archi"),
            "module Primary\n  port data: out UInt<8>;\nend module Primary\n",
        )
        .unwrap();
        let widths = dut_port_widths_from_files(&[arch], "Secondary");
        assert_eq!(widths.get("data"), Some(&1024));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The realistic case: the DUT module's own port carries the override
    /// (`port s: target BusAxiG<WRITE=0>`, as recorded in its `.arch`/`.archi`),
    /// and the harc TB does NOT restate it at the bind site. The override must
    /// be ingested from the interface line and drop the WRITE-gated channel from
    /// the modeled port set, agreeing with `arch build`'s flatten.
    #[test]
    fn dut_port_override_from_archi_drops_gated_channel_no_tb_generic() {
        let bus = bus_of(GATED_BUS);
        // `.archi` interface line shape emitted by arch#567.
        let archi = "module RwTarget\n  port clk: in Clock<SysDomain>;\n  port s: target BusAxiG<WRITE=0>;\nend module RwTarget\n";
        let mut overrides = std::collections::HashMap::new();
        collect_port_overrides_from_src(archi, &mut overrides);
        // Override keyed by DUT port name `s`.
        let s_ov = overrides.get("s").expect("port s override ingested");
        assert_eq!(s_ov.get("WRITE"), Some(&0));

        // No harc-TB bind-site generic (bind_ty = None) — the DUT port override
        // is the ONLY non-default layer.
        let env = bus_param_env_with_port_override(&bus, None, Some(s_ov));
        assert_eq!(env.get("WRITE"), Some(&0));
        // WRITE=0 ⇒ AW channel gated OFF (absent), matching `arch build`.
        assert!(!gate_passes(gate_for(&bus, "aw_valid").as_ref(), &env));
        // READ still defaults to 1 ⇒ AR/R channel present.
        assert!(gate_passes(gate_for(&bus, "ar_valid").as_ref(), &env));
        assert!(gate_passes(gate_for(&bus, "r_valid").as_ref(), &env));
        // Ungated signal unaffected.
        assert!(gate_passes(gate_for(&bus, "hready").as_ref(), &env));
    }

    /// Precedence: when the DUT port override AND a harc-TB bind-site generic
    /// name the same param, the DUT port's override wins (it reflects what
    /// `arch build` actually synthesized).
    #[test]
    fn dut_port_override_wins_over_tb_bind_generic() {
        let bus = bus_of(GATED_BUS);
        // TB restates WRITE=1, but the DUT port was built WRITE=0.
        let tb_ty = bind_ty("BusAxiG#(WRITE=1)");
        let mut ov = std::collections::HashMap::new();
        ov.insert("WRITE".to_string(), 0i64);
        let env = bus_param_env_with_port_override(&bus, Some(&tb_ty), Some(&ov));
        assert_eq!(env.get("WRITE"), Some(&0));
        assert!(!gate_passes(gate_for(&bus, "aw_valid").as_ref(), &env));
    }

    /// `Vec<BusName<P=v>, N>` array-of-bus port form (arch#567) ingests the
    /// element bus's override.
    #[test]
    fn vec_of_bus_port_override_is_ingested() {
        let archi = "module M\n  port outs: initiator Vec<BusAxiG<READ=0>, 4>;\nend module M\n";
        let mut overrides = std::collections::HashMap::new();
        collect_port_overrides_from_src(archi, &mut overrides);
        let ov = overrides.get("outs").expect("vec-of-bus override ingested");
        assert_eq!(ov.get("READ"), Some(&0));
    }

    /// A port with no override contributes nothing (no spurious entry).
    #[test]
    fn plain_bus_port_contributes_no_override() {
        let archi = "module M\n  port s: target BusAxiG;\n  port busy: out Bool;\nend module M\n";
        let mut overrides = std::collections::HashMap::new();
        collect_port_overrides_from_src(archi, &mut overrides);
        assert!(overrides.is_empty());
    }

    /// A `.arch` source on disk (no sibling `.archi`) is scanned directly — the
    /// source port decl carries the identical override line.
    #[test]
    fn override_read_from_arch_source_on_disk() {
        let dir =
            std::env::temp_dir().join(format!("harc_archi_override_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let arch = dir.join("RwTarget.arch");
        std::fs::write(
            &arch,
            "module RwTarget\n  port clk: in Clock<SysDomain>;\n  port s: target BusAxiG<WRITE=0>;\nend module RwTarget\n",
        )
        .unwrap();
        let overrides = dut_bus_port_overrides_from_files(&[arch.clone()]);
        assert_eq!(overrides.get("s").and_then(|m| m.get("WRITE")), Some(&0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `.archi` sibling is preferred over the `.arch` source when both exist.
    #[test]
    fn archi_sibling_preferred_over_arch_source() {
        let dir = std::env::temp_dir().join(format!("harc_archi_pref_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let arch = dir.join("M.arch");
        // .arch says WRITE=1; .archi (authoritative, post-elaboration) says WRITE=0.
        std::fs::write(
            &arch,
            "module M\n  port s: target BusAxiG<WRITE=1>;\nend module M\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("M.archi"),
            "module M\n  port s: target BusAxiG<WRITE=0>;\nend module M\n",
        )
        .unwrap();
        let overrides = dut_bus_port_overrides_from_files(&[arch]);
        assert_eq!(
            overrides.get("s").and_then(|m| m.get("WRITE")),
            Some(&0),
            "the .archi sibling override should win"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod vec_lane_sv_scan_tests {
    use super::{scan_sv_module_lane_widths, scan_sv_module_ports};

    fn scan(src: &str, top: &str) -> std::collections::HashMap<String, u32> {
        scan_sv_module_lane_widths(src, top).unwrap_or_default()
    }

    #[test]
    fn packed_one_dim_lane_is_width_one() {
        let sv = "module M (\n  input logic [2:0] valid,\n);\nendmodule\n";
        let m = scan(sv, "M");
        assert_eq!(m.get("valid"), Some(&1));
    }

    #[test]
    fn packed_two_dim_lane_resolves_inner_width() {
        let sv = "module M (\n  input logic [2:0] [7:0] data,\n  input logic [3:0] [1:0] resp,\n);\nendmodule\n";
        let m = scan(sv, "M");
        assert_eq!(m.get("data"), Some(&8));
        assert_eq!(m.get("resp"), Some(&2));
    }

    #[test]
    fn lane_width_folds_against_module_params() {
        let sv = "module M #(\n  parameter int W = 5\n) (\n  input logic [2:0] [W-1:0] id,\n);\nendmodule\n";
        let m = scan(sv, "M");
        assert_eq!(m.get("id"), Some(&5));
    }

    #[test]
    fn unpacked_array_port_is_excluded() {
        // `logic [7:0] uvec [N]` — packed [7:0] before the name, UNPACKED
        // [N] after the name. This is a real SV unpacked array and must
        // NOT be recorded as a packed lane vector.
        let sv = "module M #(\n  parameter int N = 3\n) (\n  input logic [7:0] uvec [N],\n);\nendmodule\n";
        let m = scan(sv, "M");
        assert_eq!(m.get("uvec"), None);
    }

    #[test]
    fn plain_clk_rst_scalars_are_excluded() {
        let sv = "module M (\n  input logic clk,\n  input logic rst,\n);\nendmodule\n";
        let m = scan(sv, "M");
        assert!(m.is_empty());
    }

    #[test]
    fn only_named_top_module_is_scanned() {
        let sv = "module Other (\n  input logic [2:0] [7:0] data,\n);\nendmodule\nmodule M (\n  input logic [3:0] flag,\n);\nendmodule\n";
        let m = scan(sv, "M");
        assert_eq!(m.get("flag"), Some(&1));
        // `data` belongs to `Other`, not `M`.
        assert_eq!(m.get("data"), None);
    }

    #[test]
    fn total_port_scan_retains_raw_wide_width() {
        let sv = "module M (\n  input logic clk,\n  output logic [1023:0] data,\n);\nendmodule\n";
        let ports = scan_sv_module_ports(sv, "M").expect("top module scans");
        assert_eq!(
            ports
                .iter()
                .find(|port| port.name == "data")
                .map(|port| port.width_bits),
            Some(1024)
        );
    }

    #[test]
    fn one_line_ansi_header_preserves_ports_and_inherited_declarators() {
        let sv = "module M(input logic [7:0] a, b, output logic signed [8:0] sum); endmodule\n";
        let ports = scan_sv_module_ports(sv, "M").expect("one-line top module scans");
        assert_eq!(
            ports
                .iter()
                .map(|port| (
                    port.name.as_str(),
                    port.width_bits,
                    port.is_input,
                    port.is_signed,
                ))
                .collect::<Vec<_>>(),
            vec![
                ("a", 8, true, false),
                ("b", 8, true, false),
                ("sum", 9, false, true)
            ]
        );
    }
}

#[cfg(test)]
mod time_literal_tests {
    use super::time_literal_to_ps;

    /// Largest value per unit whose picosecond conversion still fits in
    /// i64 (i64::MAX / factor), and the smallest rejected (one above).
    /// Mirrors `ir::lower::time_literal_tests` — the two emitters must
    /// accept/reject identically.
    #[test]
    fn boundary_per_unit() {
        let cases = [
            ("ps", 9_223_372_036_854_775_807i64, 1i64),
            ("ns", 9_223_372_036_854_775, 1_000),
            ("us", 9_223_372_036_854, 1_000_000),
            ("ms", 9_223_372_036, 1_000_000_000),
            ("s", 9_223_372, 1_000_000_000_000),
        ];
        for (unit, max, factor) in cases {
            let ok = format!("{max}{unit}");
            assert_eq!(
                time_literal_to_ps(&ok),
                Ok(max.checked_mul(factor).unwrap()),
                "largest accepted value for {unit}"
            );
            // Smallest rejected: max+1 (for ps this overflows i64 itself,
            // so build the digit string from u64 instead).
            let over = format!("{}{unit}", (max as u64) + 1);
            let err = time_literal_to_ps(&over).expect_err("must reject");
            assert_eq!(
                err,
                format!("time literal `{over}` overflows the picosecond range (max {max}{unit})")
            );
        }
    }

    #[test]
    fn review_finding_repro_9300000s() {
        let err = time_literal_to_ps("9300000s").expect_err("must reject");
        assert_eq!(
            err,
            "time literal `9300000s` overflows the picosecond range (max 9223372s)"
        );
    }

    #[test]
    fn underscores_and_units_still_accepted() {
        assert_eq!(time_literal_to_ps("1_000ns"), Ok(1_000_000));
        assert_eq!(time_literal_to_ps("5ns"), Ok(5_000));
        assert!(time_literal_to_ps("5cycles").is_err());
    }
}

#[cfg(test)]
mod tb_lifecycle_share_tests {
    //! #619 M4a: the lifecycle-sharing desugar variant replaces each
    //! bound testbench's lifecycle body with a marker call instead of
    //! inlining it into every test, while the historical variant keeps
    //! inlining. Lowering (next slice) turns the marker into a call to a
    //! once-lowered `FunctionKind::TestbenchLifecycle` function.
    use super::{desugar_impl_for_test_in_file, desugar_impl_for_test_sharing_lifecycle};
    use crate::ast::LifecyclePhase;
    use crate::ast::{Item, SourceFile, TestItem};
    use crate::parser::parse_source;

    const SRC: &str = r#"testbench CounterLifecycleTb
    dut : Top
    expected : uint<32> default 0

    function reset()
        dut.rst = 1
        wait 1 cycle
        dut.rst = 0
    end function reset

    setup
        reset()
    end setup

    check
        assert dut.count_out == expected
    end check
end testbench CounterLifecycleTb

impl LifecycleBumpThree for CounterLifecycleTb
    run
        dut.en = 1
        wait 3 cycles
        expected = 3
    end run
end impl LifecycleBumpThree
"#;

    fn setup_debug(file: &SourceFile, test_name: &str) -> String {
        for it in &file.items {
            if let Item::Test(t) = it {
                if t.name.name == test_name {
                    for ti in &t.items {
                        if let TestItem::Scope(sc) = ti {
                            return format!("{:?}", sc.setup);
                        }
                    }
                }
            }
        }
        panic!("no scope setup for test {test_name}");
    }

    #[test]
    fn sharing_desugar_replaces_tb_setup_with_marker() {
        let file = parse_source(SRC).expect("parse");

        // Historical inlining: the tb `setup` body (a `reset()` call) is
        // spliced into the test; no marker appears.
        let normal = desugar_impl_for_test_in_file(&file);
        let normal_setup = setup_debug(&normal, "LifecycleBumpThree");
        assert!(
            !normal_setup.contains("__harc_tb_lifecycle_setup"),
            "normal desugar must not emit a lifecycle marker"
        );
        assert!(
            normal_setup.contains("reset"),
            "normal desugar inlines the tb setup body (a `reset()` call)"
        );

        // Sharing variant: the tb `setup` body is replaced by the marker
        // call; the body itself is NOT inlined into the test.
        let (shared, captured) = desugar_impl_for_test_sharing_lifecycle(&file);
        let shared_setup = setup_debug(&shared, "LifecycleBumpThree");
        assert!(
            shared_setup.contains("__harc_tb_lifecycle_setup"),
            "sharing desugar leaves a lifecycle marker call in the test setup"
        );
        assert!(
            !shared_setup.contains("reset"),
            "sharing desugar must not inline the tb setup body into the test"
        );

        // The rewritten setup body is captured ONCE for the bound
        // testbench, keyed by (testbench name, phase), for TB-IR lowering
        // to lower into a single `TestbenchLifecycle` function (#619 M4a).
        let setup_body = captured
            .get(&("CounterLifecycleTb".to_string(), LifecyclePhase::Setup))
            .expect("setup body captured for the bound testbench");
        assert!(
            format!("{setup_body:?}").contains("reset"),
            "captured setup body is the rewritten `reset()` call, ready to lower once"
        );
    }
}
