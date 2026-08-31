//! Fixed C++ scaffolding for the TB-IR backend. Every block of text
//! here mirrors the live v1 (`cpp_tb`) emission contract byte-for-byte
//! where the output is runtime-observable (trace events, log lines,
//! clock scheduling), so the v1-vs-tbir equivalence gate can diff
//! normalized semantic traces.

use crate::ir::ClockSpec;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

pub(super) const INDENT: &str = "    ";

fn source_type_names(prog: &crate::ir::TbProgram) -> HashSet<&str> {
    let mut occupied = HashSet::new();
    occupied.extend(prog.records.iter().map(|schema| schema.name.as_str()));
    occupied.extend(prog.scoreboards.iter().map(|schema| schema.name.as_str()));
    occupied.extend(prog.components.iter().map(|schema| schema.name.as_str()));
    occupied.extend(prog.covgroups.iter().map(|schema| schema.name.as_str()));
    occupied.extend(
        prog.testbench_types
            .iter()
            .map(|schema| schema.name.as_str()),
    );
    occupied.extend(prog.transactors.iter().map(|schema| schema.name.as_str()));
    occupied
}

fn common_transactor_state_struct_types(prog: &crate::ir::TbProgram) -> Vec<String> {
    let occupied = source_type_names(prog);
    let preferred = prog
        .transactors
        .iter()
        .map(unbound_state_struct_ty)
        .collect::<HashSet<_>>();
    let mut allocated = HashSet::new();
    prog.transactors
        .iter()
        .map(|schema| {
            let preferred_name = unbound_state_struct_ty(schema);
            let mut candidate = if occupied.contains(preferred_name.as_str()) {
                format!("HarcTransactorState_{}", schema.name)
            } else {
                preferred_name.clone()
            };
            let base = candidate.clone();
            let mut suffix = 1usize;
            while occupied.contains(candidate.as_str())
                || allocated.contains(&candidate)
                || (preferred.contains(&candidate) && candidate != preferred_name)
            {
                candidate = format!("{base}_{suffix}");
                suffix += 1;
            }
            allocated.insert(candidate.clone());
            candidate
        })
        .collect()
}

pub(super) fn unique_generated_type_name(prog: &crate::ir::TbProgram, base: &str) -> String {
    let occupied = source_type_names(prog);
    let transactor_state_types = common_transactor_state_struct_types(prog)
        .into_iter()
        .collect::<HashSet<_>>();
    let mut name = base.to_string();
    let mut suffix = 1usize;
    while occupied.contains(name.as_str()) || transactor_state_types.contains(&name) {
        name = format!("{base}_{suffix}");
        suffix += 1;
    }
    name
}

pub(super) fn test_hook_belongs_to_test(
    function: &crate::ir::TbFunction,
    test: &crate::ir::TestSchema,
) -> bool {
    let crate::ir::FunctionKind::TestHook { member } = &function.kind else {
        return false;
    };
    match member {
        crate::ir::TestHookMember::EventSubscription(site)
        | crate::ir::TestHookMember::MethodSubscription(site)
        | crate::ir::TestHookMember::StatementCycle(site) => site.owner.test == test.name,
        crate::ir::TestHookMember::RegblockWrite { .. }
        | crate::ir::TestHookMember::TestbenchPeriodic { .. }
        | crate::ir::TestHookMember::TestbenchCycle { .. } => {
            function.owner == Some(test.testbench)
        }
        crate::ir::TestHookMember::Pending => false,
    }
}

fn post_eval_service_body(services: &str, dut: &str) -> String {
    format!("for (auto& _svc : {services}) _svc(); if (!{services}.empty()) {dut}->eval();")
}

pub(super) fn post_eval_services(out: &mut String, depth: usize, services: &str, dut: &str) {
    let pad = INDENT.repeat(depth);
    writeln!(out, "{pad}for (auto& _svc : {services}) _svc();").ok();
    writeln!(out, "{pad}if (!{services}.empty()) {dut}->eval();").ok();
}

pub(super) fn checker_callbacks(out: &mut String, depth: usize, checkers: &str) {
    let pad = INDENT.repeat(depth);
    writeln!(out, "{pad}for (auto& _c : {checkers}) _c();").ok();
}

pub(super) fn automatic_coverage_reports(out: &mut String, depth: usize, reports: &str) {
    let pad = INDENT.repeat(depth);
    writeln!(out, "{pad}for (auto& _r : {reports}) _r();").ok();
}

pub(super) fn concurrent_coverage_reports(
    out: &mut String,
    depth: usize,
    covers: &[(crate::ir::CoverCheckId, &crate::ir::CoverCheckSchema)],
    runtime_cells: Option<super::expr::RuntimeCellRenderBinding<'_>>,
    coverage_json: &str,
) -> Result<(), super::EmitError> {
    if covers.is_empty() {
        return Ok(());
    }
    let runtime_cells = runtime_cells.ok_or_else(|| {
        super::EmitError("tbir: concurrent cover reporting has no runtime-cell binding".into())
    })?;
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    writeln!(out, "{pad}{{").ok();
    writeln!(out, "{pad1}uint64_t _cov_total = {};", covers.len()).ok();
    writeln!(out, "{pad1}uint64_t _cov_hit = 0;").ok();
    for (id, _) in covers {
        let counter = runtime_cells
            .field(&crate::ir::passes::runtime_cells::RuntimeCellKind::CoverHits { cover: *id })?;
        writeln!(out, "{pad1}if ({counter} > 0) _cov_hit++;").ok();
    }
    writeln!(
        out,
        "{pad1}harc_rt::log::harc_print_cover_summary(_cov_hit, _cov_total);"
    )
    .ok();
    writeln!(
        out,
        "{pad1}harc_rt::log::harc_cov_json_cover_summary({coverage_json}, _cov_hit, _cov_total);"
    )
    .ok();
    for (id, cover) in covers {
        let counter = runtime_cells
            .field(&crate::ir::passes::runtime_cells::RuntimeCellKind::CoverHits { cover: *id })?;
        let label = super::expr::escape_c(&cover.label);
        writeln!(
            out,
            "{pad1}harc_rt::log::harc_print_cover_point(\"{label}\", {counter});"
        )
        .ok();
        writeln!(
            out,
            "{pad1}harc_rt::log::harc_cov_json_cover_point({coverage_json}, \"{label}\", {counter});"
        )
        .ok();
    }
    writeln!(out, "{pad}}}").ok();
    Ok(())
}

pub(super) fn clear_run_callbacks(
    out: &mut String,
    depth: usize,
    reports: &str,
    post_eval_services: &str,
    checkers: &str,
) {
    let pad = INDENT.repeat(depth);
    writeln!(out, "{pad}{reports}.clear();").ok();
    writeln!(out, "{pad}{post_eval_services}.clear();").ok();
    writeln!(out, "{pad}{checkers}.clear();").ok();
}

pub(super) fn destroy_scheduler_threads(
    out: &mut String,
    depth: usize,
    schedulers: impl IntoIterator<Item = impl AsRef<str>>,
) {
    let pad = INDENT.repeat(depth);
    for scheduler in schedulers {
        writeln!(
            out,
            "{pad}harc_rt::harc_destroy_scheduler_threads({});",
            scheduler.as_ref()
        )
        .ok();
    }
}

/// File preamble: includes, trace gates, eval helpers, solver metadata.
///
/// `problem_table_cpp` is the immutable constraint-solver descriptor table
/// when the program has a cataloged randomize site, else empty. Mutable call
/// iterations live in each `HarcTestContext`. `uses_constraint_solver`
/// independently gates the Z3 runtime include because component-scope
/// randomize sites are not members of that table.
pub(super) fn preamble(
    out: &mut String,
    dut_type: &str,
    test_names: &[String],
    problem_table_cpp: &str,
    uses_constraint_solver: bool,
    has_probes: bool,
    cosim: Option<&crate::codegen::cpp_tb::CosimOpts>,
) {
    writeln!(out, "// Auto-generated by harc — do not edit.").ok();
    writeln!(out, "// Codegen: tbir (TB-IR loop-switch backend).").ok();
    writeln!(out, "// HARC test: {}", test_names.join(", ")).ok();
    out.push_str("\n#ifdef __clang__\n#pragma clang optimize off\n#endif\n\n");
    if cosim.is_none() {
        writeln!(out, "#include \"V{dut_type}.h\"").ok();
    }
    // Probe access dereferences `dut->rootp->...`; the `rootp` member is
    // only forward-declared in `V<Top>.h`, so pull in the root struct's
    // full definition. Gated on probe use (mirrors v1's
    // `aggregated_probes` include gate). See docs/probe-signals.md.
    // In co-sim mode the shim provides its own `rootp` of probe
    // accessor proxies — no Verilated root header exists to include.
    if has_probes && cosim.is_none() {
        writeln!(out, "#include \"V{dut_type}___024root.h\"").ok();
    }
    out.push_str(
        r#"#include "verilated.h"
#if VM_COVERAGE
#include "verilated_cov.h"
#endif
#if defined(HARC_TRACE_VCD)
#include "verilated_vcd_c.h"
#define HARC_TRACE_ENABLED 1
using HarcTraceC = VerilatedVcdC;
#elif defined(HARC_TRACE_FST)
#include "verilated_fst_c.h"
#define HARC_TRACE_ENABLED 1
using HarcTraceC = VerilatedFstC;
#else
#define HARC_TRACE_ENABLED 0
#endif
#include <cstdio>
#include <cstdint>
#include <cstdlib>
#include <cstdarg>
#include <cstring>
#include <string>
#include <array>
#include <vector>
#include <deque>
#include <functional>
#include <thread>
#include <atomic>
#include "harc_thread_rt.h"
#include "harc_random_rt.h"
#include "harc_queue_rt.h"
#include "harc_trace_rt.h"
#include "harc_log_rt.h"
"#,
    );
    if let Some(co) = cosim {
        out.push_str("#include \"harc_cosim_rt.h\"\n");
        cosim_dut_shim(out, dut_type, co);
    }
    // Constraint-solver runtime: the Z3 helper header + the per-program
    // problem table, emitted only when a randomize site exists (v1's
    // `uses_constraint_solver` gate). Without a site, the generated TB
    // never links Z3 — same as v1.
    if uses_constraint_solver {
        writeln!(
            out,
            "#include \"harc_z3_rt.h\"   // randomize(t) with <constraints>"
        )
        .ok();
    }
    if cosim.is_some() {
        // Bridge-aware edge helpers: same call sites as the direct
        // templates, but each edge advances real simulator time through
        // the co-sim bridge (advance to the nominal edge, write the
        // clock through the DPI setter, settle 1 ps so FF/comb updates
        // land). Because these are plain blocking calls they work from
        // ANY emission context — the drive loop, helper functions with
        // `wait`, and the synchronous `tick()` paths blocking TLM/bus
        // calls lower to.
        out.push_str(
            r#"
template<typename DUT>
static void _harc_eval_negedge(DUT* dut) {
    harc_rt::cosim::bridge().advance_to_next_edge();
    if constexpr (requires { dut->clk; }) { dut->clk = 0; }
    harc_rt::cosim::bridge().settle();
}
template<typename DUT>
static void _harc_eval_posedge(DUT* dut) {
    harc_rt::cosim::bridge().advance_to_next_edge();
    if constexpr (requires { dut->clk; }) { dut->clk = 1; }
    harc_rt::cosim::bridge().settle();
}

"#,
        );
    } else {
        out.push_str(
            r#"
template<typename DUT>
static void _harc_eval_negedge(DUT* dut) {
    if constexpr (requires { dut->clk; }) { dut->clk = 0; }
    dut->eval();
}
template<typename DUT>
static void _harc_eval_posedge(DUT* dut) {
    if constexpr (requires { dut->clk; }) { dut->clk = 1; }
    dut->eval();
}

"#,
        );
    }
    out.push_str(problem_table_cpp);
}

/// Co-sim DUT shim (`--cosim dpi`). Takes the place of the Verilated
/// `V<Top>` class in the emitted TB: same type name, same member names,
/// so every `dut-><port>` access site the direct backend emits compiles
/// unchanged — but each member is an id-keyed `SigProxy` forwarding to
/// the DPI-exported accessors of the generated `HarcCosimTop.sv`
/// harness. `eval()` maps to a 1 ps bridge settle so mid-cycle drive
/// points get a real simulator time step to re-settle combinational
/// logic.
///
/// Ports wider than 64 bits get a word-indexed `WideSigProxy` backed by
/// the word-granular accessor exports.
fn cosim_dut_shim(out: &mut String, dut_type: &str, co: &crate::codegen::cpp_tb::CosimOpts) {
    writeln!(out).ok();
    writeln!(
        out,
        "// Co-sim DUT shim for `{dut_type}` — id table mirrors the generated"
    )
    .ok();
    writeln!(out, "// HarcCosimTop.sv accessor case tables.").ok();
    writeln!(out, "struct V{dut_type} {{").ok();
    for (id, p) in co.ports.iter().enumerate() {
        if let Some(n) = p.unpacked_elems {
            // Unpacked-array port: element-indexed proxy; raw-subscript
            // access sites (`dut->p[i]`) go through the element
            // accessors.
            writeln!(
                out,
                "{INDENT}harc_rt::cosim::UnpackedSigProxy<{id}, {n}> {};",
                p.name
            )
            .ok();
            continue;
        }
        if p.width_bits > 64 {
            // Wide port: word-indexed proxy shaped like VlWide<N> (the
            // wide helpers derive the word count from sizeof).
            let words = p.width_bits.div_ceil(32);
            writeln!(
                out,
                "{INDENT}harc_rt::cosim::WideSigProxy<{id}, {words}> {};",
                p.name
            )
            .ok();
            continue;
        }
        writeln!(out, "{INDENT}harc_rt::cosim::SigProxy<{id}> {};", p.name).ok();
    }
    // Probe accessors: the emission reads probes as
    // `dut->rootp-><Verilator-mangled name>` (and writes the `_drv` /
    // `_en` siblings for force probes) — provide a `rootp` whose members
    // carry exactly those names, backed by accessor proxies keyed to the
    // harness's hierarchical references into the bound probe stub.
    if !co.probes.is_empty() {
        writeln!(out, "{INDENT}struct _HarcCosimRoot {{").ok();
        for (probe, (read_id, force_ids)) in co.probes.iter().zip(co.probe_ids()) {
            let mangled = crate::codegen::sv_stub::mangled_accessor(dut_type, &probe.name);
            writeln!(
                out,
                "{INDENT}{INDENT}harc_rt::cosim::SigProxy<{read_id}> {mangled};"
            )
            .ok();
            if let Some((drv_id, en_id)) = force_ids {
                writeln!(
                    out,
                    "{INDENT}{INDENT}harc_rt::cosim::SigProxy<{drv_id}> {mangled}_drv;"
                )
                .ok();
                writeln!(
                    out,
                    "{INDENT}{INDENT}harc_rt::cosim::SigProxy<{en_id}> {mangled}_en;"
                )
                .ok();
            }
        }
        writeln!(out, "{INDENT}}};").ok();
        writeln!(out, "{INDENT}_HarcCosimRoot _harc_root;").ok();
        writeln!(out, "{INDENT}_HarcCosimRoot* rootp = &_harc_root;").ok();
    }
    // eval() maps to a 1 ps bridge settle: the direct backend calls it
    // after mid-cycle drives (post-eval services, blocking-call loops)
    // to re-settle combinational logic — under co-sim the simulator
    // needs a real time step to do the same.
    writeln!(
        out,
        "{INDENT}void eval() {{ harc_rt::cosim::bridge().settle(); }}"
    )
    .ok();
    writeln!(out, "{INDENT}void final() {{}}").ok();
    writeln!(out, "}};").ok();
    writeln!(out).ok();
}

/// One testbench struct (only for non-synthetic testbenches).
/// `cov_fields` are (field name, covergroup struct name) pairs in
/// declaration order, emitted after the DUT pointer — same member
/// layout as v1. `scalar_fields` are run/check-shared scalar members
/// (`expected : uint<32> default 0`), with v1's C-type mapping.
/// One scoreboard declaration → C++ struct (v1's `emit_scoreboard`
/// shape): scalar counters with their declared defaults, `queue<T>`
/// fields as `harc_rt::HarcQueue<T>` members, plus the activity-tracking
/// `_last_in/out_cycle` heartbeat stamps v1 always injects (read by
/// `<env>.quiesced(N)` when the board is an env sub-component).
/// One bound-to target-TLM responder instance: a test-scope local
/// struct (state fields + activity stamps) plus its instance, mirroring
/// v1's per-instance component struct. Declared inside the test function
/// so the run/check coroutine and the actor coroutines share it by `[&]`
/// reference. Emitted at one indent level (inside the test fn body).
pub(super) fn target_state_struct_inst(
    out: &mut String,
    prog: &crate::ir::TbProgram,
    transactor: crate::ir::TransactorId,
    schema: &crate::ir::TransactorSchema,
    instance: &str,
    records: &[crate::ir::RecordSchema],
    runtime_cells: &crate::ir::passes::runtime_cells::RuntimeCellPlan,
) -> Result<(), super::EmitError> {
    require_transactor_heartbeats(transactor, schema, runtime_cells)?;
    let ty = format!("_{}_{}_state", schema.name, instance);
    writeln!(out, "{INDENT}struct {ty} {{").ok();
    emit_state_struct_body(out, prog, transactor, schema, records, runtime_cells, 2)?;
    writeln!(out, "{INDENT}}} {instance};").ok();
    Ok(())
}

/// The shared per-TYPE state struct name for the state-receiver method
/// ABI (`_<Type>_state`). One such struct type serves every unbound
/// active/passive instance of the type; each instance is its own variable
/// of this type (`emit_unbound_state_var`), and the type-shared method
/// lambda takes it by reference as `self_state` so one body drives any
/// number of instances (#494 P1b).
pub(super) fn unbound_state_struct_ty(schema: &crate::ir::TransactorSchema) -> String {
    format!("_{}_state", schema.emission_name())
}

pub(super) fn common_unbound_state_struct_ty(
    prog: &crate::ir::TbProgram,
    transactor: crate::ir::TransactorId,
) -> String {
    common_transactor_state_struct_types(prog)[transactor.index()].clone()
}

pub(super) fn unbound_state_struct_ref(
    prog: &crate::ir::TbProgram,
    transactor: crate::ir::TransactorId,
) -> String {
    format!(
        "struct {}",
        common_unbound_state_struct_ty(prog, transactor)
    )
}

/// Emit the shared per-TYPE state struct declaration for the state-receiver
/// ABI. Emitted once per transactor type that has at least one unbound
/// stateful instance in the test, before any instance variable or method
/// lambda that references it.
pub(super) fn unbound_state_struct_decl(
    out: &mut String,
    prog: &crate::ir::TbProgram,
    transactor: crate::ir::TransactorId,
    schema: &crate::ir::TransactorSchema,
    records: &[crate::ir::RecordSchema],
    runtime_cells: &crate::ir::passes::runtime_cells::RuntimeCellPlan,
) -> Result<(), super::EmitError> {
    require_transactor_heartbeats(transactor, schema, runtime_cells)?;
    let ty = common_unbound_state_struct_ty(prog, transactor);
    writeln!(out, "{INDENT}struct {ty} {{").ok();
    emit_state_struct_body(out, prog, transactor, schema, records, runtime_cells, 2)?;
    writeln!(out, "{INDENT}}};").ok();
    Ok(())
}

pub(super) fn common_transactor_state_struct_decl(
    out: &mut String,
    prog: &crate::ir::TbProgram,
    transactor: crate::ir::TransactorId,
    schema: &crate::ir::TransactorSchema,
    records: &[crate::ir::RecordSchema],
    runtime_cells: &crate::ir::passes::runtime_cells::RuntimeCellPlan,
) -> Result<(), super::EmitError> {
    require_transactor_heartbeats(transactor, schema, runtime_cells)?;
    let ty = common_unbound_state_struct_ty(prog, transactor);
    writeln!(out, "struct {ty} {{").ok();
    emit_state_struct_body(out, prog, transactor, schema, records, runtime_cells, 1)?;
    writeln!(out, "}};").ok();
    writeln!(out).ok();
    Ok(())
}

fn require_transactor_heartbeats(
    transactor: crate::ir::TransactorId,
    schema: &crate::ir::TransactorSchema,
    runtime_cells: &crate::ir::passes::runtime_cells::RuntimeCellPlan,
) -> Result<(), super::EmitError> {
    use crate::ir::passes::runtime_cells::{ComponentHeartbeat, RuntimeCellKind, RuntimeCellOwner};
    let owner = RuntimeCellOwner::TransactorInstance {
        transactor,
        name: schema.name.clone(),
    };
    for heartbeat in [ComponentHeartbeat::Input, ComponentHeartbeat::Output] {
        if runtime_cells
            .find(&owner, &RuntimeCellKind::TransactorHeartbeat(heartbeat))
            .is_none()
        {
            return Err(super::EmitError(format!(
                "tbir: transactor `{}` has no planned {heartbeat:?} heartbeat cell",
                schema.name
            )));
        }
    }
    Ok(())
}

/// Emit one storage variable of the shared per-TYPE state struct. `storage`
/// may be a generated symbol distinct from the source instance name; this
/// prevents a demand-created heartbeat object from colliding with a method
/// lambda such as `<Type>_<method>`.
pub(super) fn unbound_state_var(
    out: &mut String,
    prog: &crate::ir::TbProgram,
    transactor: crate::ir::TransactorId,
    storage: &str,
) {
    let ty = unbound_state_struct_ref(prog, transactor);
    writeln!(out, "{INDENT}{ty} {storage};").ok();
}

/// The shared field layout of a per-instance transactor-state struct:
/// declared scalar/queue state fields with their defaults, plus the two
/// auto-injected activity-tracking heartbeat stamps.
fn emit_state_struct_body(
    out: &mut String,
    prog: &crate::ir::TbProgram,
    transactor: crate::ir::TransactorId,
    schema: &crate::ir::TransactorSchema,
    records: &[crate::ir::RecordSchema],
    runtime_cells: &crate::ir::passes::runtime_cells::RuntimeCellPlan,
    depth: usize,
) -> Result<(), super::EmitError> {
    let pad = INDENT.repeat(depth);
    for f in &schema.state_fields {
        match &f.kind {
            crate::ir::StateFieldKind::Scalar { ty, default } => {
                let (cty, init) = scalar_field_decl(ty, *default);
                writeln!(out, "{pad}{cty} {} = {init};", f.name).ok();
            }
            crate::ir::StateFieldKind::Queue { elem } => {
                let elem = queue_elem_cty(elem, records);
                writeln!(out, "{pad}harc_rt::HarcQueue<{elem}> {};", f.name).ok();
            }
            // A whole value-record state member, carried by value (the
            // record struct is emitted earlier at file scope). Default-
            // constructed via the struct's own field initializers,
            // mirroring the scoreboard/component record-member shape.
            crate::ir::StateFieldKind::Record { record } => {
                let rname = &records[record.index()].name;
                writeln!(out, "{INDENT}{INDENT}{rname} {}{{}};", f.name).ok();
            }
            crate::ir::StateFieldKind::FixedVec { ty } => {
                let cty = super::aggregate_value_cty(ty, records);
                writeln!(out, "{pad}{cty} {}{{}};", f.name).ok();
            }
        }
    }
    use crate::ir::passes::runtime_cells::{
        HookOwner, RuntimeCellKind, RuntimeCellOwner, RuntimeHookSide,
    };
    let owner = RuntimeCellOwner::TransactorInstance {
        transactor,
        name: schema.name.clone(),
    };
    for cell in runtime_cells.for_owner(&owner) {
        match cell.kind() {
            RuntimeCellKind::TransactorHeartbeat(heartbeat) => {
                let field = transactor_heartbeat_field(schema, *heartbeat);
                let init = runtime_cell_initializer(cell)?;
                writeln!(out, "{pad}uint64_t {field}{init};").ok();
            }
            RuntimeCellKind::HookSubscribers {
                hook: HookOwner::Transactor { function },
                side,
            } => {
                let method = schema
                    .methods
                    .iter()
                    .find(|method| method.function == *function)
                    .ok_or_else(|| {
                        super::EmitError(format!(
                            "tbir: transactor `{}` runtime hook cell references missing fn{}",
                            schema.name, function.0
                        ))
                    })?;
                let suffix = match side {
                    RuntimeHookSide::Pre => "pre",
                    RuntimeHookSide::Post => "post",
                };
                let params = transactor_method_param_ctypes(prog, method)?.join(", ");
                let field = transactor_internal_member_name(
                    schema,
                    &format!("_harc_hook_{}_{suffix}", method.name),
                );
                let init = runtime_cell_initializer(cell)?;
                writeln!(
                    out,
                    "{pad}std::vector<std::function<void({params})>> {field}{init};"
                )
                .ok();
            }
            RuntimeCellKind::TransactorCoverageHookSubscribers { function, side } => {
                let method = schema
                    .methods
                    .iter()
                    .find(|method| method.function == *function)
                    .ok_or_else(|| {
                        super::EmitError(format!(
                            "tbir: transactor `{}` runtime coverage-hook cell references missing fn{}",
                            schema.name, function.0
                        ))
                    })?;
                let suffix = match side {
                    RuntimeHookSide::Pre => "pre",
                    RuntimeHookSide::Post => "post",
                };
                let params = transactor_method_param_ctypes(prog, method)?.join(", ");
                let field = transactor_coverage_hook_field(schema, &method.name, suffix);
                let init = runtime_cell_initializer(cell)?;
                writeln!(
                    out,
                    "{pad}std::vector<std::function<void({params})>> {field}{init};"
                )
                .ok();
            }
            other => {
                return Err(super::EmitError(format!(
                    "tbir: transactor `{}` has incompatible runtime cell {other:?}",
                    schema.name
                )));
            }
        }
    }
    Ok(())
}

fn transactor_internal_member_name(schema: &crate::ir::TransactorSchema, base: &str) -> String {
    let mut name = base.to_string();
    while schema.state_fields.iter().any(|field| field.name == name) {
        name = format!("_u_{name}");
    }
    name
}

pub(super) fn transactor_hook_field(
    schema: &crate::ir::TransactorSchema,
    method: &str,
    side: &str,
) -> String {
    transactor_internal_member_name(schema, &format!("_harc_hook_{method}_{side}"))
}

pub(super) fn transactor_coverage_hook_field(
    schema: &crate::ir::TransactorSchema,
    method: &str,
    side: &str,
) -> String {
    transactor_internal_member_name(schema, &format!("_harc_cov_{method}_{side}"))
}

pub(super) fn transactor_heartbeat_field(
    schema: &crate::ir::TransactorSchema,
    heartbeat: crate::ir::passes::runtime_cells::ComponentHeartbeat,
) -> String {
    use crate::ir::passes::runtime_cells::ComponentHeartbeat;
    transactor_internal_member_name(
        schema,
        match heartbeat {
            ComponentHeartbeat::Input => "_last_in_cycle",
            ComponentHeartbeat::Output => "_last_out_cycle",
        },
    )
}

fn transactor_method_param_ctypes(
    prog: &crate::ir::TbProgram,
    method: &crate::ir::TransactorMethodSchema,
) -> Result<Vec<String>, super::EmitError> {
    let function = prog.functions.get(method.function.index()).ok_or_else(|| {
        super::EmitError(format!(
            "tbir: transactor method `{}` references missing fn{}",
            method.name, method.function.0
        ))
    })?;
    function
        .locals
        .iter()
        .take(function.params.len())
        .map(|local| match &local.ty {
            crate::ir::IrType::Record(record) => prog
                .records
                .get(record.index())
                .map(|schema| schema.name.clone())
                .ok_or_else(|| {
                    super::EmitError(format!(
                        "tbir: transactor method `{}` references missing record r{}",
                        method.name, record.0
                    ))
                }),
            crate::ir::IrType::RecordSeq(record) => prog
                .records
                .get(record.index())
                .map(|schema| format!("std::vector<{}>", schema.name))
                .ok_or_else(|| {
                    super::EmitError(format!(
                        "tbir: transactor method `{}` references missing record r{}",
                        method.name, record.0
                    ))
                }),
            crate::ir::IrType::Seq(scalar) => {
                Ok(format!("std::vector<{}>", super::local_scalar_cty(scalar)))
            }
            ty => Ok(super::local_scalar_cty(ty)),
        })
        .collect()
}

/// C++ type and initializer for one declared scalar FIELD, shared by
/// the transactor-state, scoreboard, component, and testbench field
/// emitters. All four carried their own copy of a
/// `(bool | int64_t | uint64_t)` choice, which silently narrowed any
/// field wider than 64 bits to a 64-bit member; `field_scalar_cty` is
/// the same seam the queue-element emitter already uses, and it knows
/// about `_harc_u128` and `harc_rt::HarcWide<N>`.
fn scalar_field_decl(ty: &crate::ir::IrType, default: u64) -> (String, String) {
    match ty {
        crate::ir::IrType::Bool => (
            "bool".to_string(),
            if default != 0 { "true" } else { "false" }.to_string(),
        ),
        _ => (super::field_scalar_cty(ty), default.to_string()),
    }
}

pub(super) fn runtime_cell_initializer(
    cell: &crate::ir::passes::runtime_cells::RuntimeCell,
) -> Result<&'static str, super::EmitError> {
    use crate::ir::passes::runtime_cells::RuntimeCellInitializer;
    match cell.initializer() {
        RuntimeCellInitializer::DefaultConstructed => Ok("{}"),
        RuntimeCellInitializer::Empty => Ok(""),
        RuntimeCellInitializer::Zero => Ok(" = 0"),
        RuntimeCellInitializer::False => Ok(" = false"),
        RuntimeCellInitializer::SeedFromEnvironment => Err(super::EmitError(format!(
            "tbir: runtime cell `{}` requires runtime seed initialization, not aggregate storage",
            cell.symbol()
        ))),
    }
}

/// The C++ element type for a `queue<T>` field. Scalars reuse the exact
/// width-aware local mapping; a value-record element is the record struct
/// (carried by value, matching v1's `HarcQueue<Rec>`).
fn queue_elem_cty(elem: &crate::ir::QueueElem, records: &[crate::ir::RecordSchema]) -> String {
    match elem {
        crate::ir::QueueElem::Scalar { ty } => super::field_scalar_cty(ty),
        crate::ir::QueueElem::FixedVec { elem, len } => super::aggregate_value_cty(
            &crate::ir::IrType::FixedVec {
                elem: elem.clone(),
                len: *len,
            },
            records,
        ),
        crate::ir::QueueElem::List { elem } => {
            let elem = match elem.as_ref() {
                crate::ir::IrType::Record(record) => records[record.index()].name.clone(),
                scalar => super::field_scalar_cty(scalar),
            };
            format!("std::vector<{elem}>")
        }
        crate::ir::QueueElem::Record(r) => records[r.index()].name.clone(),
    }
}

pub(super) fn scoreboard_struct(
    out: &mut String,
    scoreboard: crate::ir::ScoreboardId,
    sb: &crate::ir::ScoreboardSchema,
    records: &[crate::ir::RecordSchema],
    runtime_cells: &crate::ir::passes::runtime_cells::RuntimeCellPlan,
) -> Result<(), super::EmitError> {
    use crate::ir::passes::runtime_cells::{ComponentHeartbeat, RuntimeCellKind, RuntimeCellOwner};
    let owner = RuntimeCellOwner::ScoreboardInstance {
        scoreboard,
        name: sb.name.clone(),
    };
    let heartbeat_cells = [ComponentHeartbeat::Input, ComponentHeartbeat::Output]
        .into_iter()
        .map(|heartbeat| {
            runtime_cells
                .find(&owner, &RuntimeCellKind::ScoreboardHeartbeat(heartbeat))
                .map(|cell| (heartbeat, cell))
                .ok_or_else(|| {
                    super::EmitError(format!(
                        "tbir: scoreboard `{}` has no planned {heartbeat:?} heartbeat cell",
                        sb.name
                    ))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    writeln!(out, "struct {} {{", sb.name).ok();
    for f in &sb.fields {
        match &f.kind {
            crate::ir::ScoreboardFieldKind::Scalar { ty, default } => {
                // `main`'s `ScoreboardScalarDefault` carries a WIDE
                // literal, which the `u64` slot this branch documented
                // as a limitation cannot. Its representation wins here;
                // `scalar_field_decl` still serves the three emitters
                // whose schema default is still a `u64`.
                let init = match default {
                    crate::ir::ScoreboardScalarDefault::Narrow(value) => match ty {
                        crate::ir::IrType::Bool => {
                            if *value != 0 { "true" } else { "false" }.to_string()
                        }
                        _ => value.to_string(),
                    },
                    crate::ir::ScoreboardScalarDefault::Wide(words) => {
                        super::expr::wide_literal_cpp(words)
                    }
                };
                let cty = super::field_scalar_cty(ty);
                writeln!(out, "{INDENT}{cty} {} = {init};", f.name).ok();
            }
            crate::ir::ScoreboardFieldKind::Record { record } => {
                let cty = &records[record.index()].name;
                writeln!(out, "{INDENT}{cty} {}{{}};", f.name).ok();
            }
            crate::ir::ScoreboardFieldKind::List { elem, vec_len } => {
                let elem = super::field_scalar_cty(elem);
                let elem = match vec_len {
                    Some(len) => format!("std::array<{elem}, {len}>"),
                    None => elem,
                };
                writeln!(out, "{INDENT}std::vector<{elem}> {}{{}};", f.name).ok();
            }
            crate::ir::ScoreboardFieldKind::Queue { elem } => {
                let elem = queue_elem_cty(elem, records);
                writeln!(out, "{INDENT}harc_rt::HarcQueue<{elem}> {};", f.name).ok();
            }
        }
    }
    // Auto-injected activity-tracking stamps, matching v1's
    // `emit_scoreboard`. A data-only scoreboard counts as a component for
    // heartbeat purposes, so `<env>.quiesced(N)` (which expands to
    // `sb.idle(N)`) reads these stamps on a scoreboard sub-component.
    for (heartbeat, cell) in heartbeat_cells {
        let field = scoreboard_heartbeat_field(sb, heartbeat);
        let init = runtime_cell_initializer(cell)?;
        writeln!(out, "{INDENT}uint64_t {field}{init};").ok();
    }
    let copy_method = scoreboard_copy_method_name(sb);
    writeln!(out, "{INDENT}{}() = default;", sb.name).ok();
    writeln!(out, "{INDENT}{}(const {}& source) {{", sb.name, sb.name).ok();
    writeln!(out, "{INDENT}{INDENT}{copy_method}(source);").ok();
    writeln!(out, "{INDENT}}}").ok();
    writeln!(
        out,
        "{INDENT}{}& operator=(const {}& source) {{",
        sb.name, sb.name
    )
    .ok();
    writeln!(
        out,
        "{INDENT}{INDENT}if (this != &source) {copy_method}(source);"
    )
    .ok();
    writeln!(out, "{INDENT}{INDENT}return *this;").ok();
    writeln!(out, "{INDENT}}}").ok();
    writeln!(
        out,
        "{INDENT}void {copy_method}(const {}& source) {{",
        sb.name
    )
    .ok();
    for field in &sb.fields {
        writeln!(
            out,
            "{INDENT}{INDENT}this->{} = source.{};",
            field.name, field.name
        )
        .ok();
    }
    writeln!(out, "{INDENT}}}").ok();
    writeln!(out, "}};").ok();
    writeln!(out).ok();
    Ok(())
}

fn scoreboard_copy_method_name(scoreboard: &crate::ir::ScoreboardSchema) -> String {
    scoreboard_internal_member_name(scoreboard, "_harc_copy_user_state_from")
}

fn scoreboard_internal_member_name(scoreboard: &crate::ir::ScoreboardSchema, base: &str) -> String {
    let mut name = base.to_string();
    while scoreboard.fields.iter().any(|field| field.name == name) {
        name = format!("_u_{name}");
    }
    name
}

pub(super) fn scoreboard_heartbeat_field(
    scoreboard: &crate::ir::ScoreboardSchema,
    heartbeat: crate::ir::passes::runtime_cells::ComponentHeartbeat,
) -> String {
    use crate::ir::passes::runtime_cells::ComponentHeartbeat;
    scoreboard_internal_member_name(
        scoreboard,
        match heartbeat {
            ComponentHeartbeat::Input => "_last_in_cycle",
            ComponentHeartbeat::Output => "_last_out_cycle",
        },
    )
}

/// The C++ payload type carried by an `event<T>` subscriber closure.
/// Mirrors v1's `payload_type_for_arg`: integer scalars use their
/// width-aware carrier, `bool` remains `bool`, and a value-record payload
/// is the record struct (carried by value).
pub(super) fn event_payload_cty(
    p: &crate::ir::EventPayload,
    records: &[crate::ir::RecordSchema],
) -> String {
    match p {
        // The subscriber's parameter type. `field_scalar_cty` carries
        // the declared width; the pair this replaced could only say
        // 64 bits, so an `event<uint<1024>>` had nowhere to put the
        // other fifteen sixteenths.
        crate::ir::EventPayload::Scalar { .. } => {
            super::field_scalar_cty(&p.scalar_ir_type().expect("a scalar payload types"))
        }
        crate::ir::EventPayload::Record(r) => records[r.index()].name.clone(),
        crate::ir::EventPayload::FixedVec { .. } => {
            super::aggregate_value_cty(&p.value_ir_type(), records)
        }
    }
}

/// One composite-component struct (env/agent cluster, flat-struct
/// subset). Mirrors v1's `emit_component_struct`: scalar/queue/event/
/// sub-component fields plus the `_last_in_cycle`/`_last_out_cycle`
/// heartbeat stamps. Sub-component fields are emitted by their component
/// name, so file order (subs before the env that holds them) guarantees
/// the held type is already defined.
pub(super) fn component_struct(
    out: &mut String,
    prog: &crate::ir::TbProgram,
    component: crate::ir::ComponentId,
    c: &crate::ir::ComponentSchema,
    components: &[crate::ir::ComponentSchema],
    scoreboards: &[crate::ir::ScoreboardSchema],
    records: &[crate::ir::RecordSchema],
    runtime_cells: &crate::ir::passes::runtime_cells::RuntimeCellPlan,
) -> Result<(), super::EmitError> {
    use crate::ir::ComponentFieldKind;
    writeln!(out, "struct {} {{", c.name).ok();
    if c.kind == crate::ir::ComponentKindTag::Sequencer {
        let unique_registry = crate::codegen::cpp_tb::component_unique_registry_name(
            c.fields.iter().map(|field| field.name.as_str()),
        );
        writeln!(
            out,
            "{INDENT}harc_rt::random::HarcUniqueRegistry {unique_registry};"
        )
        .ok();
    }
    let runtime_owner = crate::ir::passes::runtime_cells::RuntimeCellOwner::ComponentInstance {
        component,
        name: c.name.clone(),
    };
    for (field_index, f) in c.fields.iter().enumerate() {
        match &f.kind {
            ComponentFieldKind::Scalar { ty, default } => {
                let (cty, init) = scalar_field_decl(ty, *default);
                writeln!(out, "{INDENT}{cty} {} = {init};", f.name).ok();
            }
            ComponentFieldKind::FixedVec(vec) => {
                // The fifth copy of the `(bool | int64_t | uint64_t)`
                // triple, and the last one. It would have rendered a
                // `Vec<uint<1024>, 4>` as `std::array<uint64_t, 4>` —
                // an array that compiles, runs, and keeps a
                // sixteenth of each element. v1 emits
                // `std::array<harc_rt::HarcWide<32>, 4>`, and
                // `field_scalar_cty` is the seam that says so.
                let cty = super::aggregate_value_cty(&vec.elem, records);
                writeln!(
                    out,
                    "{INDENT}std::array<{cty}, {}> {}{{}};",
                    vec.len, f.name
                )
                .ok();
            }
            ComponentFieldKind::Record { record } => {
                let rname = &records[record.index()].name;
                writeln!(out, "{INDENT}{rname} {}{{}};", f.name).ok();
            }
            ComponentFieldKind::Queue { elem } => {
                let elem = queue_elem_cty(elem, records);
                writeln!(out, "{INDENT}harc_rt::HarcQueue<{elem}> {};", f.name).ok();
            }
            ComponentFieldKind::Event { payload } => {
                let kind =
                    crate::ir::passes::runtime_cells::RuntimeCellKind::ComponentEventSubscribers {
                        field: field_index as u32,
                    };
                let cell = runtime_cells.find(&runtime_owner, &kind).ok_or_else(|| {
                    super::EmitError(format!(
                        "tbir: component `{}` event field `{}` has no runtime-cell owner",
                        c.name, f.name
                    ))
                })?;
                let pty = event_payload_cty(payload, records);
                let init = runtime_cell_initializer(cell)?;
                writeln!(
                    out,
                    "{INDENT}std::vector<std::function<void({pty})>> {}{init};",
                    f.name,
                )
                .ok();
            }
            ComponentFieldKind::Sub { component, .. } => {
                let cname = &components[component.index()].name;
                writeln!(out, "{INDENT}{cname} {};", f.name).ok();
            }
            ComponentFieldKind::Dut { dut_type } => {
                // The DUT handle on an event-driven transactor: a Verilator
                // instance pointer the test binds (`drv.dut = dut`) and the
                // `on <ev>` handler pokes through. Matches v1's
                // `V<dut_type>* dut = nullptr;`.
                writeln!(out, "{INDENT}V{dut_type}* {} = nullptr;", f.name).ok();
            }
            ComponentFieldKind::ScoreboardSub { scoreboard } => {
                let sname = &scoreboards[scoreboard.index()].name;
                writeln!(out, "{INDENT}{sname} {};", f.name).ok();
            }
        }
    }
    for cell in runtime_cells.for_owner(&runtime_owner) {
        use crate::ir::passes::runtime_cells::{HookOwner, RuntimeCellKind, RuntimeHookSide};
        match cell.kind() {
            RuntimeCellKind::HookSubscribers {
                hook:
                    HookOwner::Component {
                        component: hook_component,
                        member,
                    },
                side,
            } if *hook_component == component => {
                let method = c.methods.get(member.index()).ok_or_else(|| {
                    super::EmitError(format!(
                        "tbir: runtime hook cell {:?} is not an ordinary method on component `{}`",
                        cell.kind(),
                        c.name
                    ))
                })?;
                let suffix = match side {
                    RuntimeHookSide::Pre => "pre",
                    RuntimeHookSide::Post => "post",
                };
                let arg_csv = component_method_param_ctypes(prog, method).join(", ");
                let field = component_internal_member_name(
                    c,
                    &format!("_harc_hook_{}_{suffix}", method.name),
                );
                let init = runtime_cell_initializer(cell)?;
                writeln!(
                    out,
                    "{INDENT}std::vector<std::function<void({arg_csv})>> {field}{init};"
                )
                .ok();
            }
            RuntimeCellKind::ComponentCoverageHookSubscribers {
                component: hook_component,
                member,
                side,
            } if *hook_component == component => {
                let method = c.methods.get(member.index()).ok_or_else(|| {
                    super::EmitError(format!(
                        "tbir: runtime coverage-hook cell {:?} is not an ordinary method on component `{}`",
                        cell.kind(),
                        c.name
                    ))
                })?;
                let suffix = match side {
                    RuntimeHookSide::Pre => "pre",
                    RuntimeHookSide::Post => "post",
                };
                let arg_csv = component_method_param_ctypes(prog, method).join(", ");
                let init = runtime_cell_initializer(cell)?;
                writeln!(
                    out,
                    "{INDENT}std::vector<std::function<void({arg_csv})>> _harc_cov_{}_{suffix}{init};",
                    method.name
                )
                .ok();
            }
            RuntimeCellKind::ComponentPeriodicLast { member } => {
                component_periodic_handler(c, *member).ok_or_else(|| {
                    super::EmitError(format!(
                        "tbir: runtime periodic cell {:?} has no matching handler on component `{}`",
                        cell.kind(), c.name
                    ))
                })?;
                let field = component_internal_member_name(c, &format!("_harc_{}", cell.symbol()));
                let init = runtime_cell_initializer(cell)?;
                writeln!(out, "{INDENT}int64_t {field}{init};").ok();
            }
            RuntimeCellKind::ComponentEdgePrevious { member } => {
                component_cycle_handler(c, *member).ok_or_else(|| {
                    super::EmitError(format!(
                        "tbir: runtime edge cell {:?} has no matching handler on component `{}`",
                        cell.kind(),
                        c.name
                    ))
                })?;
                let field = component_internal_member_name(c, &format!("_harc_{}", cell.symbol()));
                let init = runtime_cell_initializer(cell)?;
                writeln!(out, "{INDENT}bool {field}{init};").ok();
            }
            RuntimeCellKind::ComponentCooldown { member } => {
                component_cycle_handler(c, *member).ok_or_else(|| {
                    super::EmitError(format!(
                        "tbir: runtime cooldown cell {:?} has no matching handler on component `{}`",
                        cell.kind(), c.name
                    ))
                })?;
                let field = component_internal_member_name(c, &format!("_harc_{}", cell.symbol()));
                let init = runtime_cell_initializer(cell)?;
                writeln!(out, "{INDENT}bool {field}{init};").ok();
            }
            RuntimeCellKind::ComponentWatchdogLast { member } => {
                component_watchdog(c, *member).ok_or_else(|| {
                    super::EmitError(format!(
                        "tbir: runtime watchdog cell {:?} has no matching handler on component `{}`",
                        cell.kind(), c.name
                    ))
                })?;
                let field = component_internal_member_name(c, &format!("_harc_{}", cell.symbol()));
                let init = runtime_cell_initializer(cell)?;
                writeln!(out, "{INDENT}int64_t {field}{init};").ok();
            }
            RuntimeCellKind::ComponentHeartbeat(heartbeat) => {
                let field = component_heartbeat_field(c, *heartbeat);
                let init = runtime_cell_initializer(cell)?;
                writeln!(out, "{INDENT}uint64_t {field}{init};").ok();
            }
            RuntimeCellKind::ComponentEventSubscribers { field } => {
                let schema = c.fields.get(*field as usize).ok_or_else(|| {
                    super::EmitError(format!(
                        "tbir: runtime event cell {:?} references missing field on component `{}`",
                        cell.kind(),
                        c.name
                    ))
                })?;
                if !matches!(schema.kind, ComponentFieldKind::Event { .. }) {
                    return Err(super::EmitError(format!(
                        "tbir: runtime event cell {:?} references non-event field `{}` on component `{}`",
                        cell.kind(), schema.name, c.name
                    )));
                }
            }
            other => {
                return Err(super::EmitError(format!(
                    "tbir: component `{}` has incompatible runtime cell {other:?}",
                    c.name
                )));
            }
        }
    }
    let copy_method = component_copy_method_name(c);
    writeln!(
        out,
        "{INDENT}void {copy_method}(const {}& source) {{",
        c.name
    )
    .ok();
    for field in &c.fields {
        match &field.kind {
            ComponentFieldKind::Event { .. } | ComponentFieldKind::Dut { .. } => {}
            ComponentFieldKind::Sub { component, .. } => {
                let nested = components.get(component.index()).ok_or_else(|| {
                    super::EmitError(format!(
                        "tbir: component `{}` field `{}` references missing component c{}",
                        c.name, field.name, component.0
                    ))
                })?;
                let nested_copy = component_copy_method_name(nested);
                writeln!(
                    out,
                    "{INDENT}{INDENT}this->{}.{nested_copy}(source.{});",
                    field.name, field.name
                )
                .ok();
            }
            ComponentFieldKind::ScoreboardSub { scoreboard } => {
                let scoreboard = scoreboards.get(scoreboard.index()).ok_or_else(|| {
                    super::EmitError(format!(
                        "tbir: component `{}` field `{}` references missing scoreboard",
                        c.name, field.name
                    ))
                })?;
                let scoreboard_copy = scoreboard_copy_method_name(scoreboard);
                writeln!(
                    out,
                    "{INDENT}{INDENT}this->{}.{scoreboard_copy}(source.{});",
                    field.name, field.name
                )
                .ok();
            }
            _ => {
                writeln!(
                    out,
                    "{INDENT}{INDENT}this->{} = source.{};",
                    field.name, field.name
                )
                .ok();
            }
        }
    }
    writeln!(out, "{INDENT}}}").ok();
    writeln!(out, "{INDENT}{}() = default;", c.name).ok();
    writeln!(out, "{INDENT}{}(const {}& source) {{", c.name, c.name).ok();
    writeln!(out, "{INDENT}{INDENT}{copy_method}(source);").ok();
    writeln!(out, "{INDENT}}}").ok();
    writeln!(
        out,
        "{INDENT}{}& operator=(const {}& source) {{",
        c.name, c.name
    )
    .ok();
    writeln!(
        out,
        "{INDENT}{INDENT}if (this != &source) {copy_method}(source);"
    )
    .ok();
    writeln!(out, "{INDENT}{INDENT}return *this;").ok();
    writeln!(out, "{INDENT}}}").ok();
    writeln!(out, "}};").ok();
    writeln!(out).ok();
    Ok(())
}

pub(super) fn component_internal_member_name(
    component: &crate::ir::ComponentSchema,
    base: &str,
) -> String {
    let mut name = base.to_string();
    while component.fields.iter().any(|field| field.name == name) {
        name = format!("_u_{name}");
    }
    name
}

pub(super) fn component_runtime_cell_field(
    plan: &crate::ir::passes::runtime_cells::RuntimeCellPlan,
    component: crate::ir::ComponentId,
    schema: &crate::ir::ComponentSchema,
    kind: &crate::ir::passes::runtime_cells::RuntimeCellKind,
) -> Result<String, super::EmitError> {
    let owner = crate::ir::passes::runtime_cells::RuntimeCellOwner::ComponentInstance {
        component,
        name: schema.name.clone(),
    };
    let cell = plan.find(&owner, kind).ok_or_else(|| {
        super::EmitError(format!(
            "tbir: component `{}` has no planned runtime cell {kind:?}",
            schema.name
        ))
    })?;
    Ok(component_internal_member_name(
        schema,
        &format!("_harc_{}", cell.symbol()),
    ))
}

pub(super) fn component_copy_method_name(component: &crate::ir::ComponentSchema) -> String {
    component_internal_member_name(component, "_harc_copy_user_state_from")
}

pub(super) fn component_heartbeat_field(
    component: &crate::ir::ComponentSchema,
    heartbeat: crate::ir::passes::runtime_cells::ComponentHeartbeat,
) -> String {
    use crate::ir::passes::runtime_cells::ComponentHeartbeat;
    component_internal_member_name(
        component,
        match heartbeat {
            ComponentHeartbeat::Input => "_last_in_cycle",
            ComponentHeartbeat::Output => "_last_out_cycle",
        },
    )
}

fn component_periodic_handler(
    component: &crate::ir::ComponentSchema,
    member: crate::ir::ComponentCallableId,
) -> Option<&crate::ir::PeriodicHandlerSchema> {
    let base = component.methods.len() + component.on_handlers.len();
    member
        .index()
        .checked_sub(base)
        .and_then(|index| component.periodic_handlers.get(index))
}

fn component_cycle_handler(
    component: &crate::ir::ComponentSchema,
    member: crate::ir::ComponentCallableId,
) -> Option<&crate::ir::CycleTriggerHandlerSchema> {
    let base =
        component.methods.len() + component.on_handlers.len() + component.periodic_handlers.len();
    member
        .index()
        .checked_sub(base)
        .and_then(|index| component.cycle_handlers.get(index))
}

fn component_watchdog(
    component: &crate::ir::ComponentSchema,
    member: crate::ir::ComponentCallableId,
) -> Option<&crate::ir::WatchdogSchema> {
    let index = component.methods.len()
        + component.on_handlers.len()
        + component.periodic_handlers.len()
        + component.cycle_handlers.len();
    (member.index() == index)
        .then_some(component.watchdog.as_ref())
        .flatten()
}

fn statement_cell_function<'a>(
    prog: &'a crate::ir::TbProgram,
    test: crate::ir::TestId,
    kind: &crate::ir::passes::runtime_cells::RuntimeCellKind,
) -> Result<&'a crate::ir::TbFunction, super::EmitError> {
    use crate::ir::passes::runtime_cells::{RuntimeCellKind, TemporalCheck};
    use crate::ir::{FunctionKind, Stmt};

    let mut found = None;
    for function in &prog.functions {
        if !matches!(function.kind, FunctionKind::TestBody { test: owner, .. } if owner == test) {
            continue;
        }
        let owns = function
            .blocks
            .iter()
            .flat_map(|block| &block.stmts)
            .any(|stmt| match (kind, stmt) {
                (
                    RuntimeCellKind::TemporalPrevious {
                        check: TemporalCheck::Property(cell),
                        ..
                    }
                    | RuntimeCellKind::PropertyImplicationPrevious { property: cell },
                    Stmt::PropertyCheck(stmt),
                ) => cell == stmt,
                (
                    RuntimeCellKind::TemporalPrevious {
                        check: TemporalCheck::Cover(cell),
                        ..
                    }
                    | RuntimeCellKind::CoverHits { cover: cell },
                    Stmt::CoverCheck(stmt),
                ) => cell == stmt,
                (
                    RuntimeCellKind::StatementPeriodicLast { handler: cell }
                    | RuntimeCellKind::StatementEdgePrevious { handler: cell },
                    Stmt::CycleHandler(stmt),
                ) => cell == stmt,
                _ => false,
            });
        if owns && found.replace(function).is_some() {
            return Err(super::EmitError(format!(
                "tbir: runtime cell {kind:?} for test t{} has more than one registration function",
                test.0
            )));
        }
    }
    found.ok_or_else(|| {
        super::EmitError(format!(
            "tbir: runtime cell {kind:?} for test t{} has no registration function",
            test.0
        ))
    })
}

fn has_capsule_storage(kind: &crate::ir::passes::runtime_cells::RuntimeCellKind) -> bool {
    use crate::ir::passes::runtime_cells::RuntimeCellKind;
    matches!(
        kind,
        RuntimeCellKind::TemporalPrevious { .. }
            | RuntimeCellKind::PropertyImplicationPrevious { .. }
            | RuntimeCellKind::CoverHits { .. }
            | RuntimeCellKind::StatementPeriodicLast { .. }
            | RuntimeCellKind::StatementEdgePrevious { .. }
            | RuntimeCellKind::TestbenchPeriodicLast { .. }
            | RuntimeCellKind::TestbenchEdgePrevious { .. }
            | RuntimeCellKind::ConstraintState { .. }
            | RuntimeCellKind::LocalEventSubscribers { .. }
            | RuntimeCellKind::PersistentLocal { .. }
            | RuntimeCellKind::TestHookClosure { .. }
    )
}

fn persistent_local_cty(
    prog: &crate::ir::TbProgram,
    ty: &crate::ir::IrType,
) -> Result<String, super::EmitError> {
    use crate::ir::IrType;
    match ty {
        IrType::Record(record) => prog
            .records
            .get(record.index())
            .map(|schema| schema.name.clone())
            .ok_or_else(|| {
                super::EmitError(format!(
                    "tbir: persistent local references missing record r{}",
                    record.0
                ))
            }),
        IrType::RecordSeq(record) => prog
            .records
            .get(record.index())
            .map(|schema| format!("std::vector<{}>", schema.name))
            .ok_or_else(|| {
                super::EmitError(format!(
                    "tbir: persistent sequence local references missing record r{}",
                    record.0
                ))
            }),
        IrType::Seq(elem) => Ok(format!(
            "std::vector<{}>",
            persistent_local_cty(prog, elem)?
        )),
        IrType::FixedVec { elem, len } => Ok(format!(
            "std::array<{}, {len}>",
            persistent_local_cty(prog, elem)?
        )),
        IrType::Component(component) => prog
            .components
            .get(component.index())
            .map(|schema| schema.name.clone())
            .ok_or_else(|| {
                super::EmitError(format!(
                    "tbir: persistent local references missing component c{}",
                    component.0
                ))
            }),
        IrType::Event(payload) => Ok(format!(
            "std::vector<std::function<void({})>>",
            event_payload_cty(payload, &prog.records)
        )),
        IrType::PortSnapshot => Err(super::EmitError(
            "tbir: a callback cannot retain an internal DUT port snapshot local".into(),
        )),
        other => Ok(super::local_scalar_cty(other)),
    }
}

pub(super) fn test_hook_capture_count(
    prog: &crate::ir::TbProgram,
    function: crate::ir::FunctionId,
) -> usize {
    prog.functions
        .iter()
        .flat_map(|owner| &owner.blocks)
        .flat_map(|block| &block.stmts)
        .find_map(|stmt| match stmt {
            crate::ir::Stmt::MethodHookSubscribe {
                handler, captures, ..
            } if *handler == function => Some(captures.len()),
            _ => None,
        })
        .unwrap_or(0)
}

fn test_hook_signature(
    prog: &crate::ir::TbProgram,
    function: crate::ir::FunctionId,
    dut_access: Option<&crate::ir::passes::dut_access::DutAccessPlan>,
) -> Result<String, super::EmitError> {
    let body = prog.functions.get(function.index()).ok_or_else(|| {
        super::EmitError(format!(
            "tbir: runtime-cell hook closure references missing fn{}",
            function.0
        ))
    })?;
    let capture_count = test_hook_capture_count(prog, function);
    let capture_base = body.params.len().saturating_sub(capture_count);
    let params = body
        .locals
        .iter()
        .take(body.params.len())
        .enumerate()
        .map(|(index, local)| {
            let resolved_ty = if matches!(local.ty, crate::ir::IrType::Unknown) {
                dut_access
                    .and_then(|plan| {
                        plan.inferred_local_type(function, crate::ir::LocalId(index as u32))
                    })
                    .unwrap_or(&local.ty)
            } else {
                &local.ty
            };
            persistent_local_cty(prog, resolved_ty).map(|ty| {
                if index >= capture_base {
                    format!("{ty}&")
                } else {
                    ty
                }
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(params.join(", "))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn test_runtime_cells_struct(
    out: &mut String,
    prog: &crate::ir::TbProgram,
    plan: &crate::ir::passes::runtime_cells::RuntimeCellPlan,
    test: &crate::ir::TestSchema,
    stem: &str,
    lanes: &HashMap<String, u32>,
    dut_type: &str,
    dut_access: Option<&crate::ir::passes::dut_access::DutAccessPlan>,
    randomize: Option<&crate::codegen::cpp_tb::TbirRandomizeEmissionPlan>,
) -> Result<Option<String>, super::EmitError> {
    use crate::ir::passes::runtime_cells::{RuntimeCellKind, RuntimeCellOwner, TemporalCheck};

    let test_owner = RuntimeCellOwner::Test {
        test: test.id,
        name: test.name.clone(),
    };
    let testbench = prog.testbench(test.testbench);
    let testbench_owner = RuntimeCellOwner::Testbench {
        testbench: test.testbench,
        name: testbench.name.clone(),
    };
    let mut cells = plan
        .for_owner(&test_owner)
        .chain(plan.for_owner(&testbench_owner))
        .filter(|cell| {
            has_capsule_storage(cell.kind())
                && (randomize.is_some()
                    || !matches!(cell.kind(), RuntimeCellKind::ConstraintState { .. }))
        })
        .collect::<Vec<_>>();
    cells.sort_by_key(|cell| (cell.registration(), cell.id().clone()));
    if cells.is_empty() {
        return Ok(None);
    }

    let type_name = unique_generated_type_name(prog, &format!("HarcRuntimeCells_{stem}"));
    writeln!(out, "struct {type_name} {{").ok();
    for cell in cells {
        match cell.kind() {
            RuntimeCellKind::TemporalPrevious { check, slot } => {
                let function = statement_cell_function(prog, test.id, cell.kind())?;
                let temporal = match check {
                    TemporalCheck::Property(property) => prog
                        .property_checks
                        .get(property.index())
                        .and_then(|schema| schema.temporals.get(*slot as usize)),
                    TemporalCheck::Cover(cover) => prog
                        .cover_checks
                        .get(cover.index())
                        .and_then(|schema| schema.temporals.get(*slot as usize)),
                }
                .ok_or_else(|| {
                    super::EmitError(format!(
                        "tbir: runtime cell {:?} references a missing temporal slot",
                        cell.kind()
                    ))
                })?;
                let ty = super::func::temporal_cell_cpp_type(
                    prog, function, lanes, dut_type, dut_access, temporal,
                );
                let field = cell.symbol();
                let init = runtime_cell_initializer(cell)?;
                writeln!(out, "{INDENT}{ty} {field}{init};").ok();
            }
            RuntimeCellKind::PropertyImplicationPrevious { .. } => {
                let init = runtime_cell_initializer(cell)?;
                writeln!(out, "{INDENT}bool {}{init};", cell.symbol()).ok();
            }
            RuntimeCellKind::CoverHits { .. } => {
                let init = runtime_cell_initializer(cell)?;
                writeln!(out, "{INDENT}uint64_t {}{init};", cell.symbol()).ok();
            }
            RuntimeCellKind::StatementPeriodicLast { .. } => {
                let init = runtime_cell_initializer(cell)?;
                writeln!(out, "{INDENT}int64_t {}{init};", cell.symbol()).ok();
            }
            RuntimeCellKind::StatementEdgePrevious { .. } => {
                let init = runtime_cell_initializer(cell)?;
                writeln!(out, "{INDENT}bool {}{init};", cell.symbol()).ok();
            }
            RuntimeCellKind::TestbenchPeriodicLast { .. } => {
                let init = runtime_cell_initializer(cell)?;
                writeln!(out, "{INDENT}int64_t {}{init};", cell.symbol()).ok();
            }
            RuntimeCellKind::TestbenchEdgePrevious { .. } => {
                let init = runtime_cell_initializer(cell)?;
                writeln!(out, "{INDENT}bool {}{init};", cell.symbol()).ok();
            }
            RuntimeCellKind::ConstraintState { site } => {
                let state = randomize
                    .and_then(|randomize| randomize.site_states.get(site.index()))
                    .ok_or_else(|| {
                        super::EmitError(format!(
                            "tbir: runtime cell {:?} has no randomize emission state",
                            cell.kind()
                        ))
                    })?;
                randomize_site_state_field(out, 1, &cell.symbol(), state);
            }
            RuntimeCellKind::LocalEventSubscribers { member, event }
            | RuntimeCellKind::PersistentLocal {
                member,
                local: event,
            } => {
                let function = match member {
                    crate::ir::TestCallableMember::Run => test.run,
                    crate::ir::TestCallableMember::Check => test.check.ok_or_else(|| {
                        super::EmitError(format!(
                            "tbir: test `{}` has persistent check storage without a check body",
                            test.name
                        ))
                    })?,
                };
                let body = prog.function(function);
                let local = body.locals.get(event.index()).ok_or_else(|| {
                    super::EmitError(format!(
                        "tbir: persistent runtime cell references missing fn{} local %{}",
                        function.0, event.0
                    ))
                })?;
                let resolved_ty = if matches!(local.ty, crate::ir::IrType::Unknown) {
                    dut_access
                        .and_then(|plan| plan.inferred_local_type(function, *event))
                        .unwrap_or(&local.ty)
                } else {
                    &local.ty
                };
                let ty = persistent_local_cty(prog, resolved_ty)?;
                let field = cell.symbol();
                let init = runtime_cell_initializer(cell)?;
                writeln!(out, "{INDENT}{ty} {field}{init};").ok();
            }
            RuntimeCellKind::TestHookClosure { function, .. } => {
                let signature = test_hook_signature(prog, *function, dut_access)?;
                let field = cell.symbol();
                let init = runtime_cell_initializer(cell)?;
                writeln!(
                    out,
                    "{INDENT}std::function<void({signature})> {field}{init};"
                )
                .ok();
            }
            other => {
                return Err(super::EmitError(format!(
                    "tbir: runtime cell {other:?} has no capsule storage renderer"
                )));
            }
        }
    }
    writeln!(out, "}};").ok();
    writeln!(out).ok();
    Ok(Some(type_name))
}

pub(super) fn randomize_site_state_field(
    out: &mut String,
    depth: usize,
    name: &str,
    state: &crate::codegen::cpp_tb::TbirRandomizeSiteState,
) {
    let pad = INDENT.repeat(depth);
    let member_pad = INDENT.repeat(depth + 1);
    writeln!(out, "{pad}struct {{").ok();
    if let Some(problem_id) = state.problem_id {
        writeln!(
            out,
            "{member_pad}harc_rt::random::HarcRuntimeCallSite call_site{{{problem_id}ULL, {problem_id}ULL, 0}};"
        )
        .ok();
    }
    for cell in &state.cells {
        match &cell.init {
            Some(init) => writeln!(out, "{member_pad}{} {} = {};", cell.ctype, cell.tag, init).ok(),
            None => writeln!(out, "{member_pad}{} {}{{}};", cell.ctype, cell.tag).ok(),
        };
    }
    writeln!(out, "{pad}}} {name}{{}};").ok();
}

fn component_method_param_ctypes(
    prog: &crate::ir::TbProgram,
    m: &crate::ir::ComponentMethodSchema,
) -> Vec<String> {
    let func = prog.function(m.function);
    (0..m.param_names.len())
        .map(|i| match func.locals[i].ty {
            crate::ir::IrType::Record(r) => prog.records[r.index()].name.clone(),
            crate::ir::IrType::RecordSeq(r) => {
                format!("std::vector<{}>", prog.records[r.index()].name)
            }
            crate::ir::IrType::Seq(ref scalar) => {
                format!("std::vector<{}>", super::field_scalar_cty(scalar))
            }
            crate::ir::IrType::Component(c) => prog.components[c.index()].name.clone(),
            ref ty => super::local_scalar_cty(ty),
        })
        .collect()
}

pub(super) fn tb_struct(
    out: &mut String,
    testbench: crate::ir::TestbenchId,
    tb: &crate::ir::TestbenchSchema,
    dut_type: &str,
    cov_fields: &[(String, String)],
    state_fields: &[crate::ir::TbStateFieldSchema],
    record_fields: &[(String, crate::ir::RecordId)],
    scoreboard_fields: &[(String, String)],
    records: &[crate::ir::RecordSchema],
    runtime_cells: &crate::ir::passes::runtime_cells::RuntimeCellPlan,
) -> Result<(), super::EmitError> {
    use crate::ir::passes::runtime_cells::{ComponentHeartbeat, RuntimeCellKind, RuntimeCellOwner};
    let tb_name = &tb.name;
    let owner = RuntimeCellOwner::Testbench {
        testbench,
        name: tb_name.to_string(),
    };
    let heartbeat_cells = [ComponentHeartbeat::Input, ComponentHeartbeat::Output]
        .into_iter()
        .map(|heartbeat| {
            runtime_cells
                .find(&owner, &RuntimeCellKind::TestbenchHeartbeat(heartbeat))
                .map(|cell| (heartbeat, cell))
                .ok_or_else(|| {
                    super::EmitError(format!(
                        "tbir: testbench `{tb_name}` has no planned {heartbeat:?} heartbeat cell"
                    ))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    writeln!(out, "struct {tb_name} {{").ok();
    writeln!(out, "{INDENT}V{dut_type}* dut = nullptr;").ok();
    for (field, cg_name) in cov_fields {
        writeln!(out, "{INDENT}{cg_name} {field};").ok();
    }
    for (field, sb_type) in scoreboard_fields {
        writeln!(out, "{INDENT}{sb_type} {field};").ok();
    }
    for (field, record) in record_fields {
        writeln!(out, "{INDENT}{} {field}{{}};", records[record.index()].name).ok();
    }
    for field in state_fields {
        match field {
            crate::ir::TbStateFieldSchema::Scalar(f)
                if matches!(f.ty, crate::ir::IrType::FixedVec { .. }) =>
            {
                // Fixed-vector host field. Zero-filled brace-init on the
                // `std::array` member (`std::array<cty, N> mem{};`),
                // matching v1 — `scalar_field_decl`'s `= <init>` form has no
                // constructor `std::array` accepts, which is exactly why a
                // `default` on such a field is refused at lowering.
                writeln!(
                    out,
                    "{INDENT}{} {}{{}};",
                    super::field_scalar_cty(&f.ty),
                    f.name
                )
                .ok();
            }
            crate::ir::TbStateFieldSchema::Scalar(f) => {
                let (cty, init) = scalar_field_decl(&f.ty, f.default);
                writeln!(out, "{INDENT}{cty} {} = {init};", f.name).ok();
            }
            crate::ir::TbStateFieldSchema::Queue(f) => {
                let elem = queue_elem_cty(&f.elem, records);
                writeln!(out, "{INDENT}harc_rt::HarcQueue<{elem}> {};", f.name).ok();
            }
        }
    }
    for (heartbeat, cell) in heartbeat_cells {
        let field = testbench_heartbeat_field(tb, heartbeat);
        let init = runtime_cell_initializer(cell)?;
        writeln!(out, "{INDENT}uint64_t {field}{init};").ok();
    }
    writeln!(out, "}};").ok();
    writeln!(out).ok();
    Ok(())
}

fn testbench_internal_member_name(tb: &crate::ir::TestbenchSchema, base: &str) -> String {
    let collides = |candidate: &str| {
        candidate == tb.dut_field
            || tb.cov_fields.iter().any(|(field, _)| field == candidate)
            || tb.state_fields.iter().any(|field| match field {
                crate::ir::TbStateFieldSchema::Scalar(field) => field.name == candidate,
                crate::ir::TbStateFieldSchema::Queue(field) => field.name == candidate,
            })
            || tb.record_fields.iter().any(|(field, _)| field == candidate)
            || tb
                .scoreboard_fields
                .iter()
                .any(|(field, _)| field == candidate)
            || tb
                .component_fields
                .iter()
                .any(|field| field.field == candidate)
            || tb
                .transactor_fields
                .iter()
                .any(|(field, _)| field == candidate)
    };
    let mut name = base.to_string();
    while collides(&name) {
        name = format!("_u_{name}");
    }
    name
}

pub(super) fn testbench_heartbeat_field(
    tb: &crate::ir::TestbenchSchema,
    heartbeat: crate::ir::passes::runtime_cells::ComponentHeartbeat,
) -> String {
    use crate::ir::passes::runtime_cells::ComponentHeartbeat;
    testbench_internal_member_name(
        tb,
        match heartbeat {
            ComponentHeartbeat::Input => "_last_in_cycle",
            ComponentHeartbeat::Output => "_last_out_cycle",
        },
    )
}

pub(super) fn context_struct(
    out: &mut String,
    dut_type: &str,
    randomize_site_states: &[crate::codegen::cpp_tb::TbirRandomizeSiteState],
) {
    writeln!(out, "struct HarcTestContext {{").ok();
    writeln!(out, "{INDENT}VerilatedContext verilated;").ok();
    writeln!(out, "{INDENT}V{dut_type}* dut = nullptr;").ok();
    out.push_str(
        r#"#if HARC_TRACE_ENABLED
    HarcTraceC* tfp = nullptr;
    std::string _wave_path;
#endif
    uint64_t _trace_time = 0;
    int errors = 0;
    bool _fatal = false;
    int cycle_count = 0;
    harc_rt::trace::HarcTraceWriter trace;
    harc_rt::log::HarcLogContext log_ctx;
    harc_rt::random::HarcRng rng;
    std::vector<std::function<void()>> _checkers;
    std::vector<std::function<void()>> _post_eval_services;
    std::vector<std::function<void()>> _auto_cov_reports;
"#,
    );
    for (index, state) in randomize_site_states.iter().enumerate() {
        randomize_site_state_field(out, 1, &format!("_harc_randomize_c{index}"), state);
    }
    writeln!(out, "}};").ok();
    writeln!(out).ok();
    if randomize_site_states
        .iter()
        .any(|state| state.problem_id.is_some())
    {
        writeln!(
            out,
            "static inline harc_rt::random::HarcRandomizeCall harc_prepare_randomize_call("
        )
        .ok();
        writeln!(
            out,
            "{INDENT}harc_rt::random::HarcRuntimeCallSite& call_site,"
        )
        .ok();
        writeln!(out, "{INDENT}harc_rt::random::harc_problem_id problem_id,").ok();
        writeln!(out, "{INDENT}harc_rt::random::harc_seed global_seed,").ok();
        writeln!(out, "{INDENT}harc_rt::random::harc_seed fallback_seed) {{").ok();
        writeln!(
            out,
            "{INDENT}return harc_rt::random::harc_prepare_randomize_call("
        )
        .ok();
        writeln!(out, "{INDENT}{INDENT}_harc_runtime_random_problem_table,").ok();
        writeln!(out, "{INDENT}{INDENT}&call_site,").ok();
        writeln!(out, "{INDENT}{INDENT}1,").ok();
        writeln!(out, "{INDENT}{INDENT}problem_id,").ok();
        writeln!(out, "{INDENT}{INDENT}global_seed,").ok();
        writeln!(out, "{INDENT}{INDENT}fallback_seed);").ok();
        writeln!(out, "}}").ok();
        writeln!(out).ok();
    }
}

/// `run_<Test>` prologue: context construction, alias refs, wave trace
/// setup, RNG seed, trace writer and dump lambdas.
pub(super) fn run_prologue(out: &mut String, test_name: &str, dut_type: &str, cosim: bool) {
    if cosim {
        // Co-sim form: runs on the bridge's TB thread (see
        // harc_cosim_rt.h). The simulator owns argv and time, so no
        // commandArgs; everything else matches the direct emission.
        writeln!(out, "int run_{test_name}() {{").ok();
    } else {
        writeln!(out, "int run_{test_name}(int argc, char** argv) {{").ok();
    }
    writeln!(out, "{INDENT}HarcTestContext ctx;").ok();
    if !cosim {
        writeln!(out, "{INDENT}ctx.verilated.commandArgs(argc, argv);").ok();
    }
    writeln!(
        out,
        "{INDENT}ctx.dut = harc_rt::log::harc_make_dut<V{dut_type}>(&ctx.verilated);"
    )
    .ok();
    out.push_str(
        r#"    auto* dut = ctx.dut;
#if HARC_TRACE_ENABLED
    auto* tfp = ctx.tfp;
    auto& _wave_path = ctx._wave_path;
#endif
    auto& _trace_time = ctx._trace_time;
    auto& errors = ctx.errors;
    auto& _fatal = ctx._fatal;
    auto& cycle_count = ctx.cycle_count;
    auto& trace = ctx.trace;
    auto& log_ctx = ctx.log_ctx;
    auto& harc_rng = ctx.rng;
    auto harc_rng_next = [&]() { return harc_rng.next(); };
    auto& _checkers = ctx._checkers;
    auto& _post_eval_services = ctx._post_eval_services;
    auto& _auto_cov_reports = ctx._auto_cov_reports;
#if HARC_TRACE_ENABLED
    Verilated::traceEverOn(true);
    tfp = new HarcTraceC;
    _wave_path = harc_rt::log::harc_open_wave_trace(dut, tfp, harc_rt::log::harc_wave_default_name());
#endif

    harc_rng.seed_from_env();
"#,
    );
    writeln!(
        out,
        "{INDENT}harc_rt::trace::harc_start_trace(trace, harc_rng.state, \"{dut_type}\", \"{test_name}\", cycle_count);"
    )
    .ok();
    out.push_str(
        r#"    auto _harc_trace_dump_next = [&](const char* clock, uint64_t clock_cycle) {
        uint64_t t = _trace_time++;
        trace.set_timing(t, clock, clock_cycle);
        HARC_RT_DUMP_WAVE_TRACE(tfp, t);
    };
    auto _harc_trace_dump_at = [&](uint64_t t, const char* clock, uint64_t clock_cycle) {
        trace.set_timing(t, clock, clock_cycle);
        HARC_RT_DUMP_WAVE_TRACE(tfp, t);
    };

    HARC_RT_LOG_WAVE_FILE(log_ctx.sim_log, _wave_path);


"#,
    );
}

/// Multi-clock scheduler (declared `clock` ports): `clocks_` vector +
/// `eval_clocks_until` + `tick`. Mirrors v1's emission exactly.
pub(super) fn clocked_scheduler(out: &mut String, clocks: &[ClockSpec]) {
    writeln!(out, "{INDENT}long long now_ps = 0;").ok();
    writeln!(out, "{INDENT}struct ClockState {{ const char* name; long long half_period_ps; long long next_edge_ps; int level; long long rising_count; }};").ok();
    writeln!(out, "{INDENT}std::vector<ClockState> clocks_;").ok();
    for c in clocks {
        let half = c.period_ps / 2;
        writeln!(
            out,
            "{INDENT}clocks_.push_back(ClockState{{\"{}\", {half}, {half}, 0, 0}});",
            c.name
        )
        .ok();
        writeln!(out, "{INDENT}dut->{} = 0;", c.name).ok();
    }
    writeln!(out).ok();
    out.push_str(
        r#"    auto eval_clocks_until = [&](long long t_ps) {
        while (now_ps < t_ps) {
            long long next = t_ps;
            for (auto& c : clocks_) if (c.next_edge_ps < next) next = c.next_edge_ps;
            now_ps = next;
            bool _primary_rising = false;
            const char* _last_edge_clock = "";
            uint64_t _last_edge_cycle = 0;
            for (size_t i = 0; i < clocks_.size(); i++) {
                auto& c = clocks_[i];
                if (c.next_edge_ps == now_ps) {
                    c.level = !c.level;
"#,
    );
    for (idx, c) in clocks.iter().enumerate() {
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}if (i == {idx}) dut->{} = c.level;",
            c.name
        )
        .ok();
    }
    out.push_str(
        r#"                    c.next_edge_ps += c.half_period_ps;
                    if (c.level == 1) c.rising_count++;
                    _last_edge_clock = c.name;
                    _last_edge_cycle = (uint64_t)c.rising_count;
                    if (i == 0 && c.level == 1) { cycle_count++; _primary_rising = true; }
                }
            }
            dut->eval();
            // Set the semantic trace timing for this edge *before* running
            // post_eval services so any trace events they emit carry the
            // correct time, but defer the waveform dump until after those
            // services (and the follow-up eval) have settled DUT-visible
            // state. VCD cannot represent two dumps at the same physical
            // timestamp, so we dump exactly once per `now_ps` (issue #477).
            trace.set_timing((uint64_t)now_ps, _last_edge_clock, _last_edge_cycle);
"#,
    );
    writeln!(
        out,
        "{INDENT}{INDENT}{INDENT}if (_primary_rising) {{ {} }}",
        post_eval_service_body("_post_eval_services", "dut")
    )
    .ok();
    out.push_str(
        r#"            _harc_trace_dump_at((uint64_t)now_ps, _last_edge_clock, _last_edge_cycle);
        }
    };

    auto tick = [&]() {
        long long target = now_ps + clocks_[0].half_period_ps * 2;
        eval_clocks_until(target);
"#,
    );
    checker_callbacks(out, 2, "_checkers");
    out.push_str("    };\n\n");
}

/// Single-clock backward-compat `tick` (no declared clocks).
///
/// Clockless tests historically only had synthetic cycle time via
/// `tick()`. TB-IR also lowers wall-clock waits to
/// `eval_clocks_until(now_ps + N)`, so provide a minimal absolute-time
/// runtime here: wall waits settle the DUT at the requested picosecond
/// timestamp, while normal cycle ticks keep `now_ps` synchronized to the
/// trace timeline.
pub(super) fn clockless_scheduler(out: &mut String) {
    out.push_str(
        r#"    long long now_ps = 0;
    auto eval_clocks_until = [&](long long t_ps) {
        if (t_ps <= now_ps) return;
        now_ps = t_ps;
        dut->eval();
        _harc_trace_dump_at((uint64_t)now_ps, "", (uint64_t)cycle_count);
        if (_trace_time <= (uint64_t)now_ps) _trace_time = (uint64_t)now_ps + 1;
    };

    auto tick = [&]() {
        _harc_eval_negedge(dut);
        _harc_trace_dump_next("clk", (uint64_t)cycle_count);
        now_ps = (long long)_trace_time;
        _harc_eval_posedge(dut);
        _harc_trace_dump_next("clk", (uint64_t)(cycle_count + 1));
        now_ps = (long long)_trace_time;
        cycle_count++;
"#,
    );
    post_eval_services(out, 2, "_post_eval_services", "dut");
    out.push_str(
        r#"        if (!_post_eval_services.empty()) { _harc_trace_dump_next("clk", (uint64_t)cycle_count); now_ps = (long long)_trace_time; }
"#,
    );
    checker_callbacks(out, 2, "_checkers");
    out.push_str("    };\n\n");
}

/// Log lambdas + seed line + scheduler declaration.
pub(super) fn log_helpers_and_seed(out: &mut String, qualified_clock_wait: bool) {
    out.push_str(
        r#"    auto sim_logf_line = [&](FILE* f, const char* sev, const char* fmt, ...) {
        HARC_RT_LOG_FILE_ONLY_PRINTF(f, cycle_count, sev, fmt);
    };

    auto sim_log_line = [&](const char* sev, const char* fmt, ...) {
        HARC_RT_LOG_PRINTF(log_ctx.sim_log, &trace, cycle_count, sev, fmt);
    };

    // Route an empty-queue pop through the sim's own FATAL path instead
    // of the runtime header's abort backstop: the run records the
    // failure, unwinds normally, and still writes its log and trace.
    // `_fatal` is a loop-exit condition, so the run stops at the next
    // scheduler tick — same semantics as `log(fatal, ...)`. Emitted
    // verbatim by both codegens so the two traces stay diffable.
    harc_rt::HarcQueueFatalScope _queue_fatal_scope([&]() {
        sim_log_line("FATAL", "queue front/pop on an empty queue -- guard it with .empty()/.size(), or wait until the producer has pushed");
        ctx.errors++;
        _fatal = true;
    });

    sim_log_line("INFO", "seed=%llu", (long long)harc_rng.state);

    harc_rt::ThreadScheduler sched;
"#,
    );
    if qualified_clock_wait {
        out.push_str(
            r#"    harc_rt::ThreadSlot* _harc_running_slot = nullptr;
    std::function<void()> _harc_advance_actors = []() {};
    bool _harc_actor_tick_due = false;
"#,
        );
    }
}

/// Register an actor coroutine slot. Cooperative (`mt=false`) mode pushes
/// the slot into the global `sched` (single-threaded round-robin). Under
/// `--mt`, the actor gets a dedicated `harc_rt::ThreadScheduler` (declared
/// here) running on its own OS worker thread; the `(sched, slot)` pair is
/// recorded in `actor_threads` so the worker-spawn / barrier protocol picks
/// it up. Every actor slot has one worker and workers tick in deterministic
/// registration order.
pub(super) fn register_actor_slot(
    out: &mut String,
    mt: bool,
    actor_threads: &mut Vec<(String, String)>,
    sched_var: &str,
    slot_var: &str,
    depth: usize,
) {
    let pad = INDENT.repeat(depth);
    if mt {
        writeln!(out, "{pad}harc_rt::ThreadScheduler {sched_var};").ok();
        writeln!(out, "{pad}harc_rt::ThreadSlot {slot_var};").ok();
        writeln!(out, "{pad}{sched_var}.slots.push_back(&{slot_var});").ok();
        actor_threads.push((sched_var.to_string(), slot_var.to_string()));
    } else {
        writeln!(out, "{pad}harc_rt::ThreadSlot {slot_var};").ok();
        writeln!(out, "{pad}sched.slots.push_back(&{slot_var});").ok();
    }
}

pub(super) fn register_context_actor_slot(
    out: &mut String,
    mt: bool,
    actor_threads: &mut Vec<(String, String)>,
    context: &str,
    sched_var: &str,
    slot_var: &str,
    depth: usize,
) {
    let pad = INDENT.repeat(depth);
    writeln!(
        out,
        "{pad}auto& {slot_var} = *{context}.actor_slots.emplace_back(std::make_unique<harc_rt::ThreadSlot>());"
    )
    .ok();
    if mt {
        writeln!(
            out,
            "{pad}auto& {sched_var} = *{context}.actor_schedulers.emplace_back(std::make_unique<harc_rt::ThreadScheduler>());"
        )
        .ok();
        writeln!(out, "{pad}{sched_var}.slots.push_back(&{slot_var});").ok();
        actor_threads.push((sched_var.to_string(), slot_var.to_string()));
    } else {
        writeln!(
            out,
            "{pad}{context}.scheduler.slots.push_back(&{slot_var});"
        )
        .ok();
    }
}

/// `--mt` worker-thread topology, emitted after the run-slot is set up and
/// before the drive loop. Ports v1's `cpp_tb.rs:2440-2492` verbatim: a
/// shutdown flag, two `N+1`-participant barriers (main + N workers), and
/// one `std::thread` per actor running the dual-barrier loop
/// `while(true){ start.wait(); if(shutdown) break; sched.tick(); end.wait(); }`.
/// Each per-actor scheduler must already be `bootstrap()`-ed (single
/// threaded) before the workers spin up. No-op when `actor_threads` is
/// empty (cooperative mode / no actors).
pub(super) fn mt_worker_setup(
    out: &mut String,
    actor_threads: &[(String, String)],
    qualified_clock_wait: bool,
) {
    if actor_threads.is_empty() {
        return;
    }
    let n = actor_threads.len();
    writeln!(
        out,
        "{INDENT}// Phase 3a: per-actor OS threads with dual barrier sync."
    )
    .ok();
    writeln!(
        out,
        "{INDENT}// {n} actor(s) → {} barrier participants (main + workers).",
        n + 1
    )
    .ok();
    writeln!(out, "{INDENT}std::atomic<bool> _shutdown{{false}};").ok();
    writeln!(out, "{INDENT}std::atomic<size_t> _worker_turn{{0}};").ok();
    writeln!(out, "{INDENT}harc_rt::Barrier _start_barrier({});", n + 1).ok();
    writeln!(out, "{INDENT}harc_rt::Barrier _end_barrier({});", n + 1).ok();
    writeln!(out, "{INDENT}std::vector<std::thread> _workers;").ok();
    for (worker_index, (sched_var, _)) in actor_threads.iter().enumerate() {
        writeln!(out, "{INDENT}_workers.emplace_back([&]() {{").ok();
        writeln!(out, "{INDENT}{INDENT}while (true) {{").ok();
        writeln!(out, "{INDENT}{INDENT}{INDENT}_start_barrier.wait();").ok();
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}if (_shutdown.load(std::memory_order_acquire)) break;"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}while (_worker_turn.load(std::memory_order_acquire) != {worker_index}) std::this_thread::yield();"
        )
        .ok();
        writeln!(out, "{INDENT}{INDENT}{INDENT}{sched_var}.tick();").ok();
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}_worker_turn.fetch_add(1, std::memory_order_release);"
        )
        .ok();
        writeln!(out, "{INDENT}{INDENT}{INDENT}_end_barrier.wait();").ok();
        writeln!(out, "{INDENT}{INDENT}}}").ok();
        writeln!(out, "{INDENT}}});").ok();
    }
    if qualified_clock_wait {
        writeln!(out, "{INDENT}_harc_advance_actors = [&]() {{").ok();
        writeln!(
            out,
            "{INDENT}{INDENT}_worker_turn.store(0, std::memory_order_release);"
        )
        .ok();
        writeln!(out, "{INDENT}{INDENT}_start_barrier.wait();").ok();
        writeln!(out, "{INDENT}{INDENT}_end_barrier.wait();").ok();
        writeln!(out, "{INDENT}}};").ok();
        writeln!(out).ok();
    }
}

/// `--mt` shutdown sequence, emitted after the drive loop. Ports v1's
/// `cpp_tb.rs:2672-2685`: workers are parked on `_start_barrier` awaiting
/// their next iteration; set `_shutdown`, wake them through the start
/// barrier so they observe the flag and break before reaching
/// `_end_barrier`, then join. No-op in cooperative mode.
pub(super) fn mt_worker_shutdown(out: &mut String, actor_threads: &[(String, String)]) {
    if actor_threads.is_empty() {
        return;
    }
    writeln!(out).ok();
    writeln!(
        out,
        "{INDENT}_shutdown.store(true, std::memory_order_release);"
    )
    .ok();
    writeln!(out, "{INDENT}_start_barrier.wait();").ok();
    writeln!(out, "{INDENT}for (auto& _w : _workers) _w.join();").ok();
}

/// Bootstrap fan-out: resume the global scheduler, then each per-actor
/// scheduler (`--mt`), all single-threaded — emitted BEFORE the worker
/// threads spin up (`mt_worker_setup`) so every actor's initial-setup
/// statements run race-free. Mirrors v1's `cpp_tb.rs:2417-2438`.
pub(super) fn drive_bootstrap(out: &mut String, actor_threads: &[(String, String)]) {
    out.push_str(
        r#"
    // Resume each coroutine once so initial-setup statements run
    // before the first clock edge. Single-threaded — workers
    // haven't been spawned yet.
    sched.bootstrap();
"#,
    );
    for (sched_var, _) in actor_threads {
        writeln!(out, "{INDENT}{sched_var}.bootstrap();").ok();
    }
}

/// Main drive loop. `clocked` selects between the eval_clocks_until loop
/// and the negedge/posedge loop, mirroring v1. When `actor_threads` is
/// non-empty (`--mt`), the loop fences the worker tick between
/// `_start_barrier.wait()` / `_end_barrier.wait()` after `sched.tick()`
/// and before the `_checkers` fan-out — so the run-coroutine's writes
/// complete before workers wake, and the workers' DUT-input writes
/// complete before the next eval (no shared-state race). Bootstrap is emitted
/// separately by `drive_bootstrap` (the worker spawn sits between).
pub(super) fn drive_loop(
    out: &mut String,
    clocked: bool,
    actor_threads: &[(String, String)],
    qualified_clock_wait: bool,
) {
    let mt = !actor_threads.is_empty();
    out.push_str(
        r#"
    // Drive the clock until the run coroutine completes.
    //
    // `wait N cycles` matches Verilog's `@(posedge clk)` semantic:
    // values set in the segment BEFORE the wait are sampled at the
    // next posedge. Per loop iteration:
    //   1. Posedge (clk 0→1, eval) — DUT FFs latch the current input
    //      values (set in the previous segment, or in bootstrap on
    //      the first iteration).
    //   2. `sched.tick()` — advance the run coroutine to its next
    //      wait, setting the inputs for the NEXT cycle's posedge.
    //   3. Falling edge (clk 1→0, eval) — comb re-settles with the
    //      newly-set inputs.
    //   4. Cycle counter + checkers.
    // One initial `eval(clk=0)` before the loop settles combinational
    // logic with the bootstrap inputs — same role as `initial`-block
    // settle in Verilog. Each `wait 1 cycle` then maps to exactly
    // one posedge that observes the just-set values.
"#,
    );
    if mt {
        out.push_str(
            r#"    //
    // MT mode: workers run between tick() and the falling edge, gated by
    // _start_barrier / _end_barrier. Run-coroutine writes complete BEFORE
    // workers wake → no race on shared queues. Workers' DUT-input writes
    // complete BEFORE the falling-edge eval → no race on signal state.
"#,
        );
    }

    let barrier_gate = if mt {
        "        _worker_turn.store(0, std::memory_order_release);\n        _start_barrier.wait();\n        _end_barrier.wait();\n"
    } else {
        ""
    };
    if clocked {
        out.push_str(
            r#"    dut->eval();
    _harc_trace_dump_at((uint64_t)now_ps, "", 0);
    while (_run_slot.kind != harc_rt::WaitKind::Done && !_fatal) {
        long long _target = now_ps + clocks_[0].half_period_ps * 2;
        eval_clocks_until(_target);
"#,
        );
        if mt && qualified_clock_wait {
            out.push_str("        _harc_actor_tick_due = true;\n");
        }
        out.push_str("        sched.tick();\n");
        if mt && qualified_clock_wait {
            out.push_str(
                "        if (_harc_actor_tick_due) { _harc_advance_actors(); _harc_actor_tick_due = false; }\n",
            );
        } else {
            out.push_str(barrier_gate);
        }
        checker_callbacks(out, 2, "_checkers");
        out.push_str("    }\n");
    } else {
        out.push_str(
            r#"    _harc_eval_negedge(dut);
    _harc_trace_dump_next("clk", (uint64_t)cycle_count);
    now_ps = (long long)_trace_time;
    while (_run_slot.kind != harc_rt::WaitKind::Done && !_fatal) {
        _harc_eval_posedge(dut);
        _harc_trace_dump_next("clk", (uint64_t)(cycle_count + 1));
        now_ps = (long long)_trace_time;
        cycle_count++;
"#,
        );
        post_eval_services(out, 2, "_post_eval_services", "dut");
        out.push_str(
            r#"        if (!_post_eval_services.empty()) { _harc_trace_dump_next("clk", (uint64_t)cycle_count); now_ps = (long long)_trace_time; }
        sched.tick();
"#,
        );
        out.push_str(barrier_gate);
        out.push_str(
            r#"        _harc_eval_negedge(dut);
        _harc_trace_dump_next("clk", (uint64_t)cycle_count);
        now_ps = (long long)_trace_time;
"#,
        );
        checker_callbacks(out, 2, "_checkers");
        out.push_str("    }\n");
    }
}

/// Coverage reports, finalization, exit status.
///
/// `covers` is the test's concurrent-`cover` checks in registration
/// order; each contributes a hit/total tally and a per-point report line
/// (v1's property-cover summary, ARCH-style: header then per-point lines
/// with a `*NOT HIT*` marker supplied by the runtime printer).
pub(super) fn run_epilogue(
    out: &mut String,
    cosim: bool,
    covers: &[(crate::ir::CoverCheckId, &crate::ir::CoverCheckSchema)],
    actor_threads: &[(String, String)],
    runtime_cells: Option<super::expr::RuntimeCellRenderBinding<'_>>,
) -> Result<(), super::EmitError> {
    out.push_str("\n\n");
    automatic_coverage_reports(out, 1, "_auto_cov_reports");
    concurrent_coverage_reports(out, 1, covers, runtime_cells, "log_ctx.coverage_json")?;
    clear_run_callbacks(
        out,
        1,
        "_auto_cov_reports",
        "_post_eval_services",
        "_checkers",
    );
    destroy_scheduler_threads(
        out,
        1,
        std::iter::once("sched").chain(
            actor_threads
                .iter()
                .map(|(scheduler, _)| scheduler.as_str()),
        ),
    );
    out.push_str(
        r#"    dut->final();
"#,
    );
    if !cosim {
        // Verilator coverage belongs to the simulator process on the
        // co-sim path; the shim has no Verilated context to query.
        out.push_str("    HARC_RT_WRITE_COVERAGE(ctx.verilated.coveragep());\n");
    }
    out.push_str(
        r#"    HARC_RT_CLOSE_WAVE_TRACE(tfp);
    delete dut;

"#,
    );
    out.push_str(
        "    return harc_rt::log::harc_finish_sim_run(log_ctx, trace, cycle_count, errors);\n}\n\n",
    );
    Ok(())
}

/// `main` dispatcher: `--test <name>` / `HARC_TEST` selection.
pub(super) fn dispatcher(out: &mut String, test_names: &[String]) {
    writeln!(out, "int main(int argc, char** argv) {{").ok();
    writeln!(
        out,
        "{INDENT}const char* test_sel = harc_rt::log::harc_select_test(argc, argv);"
    )
    .ok();
    let first = &test_names[0];
    writeln!(
        out,
        "{INDENT}if (!test_sel) return run_{first}(argc, argv);"
    )
    .ok();
    for name in test_names {
        writeln!(
            out,
            "{INDENT}if (std::strcmp(test_sel, \"{name}\") == 0) return run_{name}(argc, argv);"
        )
        .ok();
    }
    let avail = test_names.join(", ");
    writeln!(
        out,
        "{INDENT}harc_rt::log::harc_report_unknown_test(test_sel, \"{avail}\");"
    )
    .ok();
    writeln!(out, "{INDENT}return 1;").ok();
    writeln!(out, "}}").ok();
}

/// Co-sim entrypoints (`--cosim dpi`): replaces `main()`. The generated
/// `HarcCosimTop.sv` harness imports these over DPI-C; test selection
/// comes from `HARC_TEST` (the harc CLI sets it — Verilator's generated
/// `main()` owns argv on this path). The selected `run_<Test>` executes
/// on the bridge's dedicated TB thread under a strict handshake: the
/// simulator thread stays parked inside `harc_cosim_step` while the TB
/// runs, so all TB-side simulator access happens inside a DPI
/// entrypoint by delegation (see harc_cosim_rt.h). `run_until_request`
/// is idempotent after completion — Verilator defers $finish to the end
/// of the timestep, so the master process may step once more after the
/// test finishes.
pub(super) fn cosim_entrypoints(out: &mut String, test_names: &[String], half_period_ps: u64) {
    out.push_str(
        r#"extern "C" void harc_cosim_init() {
    auto& _b = harc_rt::cosim::bridge();
"#,
    );
    writeln!(out, "{INDENT}_b.half_period_ps = {half_period_ps};").ok();
    out.push_str(
        r#"    _b.vl_ctx = Verilated::threadContextp();
    _b.vl_scope = svGetScope();
    const char* test_sel = harc_rt::log::harc_select_test(0, nullptr);
"#,
    );
    let first = &test_names[0];
    writeln!(
        out,
        "{INDENT}if (!test_sel) {{ _b.body = &run_{first}; return; }}"
    )
    .ok();
    for name in test_names {
        writeln!(
            out,
            "{INDENT}if (std::strcmp(test_sel, \"{name}\") == 0) {{ _b.body = &run_{name}; return; }}"
        )
        .ok();
    }
    let avail = test_names.join(", ");
    writeln!(
        out,
        "{INDENT}harc_rt::log::harc_report_unknown_test(test_sel, \"{avail}\");"
    )
    .ok();
    writeln!(out, "{INDENT}_b.body = nullptr;").ok();
    writeln!(out, "}}").ok();
    out.push_str(
        r#"
extern "C" long long harc_cosim_step() {
    auto& _b = harc_rt::cosim::bridge();
    if (!_b.body) return harc_rt::cosim::RC_DONE_FAIL;
    return _b.run_until_request();
}

// Called from the harness's `final` block on every simulation end.
// Detects ends the HARC runtime did not drive — most commonly a
// DUT-initiated $finish — which would otherwise exit 0 with no test
// summary while the TB thread is still parked on the bridge.
extern "C" void harc_cosim_shutdown() {
    auto& _b = harc_rt::cosim::bridge();
    if (_b.done || !_b.started) return;
    std::fflush(stdout);
    std::fprintf(stderr,
                 "FATAL: simulation ended outside HARC control (DUT-initiated "
                 "$finish/$stop?) before the test completed\n");
    std::fflush(stderr);
    // The TB thread is parked on the bridge and can never finish;
    // exit hard with a distinct status so callers see a failure, not
    // a silent zero-exit with no summary.
    std::_Exit(97);
}
"#,
    );
}
