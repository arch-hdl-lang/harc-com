//! TB-IR → C++ testbench backend (`--codegen tbir`).
//!
//! Consumes a verified `ir::TbProgram` and emits a Verilator-class C++
//! TB whose scaffolding mirrors the live v1 (`cpp_tb`) contract —
//! preamble, `HarcTestContext`, clock scheduler, `sim_log_line`
//! plumbing, trace events, exit status, and the `--test`/`HARC_TEST`
//! dispatcher `main()` — so a tbir binary is a drop-in replacement on
//! both the `--sv` (Verilator) and `--dut` (arch sim) paths and its
//! semantic trace diffs clean against v1.
//!
//! Function bodies use a loop-switch over `BlockId` instead of
//! re-structured control flow; see `func.rs`.

pub mod common;
mod covergroup;
mod expr;
mod func;
mod runtime;

use crate::ast::SourceFile;
use crate::codegen::cpp_tb::{EmitError, EmitOpts, GeneratedCppFile, SplitCppOutput};
use crate::ir::{self, TbProgram};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const INDENT: &str = "    ";

/// Whether a testbench needs a `_tb` host struct emitted (and a `_tb`
/// instance declared in each owning test). Non-synthetic testbenches
/// always do — they own the DUT handle plus cov/scoreboard/transactor
/// state. A SYNTHETIC testbench (classic `test` form, no `testbench`
/// binding) normally has none, but it acquires one when it carries
/// promoted scalar fields: a test-scope `let` written in `run` and read
/// in `check` is promoted to a `_tb` scalar field so its value persists
/// across the run→check boundary (the two phases lower to separate IR
/// functions). The promoted field is the only `_tb` member used in that
/// case — the unused `dut` handle stays a nullptr (synthetic tests use a
/// bare `dut` local).
fn needs_tb_struct(tb: &ir::TestbenchSchema) -> bool {
    !tb.synthetic || !tb.state_fields.is_empty() || !tb.record_fields.is_empty()
}

pub(super) fn emit_component_dut_bindings(
    out: &mut String,
    prog: &TbProgram,
    component: ir::ComponentId,
    instance: &str,
    dut_type: &str,
    dut_receiver: &str,
    depth: usize,
) -> Result<(), EmitError> {
    let schema = prog.components.get(component.index()).ok_or_else(|| {
        EmitError(format!(
            "tbir: component DUT binding references missing component c{}",
            component.0
        ))
    })?;
    let pad = INDENT.repeat(depth);
    for field in &schema.fields {
        match &field.kind {
            ir::ComponentFieldKind::Dut {
                dut_type: field_type,
            } => {
                if field_type != dut_type {
                    return Err(EmitError(format!(
                        "tbir: component `{}` DUT field `{}` has module type `{field_type}`, expected `{dut_type}`",
                        schema.name, field.name
                    )));
                }
                writeln!(out, "{pad}{instance}.{} = {dut_receiver};", field.name).ok();
            }
            ir::ComponentFieldKind::Sub {
                component: child, ..
            } => {
                emit_component_dut_bindings(
                    out,
                    prog,
                    *child,
                    &format!("{instance}.{}", field.name),
                    dut_type,
                    dut_receiver,
                    depth,
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Whether this testbench installs a user hook on this component method.
pub(super) fn has_component_method_hook_subscription(
    prog: &TbProgram,
    owner: ir::TestbenchId,
    component: ir::ComponentId,
    method: &str,
) -> bool {
    prog.functions
        .iter()
        .filter(|f| f.owner == Some(owner))
        .any(|f| {
            f.blocks.iter().any(|b| {
                b.stmts.iter().any(|s| {
                    matches!(
                        s,
                        ir::Stmt::MethodHookSubscribe {
                            target: ir::MethodHookTarget::Component {
                                component: target,
                                method: target_method,
                                ..
                            },
                            ..
                        } if *target == component && target_method == method
                    )
                })
            })
        })
}

/// Suite-global emission state: everything `emit` computes that is the
/// same for any subset of the suite's tests.
///
/// The split path builds each shard's program as `prog.clone()` with only
/// `.tests` overwritten (see `plan_split_tests`), so any value not derived
/// from `prog.tests` is shard-invariant *by construction* — which is what
/// makes hoisting it out of the per-shard loop byte-identical rather than
/// merely plausible. Built once per suite and shared by `&` across shard
/// workers.
pub struct SuiteScaffold {
    /// From `tests[0]`'s testbench, but `validate_tests_share_dut` proves
    /// every test agrees, so the string is the same for any test subset.
    dut_type: String,
    // The three fields below are suite-invariant BY DESIGN — do NOT narrow
    // them to the selected tests (harc#538). Each reads an unfiltered
    // program table, so today every shard is emitted with identical bytes,
    // including shards whose own tests use no probe and no `randomize`.
    has_probes: bool,
    uses_constraint_solver: bool,
    problem_table_cpp: String,
    randomize_snippets: Vec<String>,
    randomize_site_states: Vec<crate::codegen::cpp_tb::TbirRandomizeSiteState>,
    runtime_cells: ir::passes::runtime_cells::RuntimeCellPlan,
    dut_access: Option<ir::passes::dut_access::DutAccessPlan>,
    bus_access: Option<ir::passes::bus_access::BusAccessPlan>,
}

impl SuiteScaffold {
    pub fn bus_access(&self) -> Option<&ir::passes::bus_access::BusAccessPlan> {
        self.bus_access.as_ref()
    }

    /// `dut_scope` is the phrase used in the multi-DUT diagnostic — "one
    /// binary" for whole-program emission, "one split binary" for a split
    /// build.
    ///
    /// The order of the fallible steps here is load-bearing: it reproduces
    /// the order `emit` used before the split refactor, so a program with
    /// more than one defect still reports the same first error.
    fn build(
        prog: &TbProgram,
        file: &SourceFile,
        opts: &EmitOpts,
        dut_scope: &str,
    ) -> Result<Self, EmitError> {
        // All tests in one binary share the DUT type (same v0 rule as v1).
        let dut_type = validate_tests_share_dut(prog, dut_scope)?;

        // Preserve the source-level gate diagnostic before physical catalog
        // validation can report the intentionally absent gated-off port.
        check_gated_bus_access(prog, file, opts)?;
        let dut_access = opts
            .dut_interface
            .as_ref()
            .map(|interface| {
                let plan = if opts.program_verified {
                    ir::passes::dut_access::analyze_verified(prog, interface)
                } else {
                    ir::passes::dut_access::analyze(prog, interface)
                };
                plan.map_err(|error| EmitError(format!("tbir: {error}")))
            })
            .transpose()?;
        let bus_access = opts
            .dut_interface
            .as_ref()
            .map(|interface| {
                ir::passes::bus_access::analyze(prog, interface)
                    .map_err(|error| EmitError(format!("tbir: {error}")))
            })
            .transpose()?;

        // Constraint-solver wiring (randomize sites). The runtime problem
        // table + per-site Z3-solve snippets are emitted by v1's shared
        // constraint codegen ("only the call site moves to the IR backend").
        // Empty when the program has no randomize site — the TB then never
        // links Z3, exactly like v1.
        let randomize = crate::codegen::cpp_tb::plan_tbir_randomize_emission(
            file,
            opts,
            &prog.constraint_sites,
            5,
        )?;
        let problem_table_cpp = if randomize.runtime_table.problems.is_empty() {
            String::new()
        } else {
            randomize
                .runtime_table
                .render_cpp_inline_descriptors("_harc_runtime_random_problem_table")
        };
        let randomize_snippets = randomize.snippets;
        let randomize_site_states = randomize.site_states;
        // The runtime metadata table intentionally excludes component-scope
        // sites. Include detection must follow the source/codegen decision,
        // not table non-emptiness, or a component-only kept struct emits Z3
        // calls without the runtime header.
        let uses_constraint_solver = crate::codegen::cpp_tb::uses_constraint_solver(file);

        // Probe reads/forces dereference `dut->rootp->...`, which needs the
        // root struct's full definition (`V<Top>___024root.h`) — the `rootp`
        // member in `V<Top>.h` is only a forward-declared pointer. Mirrors
        // v1's `aggregated_probes` include gate. See docs/probe-signals.md.
        let has_probes = dut_access
            .as_ref()
            .map_or_else(|| program_has_probes(prog), |plan| plan.uses_probe());
        let runtime_cells = ir::passes::runtime_cells::analyze(prog)
            .map_err(|error| EmitError(format!("tbir: {error}")))?;
        if opts.cosim.is_some() && opts.mt {
            return Err(EmitError(
                "--cosim dpi does not support --mt yet (actor worker threads \
                 would call into simulator-owned state outside a DPI entrypoint)"
                    .into(),
            ));
        }

        Ok(SuiteScaffold {
            dut_type,
            has_probes,
            uses_constraint_solver,
            problem_table_cpp,
            randomize_snippets,
            randomize_site_states,
            runtime_cells,
            dut_access,
            bus_access,
        })
    }
}

/// Whether a translation unit carries the `main()`-equivalent tail.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EmitTail {
    /// `runtime::dispatcher`, or `runtime::cosim_entrypoints` under `--cosim`.
    Whole,
    /// No tail — a split shard. `main()` lives in the split dispatcher, and
    /// the caller applies `finish_shard` to normalize the trailing bytes.
    ShardBody,
}

pub fn emit(prog: &TbProgram, file: &SourceFile, opts: &EmitOpts) -> Result<String, EmitError> {
    if prog.tests.is_empty() {
        return Err(EmitError("no `test` declaration found".to_string()));
    }
    let scaffold = SuiteScaffold::build(prog, file, opts, "one binary")?;
    let all: Vec<usize> = (0..prog.tests.len()).collect();
    emit_selected_tests(prog, file, opts, &scaffold, &all, EmitTail::Whole)
}

/// #619 M4b: which reusable-testbench lifecycle bodies can be emitted OUT
/// OF LINE (once) rather than re-inlined per test, keyed to their variant
/// (`Plain` void function / `Coro` `HarcThread`). Empty unless
/// `HARC_TBIR_NATIVE_LIFECYCLE` minted the `TestbenchLifecycle` functions.
/// Computed identically by every emission path (monolithic, self-contained
/// shard, and separate/common interface+common+shard) so the definitions,
/// prototypes, and call sites always agree.
fn shareable_lifecycle_map(prog: &TbProgram) -> HashMap<ir::FunctionId, func::LifecycleEmit> {
    prog.functions
        .iter()
        .filter(|f| matches!(f.kind, ir::FunctionKind::TestbenchLifecycle { .. }))
        .filter_map(|f| func::lifecycle_shareable_kind(f).map(|k| (f.id, k)))
        .collect()
}

/// #619 M4b: the `TestbenchLifecycle` FunctionIds actually referenced (via
/// `Terminator::TbLifecycleCall`) by the run/check bodies of `test_indices`.
/// A self-contained shard carries only a subset of the suite's tests, so it
/// must define only the lifecycle bodies its own tests call — a `static`
/// definition that nothing in the TU calls trips `-Wunused-function`.
fn referenced_lifecycle_fns(prog: &TbProgram, test_indices: &[usize]) -> HashSet<ir::FunctionId> {
    let mut set = HashSet::new();
    for &i in test_indices {
        let test = &prog.tests[i];
        for fid in std::iter::once(test.run).chain(test.check) {
            for b in &prog.function(fid).blocks {
                if let ir::Terminator::TbLifecycleCall { function, .. } = &b.terminator {
                    set.insert(*function);
                }
            }
        }
    }
    set
}

/// #619 M4b: emit each shareable lifecycle body ONCE — a `Plain` body as a
/// `void` function, a `Coro` body as a `harc_rt::HarcThread` coroutine.
/// `static_linkage` is `true` for a complete single TU (monolithic or a
/// self-contained shard: internal linkage, definition precedes the tests)
/// and `false` for the separate/common layout (external linkage, the
/// definition lives once in the common `.cpp` and shards call it).
///
/// `referenced` scopes WHICH bodies are emitted: `Some(set)` (a complete
/// single TU) emits only bodies its own tests call, so an internal-linkage
/// def is never left uncalled (`-Wunused-function`); `None` (the common
/// `.cpp`) emits ALL shareable bodies with external linkage, since different
/// shards in other TUs call different ones and an unused EXTERNAL function
/// does not warn.
fn emit_shared_lifecycle_defs(
    out: &mut String,
    prog: &TbProgram,
    vec_lane_widths: &HashMap<String, u32>,
    randomize_snippets: &[String],
    dut_type: &str,
    dut_access: Option<&ir::passes::dut_access::DutAccessPlan>,
    map: &HashMap<ir::FunctionId, func::LifecycleEmit>,
    common_context: bool,
    static_linkage: bool,
    referenced: Option<&HashSet<ir::FunctionId>>,
) -> Result<(), EmitError> {
    for f in &prog.functions {
        let Some(kind) = map.get(&f.id).copied() else {
            continue;
        };
        if referenced.is_some_and(|set| !set.contains(&f.id)) {
            continue;
        }
        let ir::FunctionKind::TestbenchLifecycle { testbench, .. } = f.kind else {
            continue;
        };
        let tb = prog.testbench(testbench);
        match kind {
            func::LifecycleEmit::Plain => func::emit_lifecycle_function(
                out,
                prog,
                f,
                &tb.name,
                &prog.records,
                &tb.bus_bindings,
                vec_lane_widths,
                randomize_snippets,
                dut_type,
                dut_access,
                common_context,
                static_linkage,
                map,
            )?,
            func::LifecycleEmit::Coro => func::emit_lifecycle_coroutine(
                out,
                prog,
                f,
                &tb.name,
                &prog.records,
                &tb.bus_bindings,
                vec_lane_widths,
                randomize_snippets,
                dut_type,
                dut_access,
                common_context,
                static_linkage,
                map,
            )?,
        }
    }
    Ok(())
}

/// #619 M4b: emit forward declarations for every shareable lifecycle body
/// into the split/common interface header, so shards in other translation
/// units can call the definitions that live in the common `.cpp`.
fn emit_shared_lifecycle_prototypes(
    out: &mut String,
    prog: &TbProgram,
    map: &HashMap<ir::FunctionId, func::LifecycleEmit>,
) {
    for f in &prog.functions {
        let Some(kind) = map.get(&f.id).copied() else {
            continue;
        };
        let ir::FunctionKind::TestbenchLifecycle { testbench, .. } = f.kind else {
            continue;
        };
        let tb = prog.testbench(testbench);
        func::emit_lifecycle_prototype(out, f, &tb.name, kind);
    }
}

/// Emit one translation unit covering `test_indices` (indices into
/// `prog.tests`), borrowing the whole verified program.
///
/// Everything outside the `run_<Test>` bodies is suite-global and comes
/// either from `scaffold` or from an unfiltered `prog` table; only
/// `test_names` and the emitted test bodies depend on the selection. That
/// is exactly the set of things the old clone-and-filter split path varied
/// per shard, so output is byte-identical to it.
fn emit_selected_tests(
    prog: &TbProgram,
    file: &SourceFile,
    opts: &EmitOpts,
    scaffold: &SuiteScaffold,
    test_indices: &[usize],
    tail: EmitTail,
) -> Result<String, EmitError> {
    // SELECTED test names only — they feed the preamble's test-list comment
    // and the dispatcher tail, both of which were shard-scoped before.
    let test_names: Vec<String> = test_indices
        .iter()
        .map(|&i| prog.tests[i].name.clone())
        .collect();
    let dut_type = &scaffold.dut_type;

    let mut out = String::new();
    runtime::preamble(
        &mut out,
        dut_type,
        &test_names,
        &scaffold.problem_table_cpp,
        scaffold.uses_constraint_solver,
        scaffold.has_probes,
        opts.cosim.as_ref(),
    );

    // Transaction value-record structs, in declaration order. Mirrors
    // v1's `emit_record_struct` shape (field defaults as member
    // initializers, `operator==`/`!=`). Pack/unpack helpers follow the
    // structural schema below. Legacy `randomize_<T>` helpers are emitted
    // only when a dynamic-list record is present: constrained sites use the
    // shared solver, while an unconstrained list can legitimately fall back
    // to v1's field-draw helper.
    // Emit in TOPOLOGICAL order (a record after every record it nests) so
    // an inner struct's definition and `harc_pack_*` precede any outer
    // struct that holds it by value — C++ needs the complete inner type.
    let record_order = record_emit_order(&prog.records);
    for &i in &record_order {
        record_struct(&mut out, &prog.records[i], &prog.records);
    }
    if prog.records.iter().any(|record| {
        record
            .fields
            .iter()
            .any(|field| matches!(field.ty, ir::IrType::Seq(_)))
    }) {
        out.push_str(&crate::codegen::cpp_tb::emit_record_randomize_helpers(
            file,
            opts,
            &prog.records,
            &record_order,
        )?);
    }

    // RAL per-register write-callback recursion-depth limit — emitted once
    // when any testbench registers `on regs.REG` callbacks (mirrors v1's
    // `#ifndef HARC_RAL_CB_MAX_DEPTH` block). Guards a callback that
    // re-enters `record_write` from blowing the host stack.
    if prog
        .testbenches
        .iter()
        .any(|tb| tb.regblock_bindings.iter().any(|b| !b.callbacks.is_empty()))
    {
        out.push_str(
            "#ifndef HARC_RAL_CB_MAX_DEPTH\n\
             static constexpr uint32_t HARC_RAL_CB_MAX_DEPTH = 16;\n\
             #endif\n",
        );
    }

    // Scoreboard structs (data-only host-state records — they never name
    // a TB or DUT type), before the testbench structs that hold them.
    for (index, sb) in prog.scoreboards.iter().enumerate() {
        runtime::scoreboard_struct(
            &mut out,
            ir::ScoreboardId(index as u32),
            sb,
            &prog.records,
            &scaffold.runtime_cells,
        )?;
    }

    // Composite-component structs (env/agent cluster). A component holds
    // its sub-components by value, so each held type's struct must be
    // defined first. Source order usually puts subs before the env, but
    // a user may declare the env first — so emit in dependency order
    // (DFS over `Sub` fields), mirroring v1's `topo_sort_component_indices`.
    for ci in component_emit_order(prog) {
        runtime::component_struct(
            &mut out,
            prog,
            ir::ComponentId(ci as u32),
            &prog.components[ci],
            &prog.components,
            &prog.scoreboards,
            &prog.records,
            &scaffold.runtime_cells,
        )?;
    }

    // Lowered pure helpers — file-scope C++ functions. Declaration
    // order is source order, which is not necessarily topological for
    // helper-to-helper calls, so prototypes go first.
    let helpers: Vec<&ir::TbFunction> = prog
        .functions
        .iter()
        .filter(|f| f.kind == ir::FunctionKind::Helper)
        .collect();
    for h in &helpers {
        func::emit_helper_prototype(&mut out, prog, h)?;
    }
    if !helpers.is_empty() {
        writeln!(out).ok();
    }
    crate::codegen::cpp_tb::emit_extern_fn_decls(&mut out, file);
    // Covergroup structs are leaf observables, but hook-triggered sampler
    // bodies may call pure helpers or extern reference functions, so their
    // forward declarations must be visible before the struct definition.
    for cg in &prog.covgroups {
        covergroup::covgroup_struct(&mut out, cg);
    }
    for h in &helpers {
        func::emit_helper_function(&mut out, prog, h)?;
        writeln!(out).ok();
    }

    // One struct per unique testbench that needs a `_tb` host struct.
    // Non-synthetic testbenches always get one. A SYNTHETIC testbench
    // (classic `test` form, no `testbench` binding) normally has no
    // `_tb` — but it still needs one when it carries promoted scalar
    // fields: a test-scope `let` written in `run` and read in `check`
    // is promoted to a `_tb` field so it persists across the run→check
    // boundary (run and check are separate IR functions). See
    // `needs_tb_struct`.
    let mut seen = HashSet::new();
    for (testbench_index, tb) in prog.testbenches.iter().enumerate() {
        if needs_tb_struct(tb) && seen.insert(tb.name.clone()) {
            let cov_fields: Vec<(String, String)> = tb
                .cov_fields
                .iter()
                .map(|(f, cg)| (f.clone(), prog.covgroups[cg.index()].name.clone()))
                .collect();
            let sb_fields: Vec<(String, String)> = tb
                .scoreboard_fields
                .iter()
                .map(|(f, sb)| (f.clone(), prog.scoreboards[sb.index()].name.clone()))
                .collect();
            runtime::tb_struct(
                &mut out,
                ir::TestbenchId(testbench_index as u32),
                tb,
                dut_type,
                &cov_fields,
                &tb.state_fields,
                &tb.record_fields,
                &sb_fields,
                &prog.records,
                &scaffold.runtime_cells,
            )?;
        }
    }
    runtime::context_struct(&mut out, dut_type, &scaffold.randomize_site_states);

    // #619 M4b: emit each shareable reusable-testbench lifecycle body ONCE
    // at file scope — a plain `void` function for a non-suspending body, a
    // `harc_rt::HarcThread` coroutine for a suspending one — and lower its
    // `TbLifecycleCall`s accordingly (see `func::emit_lifecycle_function` /
    // `func::emit_lifecycle_coroutine` / the `TbLifecycleCall` arm).
    //
    // Both `EmitTail::Whole` (monolithic single TU) and `EmitTail::ShardBody`
    // (a `SplitCppPlan` self-contained shard) emit the FULL preamble/structs
    // here, so each is a complete translation unit: the `static` definitions
    // precede the tests and are internally linked, giving within-TU
    // de-duplication with no cross-TU ODR risk. The separate/common layout
    // (`emit_separate_*`, NOT this function) is the cross-shard case: its
    // shards call EXTERNAL definitions that live once in the common `.cpp`.
    // The map is empty unless `HARC_TBIR_NATIVE_LIFECYCLE` produced the
    // `TestbenchLifecycle` functions at lowering time.
    let outofline_lifecycle: HashMap<ir::FunctionId, func::LifecycleEmit> =
        shareable_lifecycle_map(prog);
    // This is a complete single TU (monolithic Whole, or a self-contained
    // split shard): define only the bodies THIS TU's tests call, with
    // internal (`static`) linkage — a shard carrying a subset of tests must
    // not emit a `static` def nothing in the TU calls (`-Wunused-function`),
    // and must not export an external symbol another shard also defines
    // (duplicate-symbol link error). For `Whole` every shareable body is
    // referenced, so the filter is a no-op there.
    let referenced_lifecycle = referenced_lifecycle_fns(prog, test_indices);
    emit_shared_lifecycle_defs(
        &mut out,
        prog,
        &opts.vec_lane_widths,
        &scaffold.randomize_snippets,
        dut_type,
        scaffold.dut_access.as_ref(),
        &outofline_lifecycle,
        /* common_context */ false,
        /* static_linkage */ true,
        Some(&referenced_lifecycle),
    )?;

    for &i in test_indices {
        emit_test(
            &mut out,
            prog,
            &prog.tests[i],
            dut_type,
            opts,
            &scaffold.randomize_snippets,
            &scaffold.runtime_cells,
            scaffold.dut_access.as_ref(),
            &outofline_lifecycle,
        )?;
    }

    match tail {
        EmitTail::Whole if opts.cosim.is_some() => runtime::cosim_entrypoints(
            &mut out,
            &test_names,
            opts.cosim
                .as_ref()
                .map(|c| c.half_period_ps)
                .unwrap_or(5000),
        ),
        EmitTail::Whole => runtime::dispatcher(&mut out, &test_names),
        EmitTail::ShardBody => {}
    }
    Ok(out)
}

/// One shard's share of a split build: which tests it carries and what
/// file it lands in. Stable and cheap — planning is separate from emission
/// so a driver can report the whole shape of the build up front and then
/// emit shards independently, in any order.
#[derive(Clone, Debug)]
pub struct SplitShardPlan {
    /// 0-based position in `SplitCppPlan::shards`. Shards may complete out
    /// of order under parallel emission; this is what restores determinism.
    pub index: usize,
    pub filename: String,
    /// Indices into `TbProgram::tests`, ascending.
    pub test_indices: Vec<usize>,
}

/// A planned split build: the dispatcher (renderable before any shard work
/// happens) plus one entry per shard. Also carries the suite-global
/// scaffolding so every shard emission reuses it instead of recomputing it.
pub struct SplitCppPlan {
    pub dispatcher: GeneratedCppFile,
    /// Every test in the suite, in program order — the dispatcher's
    /// selection table. Shard membership lives in `shards`.
    pub test_names: Vec<String>,
    pub shards: Vec<SplitShardPlan>,
    scaffold: SuiteScaffold,
}

/// Plan a dispatcher plus one or more self-contained C++ translation units
/// for TB-IR tests. The split happens after lowering: each shard keeps the
/// full lowered scaffolding (records, components, helpers, randomize tables)
/// and emits only its selected `run_<Test>` functions. This mirrors the v1
/// split-linkage contract while avoiding a shared C++ ABI for TB-IR runtime
/// internals.
///
/// All suite-wide validation happens here, once, so a shard emission that
/// starts can only fail on its own test bodies.
pub fn plan_split_tests(
    prog: &TbProgram,
    file: &SourceFile,
    opts: &EmitOpts,
    file_prefix: &str,
    group_size: usize,
) -> Result<SplitCppPlan, EmitError> {
    let group_size = group_size.max(1);
    if prog.tests.is_empty() {
        return Err(EmitError("no `test` declaration found".into()));
    }
    if opts.cosim.is_some() {
        return Err(EmitError(
            "--cosim dpi does not support split-test builds yet (the split \
             dispatcher links against per-shard `main()` functions; co-sim \
             emission replaces `main()` with DPI entrypoints)"
                .into(),
        ));
    }
    let scaffold = SuiteScaffold::build(prog, file, opts, "one split binary")?;

    let test_names: Vec<String> = prog.tests.iter().map(|t| t.name.clone()).collect();
    let all: Vec<usize> = (0..test_names.len()).collect();
    let shards: Vec<SplitShardPlan> = all
        .chunks(group_size)
        .enumerate()
        .map(|(index, test_indices)| SplitShardPlan {
            index,
            filename: if group_size == 1 {
                format!(
                    "{file_prefix}test_{}.cpp",
                    crate::codegen::cpp_tb::sanitize_file_component(&test_names[test_indices[0]])
                )
            } else {
                format!("{file_prefix}shard{}.cpp", index + 1)
            },
            test_indices: test_indices.to_vec(),
        })
        .collect();

    // Shards are written concurrently, one worker per shard, so two shards
    // sharing a filename would be two workers writing one path. Distinct
    // names hold today (duplicate test names are rejected upstream, and
    // `sanitize_file_component` is the identity on HARC identifiers), but
    // it is a precondition of `emit_split_shards` now, not a nicety.
    debug_assert!(
        {
            let mut names: Vec<&str> = shards.iter().map(|s| s.filename.as_str()).collect();
            names.sort_unstable();
            let before = names.len();
            names.dedup();
            names.len() == before
        },
        "split plan produced two shards with the same filename"
    );

    Ok(SplitCppPlan {
        dispatcher: GeneratedCppFile {
            filename: format!("{file_prefix}main.cpp"),
            contents: emit_split_dispatcher(&test_names),
        },
        test_names,
        shards,
        scaffold,
    })
}

/// Emit one shard of a planned split build, borrowing the verified program.
pub fn emit_split_shard(
    prog: &TbProgram,
    file: &SourceFile,
    opts: &EmitOpts,
    plan: &SplitCppPlan,
    shard: &SplitShardPlan,
) -> Result<String, EmitError> {
    let cpp = emit_selected_tests(
        prog,
        file,
        opts,
        &plan.scaffold,
        &shard.test_indices,
        EmitTail::ShardBody,
    )?;
    Ok(finish_shard(cpp))
}

/// Normalize a shard's trailing bytes.
///
/// The shard body is emitted without a dispatcher tail, so this reproduces
/// exactly what the old post-hoc `rfind("\nint main(...)")` strip left
/// behind: the pre-tail buffer, right-trimmed, plus one newline. Nothing is
/// emitted between the last test body and the tail, so the two agree
/// byte-for-byte — without depending on a sentinel that generated user text
/// could in principle match.
fn finish_shard(mut cpp: String) -> String {
    cpp.truncate(cpp.trim_end().len());
    cpp.push('\n');
    cpp
}

/// Resolve `--emit-jobs` against the machine and the build.
///
/// `0` means automatic; the cap of 4 is deliberate — each in-flight shard
/// holds a whole generated translation unit in memory (~100 MB on large
/// suites), plus whatever the caller's delivery callback allocates per
/// shard, which for a compare-then-write is another copy of the same size.
/// The worker count is a memory knob as much as a speed one.
pub fn resolve_emit_jobs(requested: usize, shard_count: usize) -> usize {
    let shard_count = shard_count.max(1);
    match requested {
        0 => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(shard_count)
            .min(4)
            .max(1),
        n => n.min(shard_count).max(1),
    }
}

/// Emit every shard in `plan`, handing each finished translation unit to
/// `on_shard` **on the worker thread that produced it**, so the caller can
/// consume and drop it immediately.
///
/// Running `on_shard` on the worker is what keeps shard I/O off the
/// critical path: a driver's `write_if_changed` both re-reads the existing
/// file to compare and writes the new one, and on a large suite that is
/// hundreds of MB each way. Doing it on a single receiving thread
/// serialized it behind every shard and put a hard Amdahl ceiling on
/// `--emit-jobs` (harc#546). Each shard has its own path, taken from the
/// plan, so concurrent calls never touch the same file.
///
/// The cost is that `on_shard` must be `Sync` and do its own
/// synchronization for anything shared. To keep that burden small, the
/// per-shard bookkeeping a driver actually needs — which shards were
/// delivered, in what order — comes back as the return value instead:
/// **positions into `plan.shards` for every shard handed to `on_shard`,
/// ascending**, so a caller can build an ordered file list by indexing
/// `plan.shards` directly, with no mutex of its own. (They coincide with
/// `SplitShardPlan::index` for plans this module builds; the return value
/// is defined as the position so indexing stays correct regardless.)
///
/// Peak retained generated C++ stays bounded by `jobs`: a worker holds one
/// shard at a time and drops it before claiming the next.
///
/// Note the *driver's* peak is higher than that figure alone suggests. A
/// `write_if_changed`-style callback also reads the existing file back to
/// compare, and that buffer is now live on every worker at once rather
/// than once on a receiver — budget roughly 2x shard size per worker, not
/// 1x. On the benchmark suite that is about +100 MB at `--emit-jobs 4`.
///
/// Determinism, on success: shard bytes are independent of `jobs`, every
/// shard is handed to `on_shard` exactly once, and the returned indices are
/// sorted. Only the *order of the calls* varies — completion order, which
/// is index order at `jobs == 1`.
///
/// Determinism, on failure: the returned error is the lowest-indexed
/// failure, whether it came from emission or from `on_shard`, so the
/// diagnostic does not depend on thread scheduling. Which shards were
/// already handed out is NOT deterministic — see the note at the delivery
/// site.
pub fn emit_split_shards<F>(
    prog: &TbProgram,
    file: &SourceFile,
    opts: &EmitOpts,
    plan: &SplitCppPlan,
    jobs: usize,
    on_shard: F,
) -> Result<Vec<usize>, EmitError>
where
    F: Fn(&SplitShardPlan, String, Duration) -> Result<(), EmitError> + Sync,
{
    debug_assert!(
        plan.shards
            .iter()
            .all(|s| s.test_indices.iter().all(|&i| i < prog.tests.len())),
        "split plan was built for a different program than the one being emitted"
    );
    if jobs <= 1 || plan.shards.len() <= 1 {
        let mut delivered = Vec::with_capacity(plan.shards.len());
        for (pos, shard) in plan.shards.iter().enumerate() {
            let started = Instant::now();
            let cpp = emit_split_shard(prog, file, opts, plan, shard)?;
            on_shard(shard, cpp, started.elapsed())?;
            delivered.push(pos);
        }
        return Ok(delivered);
    }

    let cursor = AtomicUsize::new(0);
    // Lowest shard index known to have failed, from either emission or the
    // caller's `on_shard`. Workers refuse to CLAIM an index above it, but
    // every index BELOW it is still attempted — that is what makes "lowest
    // index wins" hold at any job count. A blanket cancel flag would let a
    // higher shard's error be reported instead, depending on scheduling.
    let fail_limit = AtomicUsize::new(usize::MAX);
    // Lowest-indexed failure seen so far, whichever side it came from.
    // Keyed by index so the reported error does not depend on which thread
    // finished first. Contended only on the error path.
    let first_err: Mutex<Option<(usize, EmitError)>> = Mutex::new(None);
    // A panic out of `on_shard`, kept separately so the original payload
    // survives to be re-raised on the caller. `thread::scope` would
    // otherwise replace it with its own "a scoped thread panicked".
    type Payload = Box<dyn std::any::Any + Send + 'static>;
    let first_panic: Mutex<Option<(usize, Payload)>> = Mutex::new(None);
    // One lock acquisition per delivered shard, taken after the shard's I/O
    // rather than around it.
    let delivered: Mutex<Vec<usize>> = Mutex::new(Vec::with_capacity(plan.shards.len()));

    let record_err = |i: usize, e: EmitError| {
        fail_limit.fetch_min(i, Ordering::Relaxed);
        let mut slot = first_err.lock().unwrap_or_else(|p| p.into_inner());
        if slot.as_ref().is_none_or(|(j, _)| i < *j) {
            *slot = Some((i, e));
        }
    };
    let record_panic = |i: usize, payload: Payload| {
        // Treat it exactly like a failure for scheduling purposes, so the
        // remaining workers stop claiming instead of emitting and writing
        // every leftover shard behind a caller that has already died.
        fail_limit.fetch_min(i, Ordering::Relaxed);
        let mut slot = first_panic.lock().unwrap_or_else(|p| p.into_inner());
        if slot.as_ref().is_none_or(|(j, _)| i < *j) {
            *slot = Some((i, payload));
        }
    };

    std::thread::scope(|scope| {
        for _ in 0..jobs {
            let cursor = &cursor;
            let fail_limit = &fail_limit;
            let record_err = &record_err;
            let on_shard = &on_shard;
            let delivered = &delivered;
            // Worker threads default to a 2 MiB stack where the main thread
            // gets ~8 MiB, and emission recurses over expression, record,
            // and component trees with no depth guard. Since `--emit-jobs`
            // defaults to automatic, a deeply nested source that emits fine
            // serially must not overflow just because it ran on a worker.
            let spawned = std::thread::Builder::new()
                .stack_size(8 * 1024 * 1024)
                .spawn_scoped(scope, move || loop {
                    let i = cursor.fetch_add(1, Ordering::Relaxed);
                    if i >= plan.shards.len() {
                        break;
                    }
                    if i > fail_limit.load(Ordering::Relaxed) {
                        continue;
                    }
                    let started = Instant::now();
                    let cpp = match emit_split_shard(prog, file, opts, plan, &plan.shards[i]) {
                        Ok(cpp) => cpp,
                        Err(e) => {
                            record_err(i, e);
                            continue;
                        }
                    };
                    // Re-check before handing the shard over: another worker
                    // may have failed a lower index while this one emitted.
                    //
                    // Skipping at or above a known failure keeps a failed
                    // build from writing shards the serial path would never
                    // have reached. The converse does NOT hold — a shard
                    // above the failing index that finished before the
                    // failure was observed has already been written, so a
                    // failed parallel emit can leave a different set of files
                    // than a failed serial one, and a different set run to
                    // run. That is accepted: the next run recomputes every
                    // shard and `write_if_changed` reuses whatever matches,
                    // and the command still exits non-zero without building.
                    if i > fail_limit.load(Ordering::Relaxed) {
                        continue;
                    }
                    // `on_shard` is caller code doing I/O, and it can panic
                    // on something as ordinary as `harc … | head`: the
                    // per-shard progress line then hits EPIPE and `eprintln!`
                    // panics. Catching it here keeps a panic behaving like
                    // the failure it is — remaining workers stop claiming
                    // rather than writing every leftover shard behind a
                    // caller that has already died — and preserves the
                    // payload, which `thread::scope` would otherwise replace
                    // with its own message. It is re-raised on the caller
                    // once the scope joins.
                    let handed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        on_shard(&plan.shards[i], cpp, started.elapsed())
                    }));
                    match handed {
                        Ok(Ok(())) => delivered.lock().unwrap_or_else(|p| p.into_inner()).push(i),
                        // Also bounds the damage: workers stop claiming
                        // higher shards instead of emitting hundreds of MB
                        // that can no longer be written anywhere. The bound
                        // is looser than it was when a bounded channel also
                        // throttled how far a worker could run ahead of the
                        // caller — a shard already in flight when the
                        // failure lands is still delivered.
                        Ok(Err(e)) => record_err(i, e),
                        Err(payload) => record_panic(i, payload),
                    }
                });
            if let Err(e) = spawned {
                record_err(
                    usize::MAX,
                    EmitError(format!("could not spawn shard emission worker: {e}")),
                );
                break;
            }
        }
    });

    // A panic outranks a returned error, matching what happened before
    // delivery moved onto the workers: a panic in `on_shard` then unwound
    // the caller immediately, so no `Err` could overtake it.
    if let Some((_, payload)) = first_panic.into_inner().unwrap_or_else(|p| p.into_inner()) {
        std::panic::resume_unwind(payload);
    }
    if let Some((_, e)) = first_err.into_inner().unwrap_or_else(|p| p.into_inner()) {
        return Err(e);
    }
    let mut delivered = delivered.into_inner().unwrap_or_else(|p| p.into_inner());
    delivered.sort_unstable();
    Ok(delivered)
}

/// Emit a whole split build at once. Retains every shard until the last one
/// finishes; prefer [`plan_split_tests`] + [`emit_split_shards`], which
/// streams. Kept for callers that want the batch shape.
pub fn emit_split_tests_with_file_prefix(
    prog: &TbProgram,
    file: &SourceFile,
    opts: EmitOpts,
    file_prefix: &str,
    group_size: usize,
) -> Result<SplitCppOutput, EmitError> {
    let plan = plan_split_tests(prog, file, &opts, file_prefix, group_size)?;
    let mut files = Vec::with_capacity(plan.shards.len() + 1);
    files.push(GeneratedCppFile {
        filename: plan.dispatcher.filename.clone(),
        contents: plan.dispatcher.contents.clone(),
    });
    // `on_shard` is `Fn + Sync` because it may run on a worker; this call
    // is serial (`jobs = 1`) so the mutex is never contended. Collecting
    // into it keeps the batch shape's original file order.
    let collected: Mutex<Vec<GeneratedCppFile>> = Mutex::new(Vec::with_capacity(plan.shards.len()));
    emit_split_shards(prog, file, &opts, &plan, 1, |shard, cpp, _| {
        collected
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(GeneratedCppFile {
                filename: shard.filename.clone(),
                contents: cpp,
            });
        Ok(())
    })?;
    files.extend(collected.into_inner().unwrap_or_else(|p| p.into_inner()));
    Ok(SplitCppOutput {
        files,
        test_names: plan.test_names,
    })
}

fn validate_tests_share_dut(prog: &TbProgram, scope: &str) -> Result<String, EmitError> {
    let dut_type = prog.testbench(prog.tests[0].testbench).dut_type.clone();
    for t in &prog.tests {
        let tb = prog.testbench(t.testbench);
        if tb.dut_type != dut_type {
            return Err(EmitError(format!(
                "multi-DUT tests in {scope} are out of scope for v0; \
                 test `{}` uses `{}`, but a previous test used `{}`",
                t.name, tb.dut_type, dut_type,
            )));
        }
    }
    Ok(dut_type)
}

fn emit_split_dispatcher(test_names: &[String]) -> String {
    let mut out = String::new();
    writeln!(out, "// Auto-generated by harc — do not edit.").ok();
    writeln!(out, "// HARC TB-IR split-test dispatcher.").ok();
    writeln!(out).ok();
    writeln!(out, "#include <cstring>").ok();
    writeln!(out, "#include \"harc_log_rt.h\"").ok();
    writeln!(out).ok();
    for name in test_names {
        writeln!(out, "extern int run_{name}(int argc, char** argv);").ok();
    }
    writeln!(out).ok();
    runtime::dispatcher(&mut out, test_names);
    out
}

/// Does any function in the program access a DUT-internal probe (a
/// `PortRef` whose access class is `Probe`/`Force`)? Drives the
/// `V<Top>___024root.h` include in the preamble — required for the
/// `dut->rootp->...` probe accessor to compile. Probe reads can sit
/// inline in expression position (assert conditions, format args, RHS),
/// so this walks both statement-level `PortRef`s and the expression
/// trees those statements carry.
fn program_has_probes(prog: &TbProgram) -> bool {
    let mut found = false;
    for function in &prog.functions {
        for block in &function.blocks {
            for stmt in &block.stmts {
                if stmt_has_probe(stmt) {
                    return true;
                }
            }
            ir::visit::try_visit_terminator_exprs(&block.terminator, &mut |expr| {
                found |= expr_has_probe(expr);
                Ok::<(), std::convert::Infallible>(())
            })
            .unwrap_or_else(|error| match error {});
            if found {
                return true;
            }
        }
    }
    if prog.components.iter().any(|component| {
        component
            .periodic_handlers
            .iter()
            .any(|handler| expr_has_probe(&handler.period))
            || component
                .cycle_handlers
                .iter()
                .any(|handler| expr_has_probe(&handler.trigger))
            || component.watchdog.as_ref().is_some_and(|watchdog| {
                watchdog.period.as_ref().is_some_and(expr_has_probe)
                    || watchdog.max_idle.as_ref().is_some_and(expr_has_probe)
            })
    }) {
        return true;
    }
    if prog.testbenches.iter().any(|testbench| {
        testbench
            .cycle_services
            .iter()
            .any(|service| expr_has_probe(&service.trigger))
    }) {
        return true;
    }
    for schema in &prog.covgroups {
        for point in &schema.points {
            if expr_has_probe(&point.target) {
                return true;
            }
            for bin in &point.bins {
                for value in &bin.values {
                    let has_probe = match value {
                        ir::CovBinValue::Eq(ir::CovBinBound::Runtime(expr)) => expr_has_probe(expr),
                        ir::CovBinValue::Range { lo, hi } => {
                            [lo, hi].into_iter().flatten().any(|bound| match bound {
                                ir::CovBinBound::Runtime(expr) => expr_has_probe(expr),
                                ir::CovBinBound::Const(_) => false,
                            })
                        }
                        ir::CovBinValue::Eq(ir::CovBinBound::Const(_)) => false,
                    };
                    if has_probe {
                        return true;
                    }
                }
            }
        }
    }
    for_each_check_body_expr(prog, |e| found |= expr_has_probe(e));
    found
}

fn port_is_probe(p: &ir::PortRef) -> bool {
    !matches!(p.access, ir::PortAccess::Port)
}

fn expr_has_probe(e: &ir::Expr) -> bool {
    let mut found = false;
    ir::visit::walk_expr(e, &mut |expr| match expr {
        ir::Expr::Port(port) | ir::Expr::PortSnapshotLane { port, .. } => {
            found |= port_is_probe(port)
        }
        _ => {}
    });
    found
}

fn stmt_has_probe(s: &ir::Stmt) -> bool {
    if match s {
        ir::Stmt::DutWrite(port, _) | ir::Stmt::DutRead(_, port) | ir::Stmt::ProbeRelease(port) => {
            port_is_probe(port)
        }
        _ => false,
    } {
        return true;
    }
    let mut found = false;
    ir::visit::try_visit_stmt_exprs(s, &mut |expr| {
        found |= expr_has_probe(expr);
        Ok::<(), std::convert::Infallible>(())
    })
    .unwrap_or_else(|error| match error {});
    found
}

/// Invoke `f` on every expression that makes up a concurrent-check body
/// (property shapes, cover predicates, and their temporal latch
/// operands). These live on `TbProgram`, not inside any function, so the
/// program-wide port/probe walks visit them through this helper.
fn for_each_check_body_expr(prog: &TbProgram, mut f: impl FnMut(&ir::Expr)) {
    for p in &prog.property_checks {
        match &p.shape {
            ir::PropertyShape::Implies { ante, cons }
            | ir::PropertyShape::ImpliesNext { ante, cons } => {
                f(ante);
                f(cons);
            }
            ir::PropertyShape::Invariant(e) => f(e),
        }
        for slot in &p.temporals {
            f(&slot.inner);
        }
        // The `else fail(...)` message renders inside the same closure,
        // so its interpolation captures are real program accesses too.
        if let Some(m) = &p.message {
            for a in &m.args {
                f(&a.expr);
            }
        }
    }
    for c in &prog.cover_checks {
        f(&c.cond);
        for slot in &c.temporals {
            f(&slot.inner);
        }
    }
    // A statement-position `on <trigger>` predicate is rendered in the
    // registration closure, so its port reads are real program port
    // accesses even though `Stmt::CycleHandler` carries none itself.
    for h in &prog.cycle_handlers {
        match &h.kind {
            ir::CycleHandlerKind::Trigger { trigger, .. } => f(trigger),
            ir::CycleHandlerKind::Periodic { period } => f(period),
        }
    }
}

/// Per-bind effective `generate_if` param env, mirroring v1's
/// `bus_param_envs` population (`cpp_tb.rs` ~2200): bus defaults overlaid
/// with the bind-site generic (`let s : BusRw<...> = bind dut`) and then
/// the DUT-port override (`port s: target BusRw<WRITE=0>`, sourced into
/// `opts.dut_bus_port_overrides`). The bind name equals the DUT port name
/// by convention, so the override map is keyed by the bind name.
struct GatedBus<'a> {
    decl: &'a crate::ast::BusDecl,
    env: std::collections::HashMap<String, i64>,
}

/// Error out if any function ACCESSES a `generate_if`-gated bus signal
/// that is gated OFF under its bind's effective param env. Mirrors v1's
/// access-site behavior: lowering carries the gates, emission decides
/// presence against the override-applied env so the tbir port set matches
/// `arch build`'s flattened port set for the same DUT override. Ungated
/// signals, and gated-ON signals, resolve normally; a gated-OFF signal
/// that is never accessed is silent (it simply never reaches a PortRef).
pub fn check_gated_bus_access(
    prog: &TbProgram,
    file: &SourceFile,
    opts: &EmitOpts,
) -> Result<(), EmitError> {
    use crate::ast::{BusDecl, Item, TestItem, TypeExpr};

    // Bus declarations in the file, by simple name (inline or `use`-imported).
    let mut buses: std::collections::HashMap<&str, &BusDecl> = std::collections::HashMap::new();
    for it in &file.items {
        if let Item::Bus(b) = it {
            buses.insert(b.name.name.as_str(), b);
        }
    }
    // Bind-site type expr per bind name, for the bind-site generic layer
    // (`let s : BusRw<...> = bind dut`). Recovered from the file's test
    // lets — lowering does not carry the bind `TypeExpr`. First binding
    // name wins on a cross-test collision (matches v1's downstream-bind
    // pre-scan), which is irrelevant in practice since binds are per-test.
    let mut bind_ty: std::collections::HashMap<&str, &TypeExpr> = std::collections::HashMap::new();
    for it in &file.items {
        if let Item::Test(t) = it {
            for ti in &t.items {
                if let TestItem::Let(l) = ti {
                    if l.bind {
                        if let Some(ty) = l.ty.as_ref() {
                            bind_ty.entry(l.name.name.as_str()).or_insert(ty);
                        }
                    }
                }
            }
        }
    }

    // bind-name -> (BusDecl, effective env). Drawn from every testbench's
    // `bus_bindings` (the binding's `field` is the bind name == flat signal
    // prefix == DUT port name). Buses with no gated signals at all are
    // skipped — they can never produce a gated-OFF access.
    let mut gated: std::collections::HashMap<String, GatedBus<'_>> =
        std::collections::HashMap::new();
    for tb in &prog.testbenches {
        for b in &tb.bus_bindings {
            if gated.contains_key(&b.field) {
                continue;
            }
            let Some(&decl) = buses.get(b.bus.as_str()) else {
                continue;
            };
            // Only plain bus signals are gate-checked (mirroring v1's
            // `bus_signal_present`), so a bus whose only gates sit on
            // handshake payloads needs no env built.
            if !decl.signals.iter().any(|s| s.gate.is_some()) {
                continue;
            }
            let env = crate::codegen::cpp_tb::bus_param_env_with_port_override(
                decl,
                bind_ty.get(b.field.as_str()).copied(),
                opts.dut_bus_port_overrides.get(&b.field),
            );
            gated.insert(b.field.clone(), GatedBus { decl, env });
        }
    }
    if gated.is_empty() {
        return Ok(());
    }

    // Walk every PortRef the program accesses; collect gated-OFF errors.
    let mut errors: Vec<String> = Vec::new();
    let mut check = |p: &ir::PortRef| {
        if let Some(err) = gated_off_error(p, &gated) {
            if !errors.contains(&err) {
                errors.push(err);
            }
        }
    };
    for f in &prog.functions {
        for blk in &f.blocks {
            for s in &blk.stmts {
                for_each_port_in_stmt(s, &mut check);
            }
            for_each_port_in_term(&blk.terminator, &mut check);
        }
    }
    for_each_check_body_expr(prog, |e| for_each_port_in_expr(e, &mut check));
    if errors.is_empty() {
        Ok(())
    } else {
        Err(EmitError(errors.join("\n")))
    }
}

/// `Some(message)` when PortRef `p` is rooted at a gated bus bind and
/// names a PLAIN bus signal that is gated OFF under that bind's effective
/// env. `None` otherwise. The message text mirrors v1's gated-OFF
/// diagnostic verbatim.
///
/// Scope deliberately matches v1's `bus_signal_present` exactly: only the
/// 2-segment `[bind, plain-signal]` form is gate-checked. v1 resolves
/// handshake-channel access (`[bind, ch, sig]` and the pre-flattened
/// `[bind, ch_sig]` form) WITHOUT a gate check — see
/// `try_emit_bus_field_access` (`cpp_tb.rs` ~9255+), where the handshake
/// branches return the port unconditionally. Mirroring that here keeps
/// the tbir backend from rejecting an access v1 accepts. A 1-segment
/// remapped path (`bind...with`) is likewise not gate-checked by v1.
fn gated_off_error(
    p: &ir::PortRef,
    gated: &std::collections::HashMap<String, GatedBus<'_>>,
) -> Option<String> {
    if !matches!(p.access, ir::PortAccess::Port) {
        return None;
    }
    let [bind, sig] = p.port_path.as_slice() else {
        return None;
    };
    let g = gated.get(bind)?;
    let s = g.decl.signals.iter().find(|s| &s.name.name == sig)?;
    if crate::codegen::cpp_tb::gate_passes(s.gate.as_ref(), &g.env) {
        return None;
    }
    Some(format!(
        "bus `{}` (binding `{}`) signal `{}` is gated OFF by its \
         `generate_if` condition under the bind's params — `arch build` \
         omits this port, so the testbench must not access it",
        g.decl.name.name, bind, sig,
    ))
}

/// Invoke `f` on every `PortRef` reachable from statement `s` (the
/// statement's own port operands and any port reads nested in its
/// expression trees). Parallels `stmt_has_probe`'s traversal, collecting
/// instead of testing.
fn for_each_port_in_stmt(s: &ir::Stmt, f: &mut impl FnMut(&ir::PortRef)) {
    match s {
        ir::Stmt::DutWrite(port, _) | ir::Stmt::DutRead(_, port) | ir::Stmt::ProbeRelease(port) => {
            f(port)
        }
        _ => {}
    }
    ir::visit::try_visit_stmt_exprs(s, &mut |expr| {
        for_each_port_in_expr(expr, f);
        Ok::<(), std::convert::Infallible>(())
    })
    .unwrap_or_else(|error| match error {});
}

/// Invoke `f` on every `PortRef` in a block terminator's expression
/// operands (`Branch`/`WaitCycles`/`WaitUntil` conditions can read a bus
/// signal).
fn for_each_port_in_term(t: &ir::Terminator, f: &mut impl FnMut(&ir::PortRef)) {
    ir::visit::try_visit_terminator_exprs(t, &mut |expr| {
        for_each_port_in_expr(expr, f);
        Ok::<(), std::convert::Infallible>(())
    })
    .unwrap_or_else(|error| match error {});
}

/// Invoke `f` on every `PortRef` in an expression tree. Parallels
/// `expr_has_probe`'s structural traversal.
fn for_each_port_in_expr(e: &ir::Expr, f: &mut impl FnMut(&ir::PortRef)) {
    ir::visit::walk_expr(e, &mut |expr| match expr {
        ir::Expr::Port(port) | ir::Expr::PortSnapshotLane { port, .. } => f(port),
        _ => {}
    });
}

/// C++ storage type for a record field's scalar (or Vec element) type,
/// using the same width-aware integer policy as standalone locals.
pub(super) fn field_scalar_cty(ty: &ir::IrType) -> String {
    match ty {
        ir::IrType::Bool => "bool".to_string(),
        ir::IrType::String => "const char*".to_string(),
        // A nested-vector element renders as a nested `std::array`, the
        // recursion terminating at the scalar leaf. `Vec<Vec<uint<8>,2>,2>`
        // → `std::array<std::array<uint64_t, 2>, 2>`, matching v1.
        ir::IrType::FixedVec { elem, len } => {
            format!("std::array<{}, {len}>", field_scalar_cty(elem))
        }
        _ => local_scalar_cty(ty),
    }
}

/// C++ value carrier for a queue aggregate whose recursive leaf may be a
/// scalar or a declared record.
pub(super) fn aggregate_value_cty(ty: &ir::IrType, records: &[ir::RecordSchema]) -> String {
    match ty {
        ir::IrType::Record(record) => records[record.index()].name.clone(),
        ir::IrType::FixedVec { elem, len } => {
            format!("std::array<{}, {len}>", aggregate_value_cty(elem, records))
        }
        scalar => field_scalar_cty(scalar),
    }
}

/// C++ value/parameter carrier for a callable `IrType`. This is the single
/// recursive resolver behind hook signatures, transactor and component method
/// parameter/return lists, helper and testbench-method signatures, and callable
/// locals. Record / record-sequence / component values keep their schema
/// carriers; a sequence element and a fixed-vector dimension both recurse
/// through the aggregate carrier, so `Seq<Vec<T, N>>` renders
/// `std::vector<std::array<…>>` exactly like a standalone `Vec<T, N>`, and
/// every callable surface agrees on the spelling. Scalar leaves use the scalar
/// ABI carrier (`local_scalar_cty`).
pub(super) fn callable_value_cty(prog: &TbProgram, ty: &ir::IrType) -> Result<String, EmitError> {
    Ok(match ty {
        ir::IrType::Record(record) => prog
            .records
            .get(record.index())
            .ok_or_else(|| {
                EmitError(format!(
                    "tbir: callable value references missing record r{}",
                    record.0
                ))
            })?
            .name
            .clone(),
        ir::IrType::RecordSeq(record) => format!(
            "std::vector<{}>",
            prog.records
                .get(record.index())
                .ok_or_else(|| {
                    EmitError(format!(
                        "tbir: callable value references missing record r{}",
                        record.0
                    ))
                })?
                .name
        ),
        ir::IrType::Seq(elem) => {
            format!("std::vector<{}>", aggregate_value_cty(elem, &prog.records))
        }
        ir::IrType::FixedVec { .. } => aggregate_value_cty(ty, &prog.records),
        ir::IrType::Component(component) => prog
            .components
            .get(component.index())
            .ok_or_else(|| {
                EmitError(format!(
                    "tbir: callable value references missing component c{}",
                    component.0
                ))
            })?
            .name
            .clone(),
        other => local_scalar_cty(other),
    })
}

/// C++ storage type for a loop-switch local / method param. Unsigned and
/// unknown scalars ≤64 bits widen to `uint64_t`; a `sint` ≤64 bits is
/// `int64_t`, matching v1's `c_type_for`, so signed division, modulo,
/// comparisons, and usual-arithmetic conversions come out identical to
/// the legacy backend (u64-backing signed locals was NOT value-identical:
/// `s / 2` and `s < 0` on a negative `sint<8>` local diverged — #524
/// adversarial-review finding 6). A 65..128-bit `uint`/`sint` uses v1's
/// `_harc_u128` (`__uint128_t`), while wider declared scalars use the
/// shared `HarcWide<N>` runtime storage. Aggregate types
/// (`Record`/`RecordSeq`) are handled by their own declaration sites,
/// never this helper.
pub(super) fn local_scalar_cty(ty: &ir::IrType) -> String {
    if let ir::IrType::UInt(Some(width)) | ir::IrType::SInt(Some(width)) = ty {
        if let Some(words) = wide_scalar_words(*width) {
            return format!("harc_rt::HarcWide<{words}>");
        }
    }
    match ty {
        ir::IrType::UInt(Some(w)) | ir::IrType::SInt(Some(w)) if *w > 64 => {
            "_harc_u128".to_string()
        }
        ir::IrType::SInt(_) => "int64_t".to_string(),
        ir::IrType::String => "const char*".to_string(),
        _ => "uint64_t".to_string(),
    }
}

/// Number of 32-bit storage words used by the shared wide scalar
/// representation. Keeping this boundary and sizing in one place prevents a
/// width-cast expression from disagreeing with its destination local type.
pub(super) fn wide_scalar_words(width: u32) -> Option<u32> {
    (width > 128).then(|| width.div_ceil(32))
}

/// Packed-bit width of a record field's element type — the declared
/// width (`Bool` → 1). Mirrors v1's `packed_width` for the scalar leaves
/// the record subset lowers; a nested-record element recurses into the
/// inner record's total width (v1 parity). Widthless uint/sint leaves reach
/// this helper as the explicit 64-bit width assigned by record lowering,
/// while builtin `int` arrives as `UInt(Some(32))`. `None` remains reserved
/// for leaves such as enums that have storage but no v1 packed layout. For a
/// `Vec<T, N>` field this is the per-element width `T`;
/// the caller multiplies by `N`.
fn field_packed_width(ty: &ir::IrType, records: &[ir::RecordSchema]) -> Option<usize> {
    match ty {
        ir::IrType::Bool => Some(1),
        ir::IrType::UInt(w) | ir::IrType::SInt(w) => w.map(|w| w as usize),
        ir::IrType::Record(rid) => record_packed_width(records.get(rid.index())?, records),
        ir::IrType::FixedVec { elem, len } => {
            field_packed_width(elem, records).map(|width| width * len)
        }
        _ => None,
    }
}

/// Total packed-bit width of a record (sum of every field's packed
/// width; a `Vec<T, N>` field contributes `N * width(T)`, a nested-record
/// field its own total). `None` when any field has no defined packed
/// width — the pack helpers are then skipped, exactly as v1's `try_fold`
/// over `packed_width` does. Recursion terminates: lowering rejects
/// record cycles (`check_no_record_cycles`).
fn record_packed_width(r: &ir::RecordSchema, records: &[ir::RecordSchema]) -> Option<usize> {
    r.fields.iter().try_fold(0usize, |acc, f| {
        let w = field_packed_width(&f.ty, records)?;
        Some(acc + w * f.vec_len.unwrap_or(1))
    })
}

pub(super) fn common_record_declaration(
    out: &mut String,
    r: &ir::RecordSchema,
    records: &[ir::RecordSchema],
) {
    record_members(out, r, records);
    writeln!(out, "bool operator==(const {0}& a, const {0}& b);", r.name).ok();
    writeln!(out, "bool operator!=(const {0}& a, const {0}& b);", r.name).ok();
    writeln!(out).ok();
    // These helpers are templates over the Verilated pin representation.
    // Keep them with the shared record declaration so both the common
    // runtime and capsule-local target responders can instantiate them.
    record_pack_helpers(out, r, records);
}

pub(super) fn common_record_definitions(out: &mut String, r: &ir::RecordSchema) {
    if r.fields.is_empty() {
        writeln!(
            out,
            "bool operator==(const {0}& a, const {0}& b) {{ (void)a; (void)b; return true; }}",
            r.name
        )
        .ok();
    } else {
        let eq = r
            .fields
            .iter()
            .map(|f| format!("a.{0} == b.{0}", f.name))
            .collect::<Vec<_>>()
            .join(" && ");
        writeln!(
            out,
            "bool operator==(const {0}& a, const {0}& b) {{ return {eq}; }}",
            r.name
        )
        .ok();
    }
    writeln!(
        out,
        "bool operator!=(const {0}& a, const {0}& b) {{ return !(a == b); }}",
        r.name
    )
    .ok();
    writeln!(out).ok();
}

fn record_members(out: &mut String, r: &ir::RecordSchema, records: &[ir::RecordSchema]) {
    writeln!(out, "struct {} {{", r.name).ok();
    for f in &r.fields {
        if let ir::IrType::Seq(elem) = &f.ty {
            let elem_cty = field_scalar_cty(elem);
            writeln!(out, "{INDENT}std::vector<{elem_cty}> {}{{}};", f.name).ok();
            continue;
        }
        // A nested-record field is a real C++ struct member (v1 parity):
        // `<Inner> field{};` value-initializes it, so it picks up the inner
        // struct's own member-initializer defaults. Copy / `==` / pack all
        // recurse through the inner struct's own operators/helpers. A
        // `Vec<Record, N>` field is `std::array<Inner, N>` whose `= {}`
        // value-initializes every element (each runs the inner struct's
        // member defaults) — `record_emit_order` already placed `Inner`
        // before this struct.
        if let ir::IrType::Record(rid) = f.ty {
            let inner = &records[rid.index()];
            if let Some(n) = f.vec_len {
                writeln!(
                    out,
                    "{INDENT}std::array<{}, {n}> {} = {{}};",
                    inner.name, f.name
                )
                .ok();
            } else {
                writeln!(out, "{INDENT}{} {}{{}};", inner.name, f.name).ok();
            }
            continue;
        }
        let cty = field_scalar_cty(&f.ty);
        if let Some(n) = f.vec_len {
            // `Vec<T, N>` field → `std::array<T, N>` member, zero-filled
            // (v1's `record_field_c_type` Vec branch + `{}` default).
            writeln!(out, "{INDENT}std::array<{cty}, {n}> {} = {{}};", f.name).ok();
            continue;
        }
        let init = match f.ty {
            ir::IrType::Bool => if f.default.is_some_and(|d| d != 0) {
                "true"
            } else {
                "false"
            }
            .to_string(),
            _ => f.default.unwrap_or(0).to_string(),
        };
        writeln!(out, "{INDENT}{cty} {} = {init};", f.name).ok();
    }
    writeln!(out, "}};").ok();
}

fn record_struct(out: &mut String, r: &ir::RecordSchema, records: &[ir::RecordSchema]) {
    record_members(out, r, records);
    if r.fields.is_empty() {
        writeln!(
            out,
            "inline bool operator==(const {0}& a, const {0}& b) {{ (void)a; (void)b; return true; }}",
            r.name
        )
        .ok();
    } else {
        let eq = r
            .fields
            .iter()
            .map(|f| format!("a.{0} == b.{0}", f.name))
            .collect::<Vec<_>>()
            .join(" && ");
        writeln!(
            out,
            "inline bool operator==(const {0}& a, const {0}& b) {{ return {eq}; }}",
            r.name
        )
        .ok();
    }
    writeln!(
        out,
        "inline bool operator!=(const {0}& a, const {0}& b) {{ return !(a == b); }}",
        r.name
    )
    .ok();
    writeln!(out).ok();
    record_pack_helpers(out, r, records);
}

/// Packed-bit slot width of a whole field (per-element width times a
/// `Vec` count, or a nested record's total). Used to advance the offset
/// between top-level fields and between nested-record sub-fields.
fn field_slot_width(f: &ir::RecordFieldSchema, records: &[ir::RecordSchema]) -> usize {
    field_packed_width(&f.ty, records).unwrap_or(0) * f.vec_len.unwrap_or(1)
}

/// Emit `harc_wide_write_bits` calls that pack one field VALUE at bit
/// `offset`, recursing through `Vec` elements and nested records so the
/// bit layout is byte-identical to v1's `emit_pack_bits`: a nested record
/// contributes its own fields (reverse declaration order) as a contiguous
/// sub-blob at `offset`.
fn emit_pack_field(
    out: &mut String,
    records: &[ir::RecordSchema],
    ty: &ir::IrType,
    vec_len: Option<usize>,
    value_expr: &str,
    offset: usize,
) {
    if let Some(n) = vec_len {
        let elem_w = field_packed_width(ty, records).unwrap_or(0);
        for i in 0..n {
            emit_pack_field(
                out,
                records,
                ty,
                None,
                &format!("{value_expr}[{i}]"),
                offset + i * elem_w,
            );
        }
        return;
    }
    if let ir::IrType::Record(rid) = ty {
        let inner = &records[rid.index()];
        let mut off = offset;
        for f in inner.fields.iter().rev() {
            emit_pack_field(
                out,
                records,
                &f.ty,
                f.vec_len,
                &format!("{value_expr}.{}", f.name),
                off,
            );
            off += field_slot_width(f, records);
        }
        return;
    }
    if let ir::IrType::FixedVec { elem, len } = ty {
        let elem_w = field_packed_width(elem, records).unwrap_or(0);
        for i in 0..*len {
            emit_pack_field(
                out,
                records,
                elem,
                None,
                &format!("{value_expr}[{i}]"),
                offset + i * elem_w,
            );
        }
        return;
    }
    let w = field_packed_width(ty, records).unwrap_or(0);
    writeln!(
        out,
        "{INDENT}harc_rt::harc_wide_write_bits(_packed, {offset}, {w}, {value_expr});"
    )
    .ok();
}

/// Emit assignments that unpack one field TARGET from bit `offset`,
/// mirroring `emit_pack_field`'s layout (reverse-order nested-record
/// sub-fields, `Vec` elements). Only reached in the bit-layout fallback
/// of the (template) `harc_unpack_*`; concrete correctness for a real
/// wire, and byte-identity with v1, follow from the shared offset walk.
fn emit_unpack_field(
    out: &mut String,
    records: &[ir::RecordSchema],
    ty: &ir::IrType,
    vec_len: Option<usize>,
    target_expr: &str,
    offset: usize,
) {
    if let Some(n) = vec_len {
        let elem_w = field_packed_width(ty, records).unwrap_or(0);
        for i in 0..n {
            emit_unpack_field(
                out,
                records,
                ty,
                None,
                &format!("{target_expr}[{i}]"),
                offset + i * elem_w,
            );
        }
        return;
    }
    if let ir::IrType::Record(rid) = ty {
        let inner = &records[rid.index()];
        let mut off = offset;
        for f in inner.fields.iter().rev() {
            emit_unpack_field(
                out,
                records,
                &f.ty,
                f.vec_len,
                &format!("{target_expr}.{}", f.name),
                off,
            );
            off += field_slot_width(f, records);
        }
        return;
    }
    if let ir::IrType::FixedVec { elem, len } = ty {
        let elem_w = field_packed_width(elem, records).unwrap_or(0);
        for i in 0..*len {
            emit_unpack_field(
                out,
                records,
                elem,
                None,
                &format!("{target_expr}[{i}]"),
                offset + i * elem_w,
            );
        }
        return;
    }
    let w = field_packed_width(ty, records).unwrap_or(0);
    let cty = field_scalar_cty(ty);
    let rhs = if w <= 64 {
        format!(
            "({cty})harc_rt::harc_bits(_packed, {}, {offset})",
            offset + w - 1
        )
    } else if w <= 128 {
        format!(
            "static_cast<_harc_u128>(harc_rt::harc_wide_extract_bits<4>(_packed, {offset}, {w}))"
        )
    } else {
        let words = w.div_ceil(32);
        format!("harc_rt::harc_wide_extract_bits<{words}>(_packed, {offset}, {w})")
    };
    writeln!(out, "{INDENT}{target_expr} = {rhs};").ok();
}

fn emit_structured_unpack_field(
    out: &mut String,
    records: &[ir::RecordSchema],
    ty: &ir::IrType,
    vec_len: Option<usize>,
    target_expr: &str,
    raw_expr: &str,
    depth: usize,
) {
    let pad = INDENT.repeat(depth);
    if let Some(n) = vec_len {
        for i in 0..n {
            emit_structured_unpack_field(
                out,
                records,
                ty,
                None,
                &format!("{target_expr}[{i}]"),
                &format!("{raw_expr}[{i}]"),
                depth,
            );
        }
        return;
    }
    if let ir::IrType::Record(rid) = ty {
        let inner = &records[rid.index()];
        writeln!(
            out,
            "{pad}{target_expr} = harc_unpack_{}({raw_expr});",
            inner.name
        )
        .ok();
        return;
    }
    if let ir::IrType::FixedVec { elem, len } = ty {
        for i in 0..*len {
            emit_structured_unpack_field(
                out,
                records,
                elem,
                None,
                &format!("{target_expr}[{i}]"),
                &format!("{raw_expr}[{i}]"),
                depth,
            );
        }
        return;
    }
    let rhs = match ty {
        ir::IrType::UInt(Some(w)) | ir::IrType::SInt(Some(w)) if *w > 128 => {
            let words = w.div_ceil(32);
            format!(
                "harc_rt::harc_wide_trunc<{words}>(harc_rt::harc_read({raw_expr}), {w})"
            )
        }
        ir::IrType::SInt(Some(w)) if *w <= 64 => format!(
            "static_cast<int64_t>(harc_rt::harc_sext_u128(static_cast<_harc_u128>(harc_rt::harc_read({raw_expr})), {w}, 64))"
        ),
        ir::IrType::UInt(Some(w)) | ir::IrType::SInt(Some(w)) => {
            let cty = field_scalar_cty(ty);
            format!(
                "static_cast<{cty}>(harc_rt::harc_trunc_u128(static_cast<_harc_u128>(harc_rt::harc_read({raw_expr})), {w}))"
            )
        }
        _ => {
            let cty = field_scalar_cty(ty);
            format!("static_cast<{cty}>(harc_rt::harc_read({raw_expr}))")
        }
    };
    writeln!(out, "{pad}{target_expr} = {rhs};").ok();
}

fn emit_structured_drive_field(
    out: &mut String,
    records: &[ir::RecordSchema],
    ty: &ir::IrType,
    vec_len: Option<usize>,
    sig_expr: &str,
    value_expr: &str,
    depth: usize,
) {
    let pad = INDENT.repeat(depth);
    if let Some(n) = vec_len {
        for i in 0..n {
            emit_structured_drive_field(
                out,
                records,
                ty,
                None,
                &format!("{sig_expr}[{i}]"),
                &format!("{value_expr}[{i}]"),
                depth,
            );
        }
        return;
    }
    if let ir::IrType::Record(rid) = ty {
        let inner = &records[rid.index()];
        writeln!(
            out,
            "{pad}harc_drive_{}({sig_expr}, {value_expr});",
            inner.name
        )
        .ok();
        return;
    }
    if let ir::IrType::FixedVec { elem, len } = ty {
        for i in 0..*len {
            emit_structured_drive_field(
                out,
                records,
                elem,
                None,
                &format!("{sig_expr}[{i}]"),
                &format!("{value_expr}[{i}]"),
                depth,
            );
        }
        return;
    }
    let normalized = match ty {
        ir::IrType::UInt(Some(w)) | ir::IrType::SInt(Some(w)) if *w > 128 => {
            format!("harc_rt::harc_wide_mask_bits({value_expr}, {w})")
        }
        ir::IrType::UInt(Some(w)) | ir::IrType::SInt(Some(w)) => {
            format!("harc_rt::harc_trunc_u128(static_cast<_harc_u128>({value_expr}), {w})")
        }
        _ => value_expr.to_string(),
    };
    writeln!(out, "{pad}harc_rt::harc_assign({sig_expr}, {normalized});").ok();
}

/// Emit `harc_pack_<R>` / `harc_unpack_<R>` / `harc_drive_<R>` for a
/// record that crosses a lowered TLM response pin — v1's
/// `emit_record_pack_helpers`. `harc_pack` lays each field into a
/// `HarcWide<words>` LSB-first in *reverse* declaration order (so the
/// first field occupies the high bits, matching the SV packed-struct
/// convention); `harc_unpack` / `harc_drive` carry a `requires`
/// fast-path that copies field-wise when the response pin is exposed as
/// a struct (Verilator packed struct), falling back to the bit layout
/// for a flat wide wire. Skipped when the record has no defined packed
/// width (for example, an enum field) — exactly v1's `try_fold` guard.
fn record_pack_helpers(out: &mut String, r: &ir::RecordSchema, records: &[ir::RecordSchema]) {
    let Some(width) = record_packed_width(r, records) else {
        return;
    };
    let name = &r.name;
    let words = width.div_ceil(32).max(1);

    // harc_pack: LSB-first, reverse declaration order. A nested-record
    // field packs its own sub-fields as a contiguous sub-blob (recursion in
    // `emit_pack_field`), bit-identical to v1's `emit_pack_bits`.
    writeln!(
        out,
        "static harc_rt::HarcWide<{words}> harc_pack_{name}(const {name}& value) {{"
    )
    .ok();
    writeln!(out, "{INDENT}harc_rt::HarcWide<{words}> _packed{{}};").ok();
    let mut offset = 0usize;
    for f in r.fields.iter().rev() {
        emit_pack_field(
            out,
            records,
            &f.ty,
            f.vec_len,
            &format!("value.{}", f.name),
            offset,
        );
        offset += field_slot_width(f, records);
    }
    writeln!(out, "{INDENT}return _packed;").ok();
    writeln!(out, "}}").ok();

    // harc_unpack: struct-shaped pin fast-path, else bit layout.
    writeln!(
        out,
        "template<typename Raw> static {name} harc_unpack_{name}(const Raw& raw) {{"
    )
    .ok();
    let raw_checks: Vec<String> = r.fields.iter().map(|f| format!("raw.{}", f.name)).collect();
    if !raw_checks.is_empty() {
        writeln!(
            out,
            "{INDENT}if constexpr (requires {{ {}; }}) {{",
            raw_checks.join("; ")
        )
        .ok();
        writeln!(out, "{0}{0}{name} value{{}};", INDENT).ok();
        for f in &r.fields {
            emit_structured_unpack_field(
                out,
                records,
                &f.ty,
                f.vec_len,
                &format!("value.{}", f.name),
                &format!("raw.{}", f.name),
                2,
            );
        }
        writeln!(out, "{0}{0}return value;", INDENT).ok();
        writeln!(out, "{INDENT}}} else {{").ok();
    }
    writeln!(
        out,
        "{INDENT}auto _packed = harc_rt::harc_wide_zext<{words}>(harc_rt::harc_read(raw));"
    )
    .ok();
    writeln!(out, "{INDENT}{name} value{{}};").ok();
    let mut offset = 0usize;
    for f in r.fields.iter().rev() {
        emit_unpack_field(
            out,
            records,
            &f.ty,
            f.vec_len,
            &format!("value.{}", f.name),
            offset,
        );
        offset += field_slot_width(f, records);
    }
    writeln!(out, "{INDENT}return value;").ok();
    if !raw_checks.is_empty() {
        writeln!(out, "{INDENT}}}").ok();
    }
    writeln!(out, "}}").ok();

    // harc_drive: struct-shaped sig fast-path, else pack-and-assign.
    writeln!(
        out,
        "template<typename Sig> static void harc_drive_{name}(Sig& sig, const {name}& value) {{"
    )
    .ok();
    if !raw_checks.is_empty() {
        let sig_checks: Vec<String> = raw_checks
            .iter()
            .map(|s| s.replacen("raw.", "sig.", 1))
            .collect();
        writeln!(
            out,
            "{INDENT}if constexpr (requires {{ {}; }}) {{",
            sig_checks.join("; ")
        )
        .ok();
        for f in &r.fields {
            emit_structured_drive_field(
                out,
                records,
                &f.ty,
                f.vec_len,
                &format!("sig.{}", f.name),
                &format!("value.{}", f.name),
                2,
            );
        }
        writeln!(out, "{INDENT}}} else {{").ok();
        writeln!(
            out,
            "{0}{0}harc_rt::harc_assign(sig, harc_pack_{name}(value));",
            INDENT
        )
        .ok();
        writeln!(out, "{INDENT}}}").ok();
    } else {
        writeln!(
            out,
            "{INDENT}harc_rt::harc_assign(sig, harc_pack_{name}(value));"
        )
        .ok();
    }
    writeln!(out, "}}").ok();
    writeln!(out).ok();
}

/// Dependency order for component-struct emission: a component appears
/// after every component it holds as a by-value `Sub` field, so the held
/// struct is already defined. DFS post-order over the `Sub` edges, in
/// id order for determinism (mirrors v1's `topo_sort_component_indices`).
/// The IR rejects sub-component cycles at lowering (a by-value cycle is
/// not constructible), so the visited-set DFS terminates.
/// Dependency order for record-struct emission: a record appears after
/// every record it NESTS as a by-value field, so the inner struct's
/// definition (and its `harc_pack_*`) precede the outer one — C++ requires
/// a complete inner type before it can be a member. DFS post-order over
/// `IrType::Record` field edges, in `RecordId` (declaration) order for
/// determinism. Lowering rejects record cycles (`check_no_record_cycles`),
/// so the visited-set DFS terminates.
fn record_emit_order(records: &[ir::RecordSchema]) -> Vec<usize> {
    let n = records.len();
    let mut order = Vec::with_capacity(n);
    let mut visited = vec![false; n];
    fn visit(i: usize, records: &[ir::RecordSchema], visited: &mut [bool], order: &mut Vec<usize>) {
        if visited[i] {
            return;
        }
        visited[i] = true;
        for f in &records[i].fields {
            if let ir::IrType::Record(rid) = f.ty {
                visit(rid.index(), records, visited, order);
            }
        }
        order.push(i);
    }
    for i in 0..n {
        visit(i, records, &mut visited, &mut order);
    }
    order
}

fn component_emit_order(prog: &TbProgram) -> Vec<usize> {
    let n = prog.components.len();
    let mut order = Vec::with_capacity(n);
    let mut visited = vec![false; n];
    fn visit(i: usize, prog: &TbProgram, visited: &mut [bool], order: &mut Vec<usize>) {
        if visited[i] {
            return;
        }
        visited[i] = true;
        for f in &prog.components[i].fields {
            if let ir::ComponentFieldKind::Sub { component, .. } = &f.kind {
                visit(component.index(), prog, visited, order);
            }
        }
        order.push(i);
    }
    for i in 0..n {
        visit(i, prog, &mut visited, &mut order);
    }
    order
}

fn edge_is_enabled(
    prog: &TbProgram,
    owner: ir::ComponentId,
    inherited: Option<ir::ComponentInstanceMode>,
    edge: &ir::ConnectEdgeSchema,
) -> bool {
    let source_mode =
        ir::resolve_component_path_mode(&prog.components, owner, inherited, &edge.src_path)
            .expect("verified component connect source path")
            .effective_mode;
    let sink_mode =
        ir::resolve_component_path_mode(&prog.components, owner, inherited, &edge.sink_path)
            .expect("verified component connect sink path")
            .effective_mode;
    ir::component_connect_modes_enabled(source_mode, sink_mode, edge)
}

fn persistent_setup_capture(run_context: Option<&str>) -> String {
    run_context
        .map(|context| {
            format!("_harc_callback_state = &_harc_run_state, _harc_callback_context = &{context}")
        })
        .unwrap_or_else(|| "&".to_string())
}

fn emit_persistent_setup_bindings(out: &mut String, depth: usize, run_context: Option<&str>) {
    if run_context.is_none() {
        return;
    }
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    writeln!(out, "{pad}auto& _harc_run_state = *_harc_callback_state;").ok();
    writeln!(out, "{pad}auto& ctx = *_harc_callback_context;").ok();
    writeln!(out, "{pad}auto* dut = ctx.dut;").ok();
    writeln!(out, "{pad}auto& errors = ctx.errors;").ok();
    writeln!(out, "{pad}auto& _fatal = ctx.fatal;").ok();
    writeln!(out, "{pad}auto& cycle_count = ctx.cycle_count;").ok();
    writeln!(out, "{pad}auto& trace = ctx.trace;").ok();
    writeln!(out, "{pad}auto& log_ctx = ctx.log_ctx;").ok();
    writeln!(out, "{pad}auto& _checkers = ctx._checkers;").ok();
    writeln!(
        out,
        "{pad}auto& _post_eval_services = ctx._post_eval_services;"
    )
    .ok();
    writeln!(out, "{pad}auto& _auto_cov_reports = ctx._auto_cov_reports;").ok();
    writeln!(out, "{pad}auto& harc_rng = ctx.rng;").ok();
    writeln!(
        out,
        "{pad}auto sim_log_line = [&](const char* sev, const char* fmt, ...) {{"
    )
    .ok();
    writeln!(out, "{pad1}va_list ap;").ok();
    writeln!(out, "{pad1}va_start(ap, fmt);").ok();
    writeln!(
        out,
        "{pad1}harc_rt::log::harc_log_vline(log_ctx.sim_log, &trace, cycle_count, sev, fmt, ap);"
    )
    .ok();
    writeln!(out, "{pad1}va_end(ap);").ok();
    writeln!(out, "{pad}}};").ok();
}

/// Register every `on <ev>(arg)` handler on `component` (and nested
/// sub-components) as a subscriber closure on the corresponding event
/// field of the instance reached by `inst_path`. The closure bumps the
/// owning instance's `_last_in_cycle` activity stamp, then runs the
/// handler body lambda — mirroring v1's `on`-subscriber registration.
fn emit_on_handler_regs(
    out: &mut String,
    prog: &TbProgram,
    component: ir::ComponentId,
    inst_path: &str,
    // When true, the top component's OWN `on <ev>` handlers are NOT
    // registered as synchronous subscribers — they were re-lowered into a
    // queue-fed worker-coroutine actor (`emit_active_bound_driver_actor`)
    // under `--mt`, which replaces the synchronous driver. Nested
    // sub-components are still registered normally. Always `false` on the
    // cooperative-default path, so default output is unchanged.
    skip_top_on_handlers: bool,
    mode: Option<ir::ComponentInstanceMode>,
    bound_bus: Option<&ir::BusBindingSchema>,
    bus_adapters: Option<&[expr::TestbenchBusAdapterPlan]>,
    run_context: Option<&str>,
) -> Result<(), EmitError> {
    let comp = &prog.components[component.index()];
    let input_heartbeat = runtime::component_heartbeat_field(
        comp,
        ir::passes::runtime_cells::ComponentHeartbeat::Input,
    );
    if !skip_top_on_handlers {
        for oh in &comp.on_handlers {
            if !ir::component_mode_includes_activation(mode, oh.activation) {
                continue;
            }
            let lambda = func::on_handler_lambda_name(comp, oh);
            let call = component_callable_call(
                prog,
                oh.function,
                &lambda,
                inst_path,
                &["_t"],
                bound_bus,
                bus_adapters,
                run_context,
            )?;
            if run_context.is_some() {
                let capture = persistent_setup_capture(run_context);
                writeln!(
                    out,
                    "{INDENT}{inst_path}.{}.push_back([{capture}](auto _t) {{",
                    oh.event
                )
                .ok();
                emit_persistent_setup_bindings(out, 2, run_context);
                writeln!(
                    out,
                    "{INDENT}{INDENT}{inst_path}.{input_heartbeat} = (uint64_t)cycle_count;"
                )
                .ok();
                writeln!(out, "{INDENT}{INDENT}{call};").ok();
                writeln!(out, "{INDENT}}});").ok();
            } else {
                writeln!(
                    out,
                    "{INDENT}{inst_path}.{}.push_back([&](auto _t) {{ {inst_path}.{input_heartbeat} = (uint64_t)cycle_count; {call}; }});",
                    oh.event
                )
                .ok();
            }
        }
    }
    // Recurse into by-value sub-components (an env holding an agent).
    for f in &comp.fields {
        if let ir::ComponentFieldKind::Sub { component: sub, .. } = &f.kind {
            let sub_path = format!("{inst_path}.{}", f.name);
            emit_on_handler_regs(
                out,
                prog,
                *sub,
                &sub_path,
                false,
                ir::resolve_component_path_mode(
                    &prog.components,
                    component,
                    mode,
                    std::slice::from_ref(&f.name),
                )
                .expect("verified nested component path")
                .effective_mode,
                bound_bus,
                bus_adapters,
                run_context,
            )?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn component_callable_call(
    prog: &TbProgram,
    function: ir::FunctionId,
    symbol: &str,
    receiver: &str,
    tail_args: &[&str],
    bound_bus: Option<&ir::BusBindingSchema>,
    bus_adapters: Option<&[expr::TestbenchBusAdapterPlan]>,
    run_context: Option<&str>,
) -> Result<String, EmitError> {
    let mut args = Vec::new();
    if let Some(context) = run_context {
        args.push(context.to_string());
    }
    args.push(receiver.to_string());
    if let Some(adapters) = bus_adapters {
        let dut_receiver = run_context
            .map(|context| format!("{context}.dut"))
            .unwrap_or_else(|| "dut".to_string());
        args.extend(func::component_callable_bus_adapter_args(
            prog,
            function,
            adapters,
            bound_bus,
            &dut_receiver,
            symbol,
        )?);
    }
    args.extend(tail_args.iter().map(|arg| (*arg).to_string()));
    Ok(format!("{symbol}({})", args.join(", ")))
}

/// Emit one resolved `connect` edge's subscriber push_back, rooted at
/// `inst_path` (the instance reaching the connect's owning component).
fn emit_one_connect(
    out: &mut String,
    prog: &TbProgram,
    inst_path: &str,
    edge: &ir::ConnectEdgeSchema,
    run_context: Option<&str>,
) {
    let prefix = (!inst_path.is_empty()).then(|| inst_path.to_string());
    let src = prefix
        .iter()
        .cloned()
        .chain(edge.src_path.iter().cloned())
        .collect::<Vec<_>>()
        .join(".");
    let sink = prefix
        .iter()
        .cloned()
        .chain(edge.sink_path.iter().cloned())
        .collect::<Vec<_>>()
        .join(".");
    match &edge.sink {
        ir::ConnectSink::Method { method } => {
            let sink_comp = &prog.components[edge.sink_component.index()].name;
            let call = run_context
                .map(|context| format!("{sink_comp}_{method}({context}, {sink}, _t)"))
                .unwrap_or_else(|| format!("{sink_comp}_{method}({sink}, _t)"));
            if run_context.is_some() {
                let capture = persistent_setup_capture(run_context);
                writeln!(
                    out,
                    "{INDENT}{src}.{}.push_back([{capture}](auto _t) {{",
                    edge.src_event
                )
                .ok();
                emit_persistent_setup_bindings(out, 2, run_context);
                writeln!(out, "{INDENT}{INDENT}{call};").ok();
                writeln!(out, "{INDENT}}});").ok();
            } else {
                writeln!(
                    out,
                    "{INDENT}{src}.{}.push_back([&](auto _t) {{ {call}; }});",
                    edge.src_event
                )
                .ok();
            }
        }
        ir::ConnectSink::Event { event } => {
            // event→event bridge: forward each emit on the source event into
            // the sink event's own subscriber list, firing the sink driver's
            // registered `on <ev>` handler(s). Mirrors v1's
            // `for (auto& _s : <sink>.<event>) _s(_t);` bridge closure.
            if run_context.is_some() {
                let capture = persistent_setup_capture(run_context);
                writeln!(
                    out,
                    "{INDENT}{src}.{}.push_back([{capture}](auto _t) {{",
                    edge.src_event
                )
                .ok();
                emit_persistent_setup_bindings(out, 2, run_context);
                writeln!(
                    out,
                    "{INDENT}{INDENT}for (auto& _s : {sink}.{event}) _s(_t);"
                )
                .ok();
                writeln!(out, "{INDENT}}});").ok();
            } else {
                writeln!(
                    out,
                    "{INDENT}{src}.{}.push_back([&](auto _t) {{ for (auto& _s : {sink}.{event}) _s(_t); }});",
                    edge.src_event
                )
                .ok();
            }
        }
    }
}

/// Install the `connect` bridges of every by-value sub-component of
/// `component` (reached via `inst_path`), recursing depth-first. Used for
/// an env that holds an agent: the agent's own
/// `sequencer.dispatched -> drv.req` bridge lives on the agent's schema and
/// must be installed at `<env>.<agent>` scope. The top component's OWN
/// connects are emitted by the caller (`cf.connects`); this only walks the
/// nested sub-components.
fn emit_nested_connects(
    out: &mut String,
    prog: &TbProgram,
    component: ir::ComponentId,
    inst_path: &str,
    mode: Option<ir::ComponentInstanceMode>,
    run_context: Option<&str>,
) {
    let comp = &prog.components[component.index()];
    for f in &comp.fields {
        if let ir::ComponentFieldKind::Sub { component: sub, .. } = &f.kind {
            let sub_path = format!("{inst_path}.{}", f.name);
            let sub_comp = &prog.components[sub.index()];
            let sub_mode = ir::resolve_component_path_mode(
                &prog.components,
                component,
                mode,
                std::slice::from_ref(&f.name),
            )
            .expect("verified nested component path")
            .effective_mode;
            for edge in &sub_comp.connects {
                if edge_is_enabled(prog, *sub, sub_mode, edge) {
                    emit_one_connect(out, prog, &sub_path, edge, run_context);
                }
            }
            emit_nested_connects(out, prog, *sub, &sub_path, sub_mode, run_context);
        }
    }
}

/// Default watchdog clause values (spec §8.6), applied when the source
/// omits the `period`/`max_idle` clause. Mirror v1's
/// `WATCHDOG_DEFAULT_PERIOD` / `WATCHDOG_DEFAULT_MAX_IDLE`.
const WATCHDOG_DEFAULT_PERIOD: i64 = 1000;
const WATCHDOG_DEFAULT_MAX_IDLE: i64 = 10000;

/// Install the `_checkers` closures for a component's `on <N> cycles`
/// periodic handlers and its `watchdog` (and those of any nested
/// sub-component), each gated on a per-instance last-fire stamp.
/// Mirrors v1's `emit_watchdog_checker` / periodic `_checkers` shape:
/// every cycle the closure re-reads the period (so a field-backed period
/// stays test-overridable), fires once it is due, and — for the watchdog
/// — runs the user body then the idle check.
fn emit_lifecycle_checkers(
    out: &mut String,
    prog: &TbProgram,
    runtime_cells: &ir::passes::runtime_cells::RuntimeCellPlan,
    component: ir::ComponentId,
    inst_path: &str,
    mode: Option<ir::ComponentInstanceMode>,
    dut_type: &str,
    dut_access: Option<&ir::passes::dut_access::DutAccessPlan>,
    dut_lane_widths: &std::collections::HashMap<String, u32>,
    bound_bus: Option<&ir::BusBindingSchema>,
    bus_adapters: Option<&[expr::TestbenchBusAdapterPlan]>,
    run_context: Option<&str>,
) -> Result<(), EmitError> {
    let comp = &prog.components[component.index()];
    let callback_capture = persistent_setup_capture(run_context);
    let periodic_member_base = comp.methods.len() + comp.on_handlers.len();
    let cycle_member_base = periodic_member_base + comp.periodic_handlers.len();
    // A valid C++ identifier for the static tag (`env.agent` → `env_agent`).
    let inst_tag = inst_path.replace('.', "_");

    for (periodic_index, ph) in comp.periodic_handlers.iter().enumerate() {
        if !ir::component_mode_includes_activation(mode, ph.activation) {
            continue;
        }
        let lambda = func::periodic_handler_lambda_name(comp, ph);
        let period = func::clause_count_cpp(
            prog,
            ph.function,
            inst_path,
            dut_type,
            dut_access,
            dut_lane_widths,
            bound_bus,
            &ph.period,
        )?;
        let tag = format!("_per_{inst_tag}_{}", ph.function.0);
        let field = runtime::component_runtime_cell_field(
            runtime_cells,
            component,
            comp,
            &ir::passes::runtime_cells::RuntimeCellKind::ComponentPeriodicLast {
                member: ir::ComponentCallableId((periodic_member_base + periodic_index) as u32),
            },
        )?;
        let last = format!("{inst_path}.{field}");
        let svc = ph.phase.service_vec();
        writeln!(out, "{INDENT}{svc}.push_back([{callback_capture}]() {{").ok();
        emit_persistent_setup_bindings(out, 2, run_context);
        writeln!(
            out,
            "{INDENT}{INDENT}int64_t {tag}_period = (int64_t)({period});"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}{INDENT}if ({tag}_period > 0 && (int64_t)cycle_count - {last} >= {tag}_period) {{"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}{last} = (int64_t)cycle_count;"
        )
        .ok();
        let call = component_callable_call(
            prog,
            ph.function,
            &lambda,
            inst_path,
            &[],
            bound_bus,
            bus_adapters,
            run_context,
        )?;
        writeln!(out, "{INDENT}{INDENT}{INDENT}{call};").ok();
        writeln!(out, "{INDENT}{INDENT}}}").ok();
        writeln!(out, "{INDENT}}});").ok();
    }

    // Cycle-trigger handlers (`on <bool-expr>`). Each installs a
    // selected cycle-service closure that re-evaluates the trigger predicate every
    // primary-clock cycle and fires the body when the predicate satisfies
    // the requested edge mode. Mirrors v1's `emit_cycle_trigger`.
    for (cycle_index, ch) in comp.cycle_handlers.iter().enumerate() {
        if !ir::component_mode_includes_activation(mode, ch.activation) {
            continue;
        }
        let lambda = func::cycle_handler_lambda_name(comp, ch);
        let trigger = func::clause_predicate_cpp(
            prog,
            ch.function,
            inst_path,
            dut_type,
            dut_access,
            dut_lane_widths,
            bound_bus,
            &ch.trigger,
        )?;
        let tag = format!("_cyc_{inst_tag}_{}", ch.function.0);
        let svc = ch.phase.service_vec();
        if ch.monitor_channel.is_some() {
            // Bound-bus handshake monitor (v1's `emit_bound_monitor_actors`).
            // v1 lowers it as a coroutine: `while (true) { co_await
            // wait_until(valid && ready); <capture+body>; co_await
            // wait_cycles(1); }`. Because the post-body `wait_cycles(1)`
            // re-parks in `wait_until` (which never resumes same-tick), v1
            // captures one beat, then SKIPS exactly the next cycle before
            // re-arming — so a continuously-held handshake samples every
            // OTHER cycle (e.g. held over cycles 5,6,7 → beats at 5 and 7),
            // NOT every cycle (Level) and NOT only the rising edge.
            //
            // The selected service pass runs once per primary cycle at the same
            // phase the monitor coroutine would resume (`sched.tick()` then
            // checkers), so the cadence is reproduced exactly with a
            // fire-then-cooldown latch: fire when the predicate holds, then
            // consume the following cycle as the `wait_cycles(1)` re-arm.
            //
            // NOTE on `--mt`: the monitor stays a cooperative `_checkers`
            // latch even under `--mt`, deliberately NOT a worker coroutine.
            // v1 can run the monitor on its own OS thread only because v1
            // ALSO re-lowers the active bound *driver* transactor into a
            // queue-fed worker coroutine that yields (`co_await
            // wait_cycles`) every cycle — so the handshake is established
            // and observed inside the same barrier window. tbir keeps the
            // driver in the run coroutine (synchronous `tick()` spins that
            // never reach the main-loop barrier window), so a worker-thread
            // monitor would miss every handshake. Re-lowering active
            // transactors into actors is a separate, larger change (see
            // issue #425 / the WS2 follow-up). Keeping the monitor as the
            // cycle-service latch is trace-correct under both `--mt` and the
            // default — the latch already fires at the right phases on
            // every `tick()`.
            writeln!(out, "{INDENT}{svc}.push_back([{callback_capture}]() {{").ok();
            emit_persistent_setup_bindings(out, 2, run_context);
            let field = runtime::component_runtime_cell_field(
                runtime_cells,
                component,
                comp,
                &ir::passes::runtime_cells::RuntimeCellKind::ComponentCooldown {
                    member: ir::ComponentCallableId((cycle_member_base + cycle_index) as u32),
                },
            )?;
            let cooldown = format!("{inst_path}.{field}");
            writeln!(out, "{INDENT}{INDENT}if ({cooldown}) {{").ok();
            writeln!(out, "{INDENT}{INDENT}{INDENT}{cooldown} = false;").ok();
            writeln!(out, "{INDENT}{INDENT}}} else if ((bool)({trigger})) {{").ok();
            let call = component_callable_call(
                prog,
                ch.function,
                &lambda,
                inst_path,
                &[],
                bound_bus,
                bus_adapters,
                run_context,
            )?;
            writeln!(out, "{INDENT}{INDENT}{INDENT}{call};").ok();
            writeln!(out, "{INDENT}{INDENT}{INDENT}{cooldown} = true;").ok();
            writeln!(out, "{INDENT}{INDENT}}}").ok();
            writeln!(out, "{INDENT}}});").ok();
            continue;
        }
        writeln!(out, "{INDENT}{svc}.push_back([{callback_capture}]() {{").ok();
        emit_persistent_setup_bindings(out, 2, run_context);
        match ch.edge {
            ir::CycleEdge::Level => {
                writeln!(out, "{INDENT}{INDENT}if ((bool)({trigger})) {{").ok();
                let call = component_callable_call(
                    prog,
                    ch.function,
                    &lambda,
                    inst_path,
                    &[],
                    bound_bus,
                    bus_adapters,
                    run_context,
                )?;
                writeln!(out, "{INDENT}{INDENT}{INDENT}{call};").ok();
                writeln!(out, "{INDENT}{INDENT}}}").ok();
            }
            ir::CycleEdge::Rising | ir::CycleEdge::Falling => {
                let field = runtime::component_runtime_cell_field(
                    runtime_cells,
                    component,
                    comp,
                    &ir::passes::runtime_cells::RuntimeCellKind::ComponentEdgePrevious {
                        member: ir::ComponentCallableId((cycle_member_base + cycle_index) as u32),
                    },
                )?;
                let previous = format!("{inst_path}.{field}");
                writeln!(out, "{INDENT}{INDENT}bool {tag}_curr = (bool)({trigger});").ok();
                let cond = match ch.edge {
                    ir::CycleEdge::Rising => format!("!{previous} && {tag}_curr"),
                    ir::CycleEdge::Falling => format!("{previous} && !{tag}_curr"),
                    ir::CycleEdge::Level => unreachable!(),
                };
                writeln!(out, "{INDENT}{INDENT}if ({cond}) {{").ok();
                let call = component_callable_call(
                    prog,
                    ch.function,
                    &lambda,
                    inst_path,
                    &[],
                    bound_bus,
                    bus_adapters,
                    run_context,
                )?;
                writeln!(out, "{INDENT}{INDENT}{INDENT}{call};").ok();
                writeln!(out, "{INDENT}{INDENT}}}").ok();
                writeln!(out, "{INDENT}{INDENT}{previous} = {tag}_curr;").ok();
            }
        }
        writeln!(out, "{INDENT}}});").ok();
    }

    if let Some(w) = &comp.watchdog {
        if !ir::component_mode_includes_activation(mode, w.activation) {
            // A passive instance has no active-only watchdog registration.
        } else {
            let lambda = func::watchdog_lambda_name(comp, w);
            let period = match &w.period {
                Some(e) => func::clause_count_cpp(
                    prog,
                    w.function,
                    inst_path,
                    dut_type,
                    dut_access,
                    dut_lane_widths,
                    bound_bus,
                    e,
                )?,
                None => WATCHDOG_DEFAULT_PERIOD.to_string(),
            };
            let max_idle = match &w.max_idle {
                Some(e) => func::clause_count_cpp(
                    prog,
                    w.function,
                    inst_path,
                    dut_type,
                    dut_access,
                    dut_lane_widths,
                    bound_bus,
                    e,
                )?,
                None => WATCHDOG_DEFAULT_MAX_IDLE.to_string(),
            };
            let tag = format!("_wdog_{inst_tag}_{}", w.function.0);
            let input_heartbeat = runtime::component_heartbeat_field(
                comp,
                ir::passes::runtime_cells::ComponentHeartbeat::Input,
            );
            let output_heartbeat = runtime::component_heartbeat_field(
                comp,
                ir::passes::runtime_cells::ComponentHeartbeat::Output,
            );
            let field = runtime::component_runtime_cell_field(
                runtime_cells,
                component,
                comp,
                &ir::passes::runtime_cells::RuntimeCellKind::ComponentWatchdogLast {
                    member: ir::ComponentCallableId(
                        (cycle_member_base + comp.cycle_handlers.len()) as u32,
                    ),
                },
            )?;
            let last = format!("{inst_path}.{field}");
            writeln!(out, "{INDENT}_checkers.push_back([{callback_capture}]() {{").ok();
            emit_persistent_setup_bindings(out, 2, run_context);
            writeln!(
                out,
                "{INDENT}{INDENT}int64_t {tag}_period = (int64_t)({period});"
            )
            .ok();
            writeln!(
            out,
            "{INDENT}{INDENT}if ({tag}_period > 0 && (int64_t)cycle_count - {last} >= {tag}_period) {{"
        )
        .ok();
            writeln!(
                out,
                "{INDENT}{INDENT}{INDENT}{last} = (int64_t)cycle_count;"
            )
            .ok();
            // 1. User body (typically a heartbeat log).
            let call = component_callable_call(
                prog,
                w.function,
                &lambda,
                inst_path,
                &[],
                bound_bus,
                bus_adapters,
                run_context,
            )?;
            writeln!(out, "{INDENT}{INDENT}{INDENT}{call};").ok();
            // 2. Idle check — trips FAIL when BOTH activity stamps are
            //    `max_idle` cycles behind. Mirrors v1's emit_watchdog idle
            //    block (framework error-counter bump on trip).
            writeln!(
                out,
                "{INDENT}{INDENT}{INDENT}int64_t {tag}_max_idle = (int64_t)({max_idle});"
            )
            .ok();
            writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}if ({tag}_max_idle > 0 \
             && (int64_t)((uint64_t)cycle_count - {inst_path}.{input_heartbeat}) >= {tag}_max_idle \
             && (int64_t)((uint64_t)cycle_count - {inst_path}.{output_heartbeat}) >= {tag}_max_idle) {{"
        )
        .ok();
            writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}{INDENT}sim_log_line(\"FAIL\", \"watchdog: {} has been idle for >= %lld cycles\", (long long){tag}_max_idle);",
            comp.name
        )
        .ok();
            writeln!(out, "{INDENT}{INDENT}{INDENT}{INDENT}ctx.errors++;").ok();
            writeln!(out, "{INDENT}{INDENT}{INDENT}}}").ok();
            writeln!(out, "{INDENT}{INDENT}}}").ok();
            writeln!(out, "{INDENT}}});").ok();
        }
    }

    // Recurse into by-value sub-components (an env holding an agent that
    // carries a watchdog / periodic handler).
    for f in &comp.fields {
        if let ir::ComponentFieldKind::Sub { component: sub, .. } = &f.kind {
            let sub_path = format!("{inst_path}.{}", f.name);
            emit_lifecycle_checkers(
                out,
                prog,
                runtime_cells,
                *sub,
                &sub_path,
                ir::resolve_component_path_mode(
                    &prog.components,
                    component,
                    mode,
                    std::slice::from_ref(&f.name),
                )
                .expect("verified nested component path")
                .effective_mode,
                dut_type,
                dut_access,
                dut_lane_widths,
                None,
                bus_adapters,
                run_context,
            )?;
        }
    }
    Ok(())
}

fn emit_tb_periodic_services(
    out: &mut String,
    tb: &ir::TestbenchSchema,
    runtime_cells: expr::RuntimeCellRenderBinding<'_>,
    run_context: Option<&str>,
) -> Result<(), EmitError> {
    let callback_capture = persistent_setup_capture(run_context);
    for svc in &tb.periodic_services {
        let lambda = runtime_cells.test_hook(svc.function)?;
        let kind = ir::passes::runtime_cells::RuntimeCellKind::TestbenchPeriodicLast {
            function: svc.function,
            phase: svc.phase.into(),
        };
        let last = runtime_cells.testbench_field(&kind)?;
        let period = svc.period;
        let vec = svc.phase.service_vec();
        writeln!(out, "{INDENT}{vec}.push_back([{callback_capture}]() {{").ok();
        emit_persistent_setup_bindings(out, 2, run_context);
        writeln!(
            out,
            "{INDENT}{INDENT}if ((int64_t)cycle_count - {last} >= {period}) {{"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}{last} = (int64_t)cycle_count;"
        )
        .ok();
        writeln!(out, "{INDENT}{INDENT}{INDENT}{lambda}();").ok();
        writeln!(out, "{INDENT}{INDENT}}}").ok();
        writeln!(out, "{INDENT}}});").ok();
    }
    Ok(())
}

fn emit_tb_cycle_services(
    out: &mut String,
    prog: &TbProgram,
    tb: &ir::TestbenchSchema,
    dut_type: &str,
    dut_access: Option<&ir::passes::dut_access::DutAccessPlan>,
    dut_lane_widths: &std::collections::HashMap<String, u32>,
    runtime_cells: expr::RuntimeCellRenderBinding<'_>,
    run_context: Option<&str>,
) -> Result<(), EmitError> {
    let callback_capture = persistent_setup_capture(run_context);
    for svc in &tb.cycle_services {
        let lambda = runtime_cells.test_hook(svc.function)?;
        let trigger = func::tb_service_expr_cpp(
            prog,
            svc.function,
            dut_type,
            dut_access,
            dut_lane_widths,
            &svc.trigger,
        )?;
        let tag = format!("_tbcyc_{}", svc.function.0);
        let vec = svc.phase.service_vec();
        // Re-evaluate the predicate every primary-clock cycle and fire the
        // body per the recorded edge mode. Mirrors v1's `emit_cycle_trigger`
        // and the transactor cycle-trigger closure in `emit_lifecycle_checkers`.
        writeln!(out, "{INDENT}{vec}.push_back([{callback_capture}]() {{").ok();
        emit_persistent_setup_bindings(out, 2, run_context);
        match svc.edge {
            ir::CycleEdge::Level => {
                writeln!(out, "{INDENT}{INDENT}if ((bool)({trigger})) {{").ok();
                writeln!(out, "{INDENT}{INDENT}{INDENT}{lambda}();").ok();
                writeln!(out, "{INDENT}{INDENT}}}").ok();
            }
            ir::CycleEdge::Rising | ir::CycleEdge::Falling => {
                let kind = ir::passes::runtime_cells::RuntimeCellKind::TestbenchEdgePrevious {
                    function: svc.function,
                    phase: svc.phase.into(),
                };
                let previous = runtime_cells.testbench_field(&kind)?;
                writeln!(out, "{INDENT}{INDENT}bool {tag}_curr = (bool)({trigger});").ok();
                let cond = match svc.edge {
                    ir::CycleEdge::Rising => format!("!{previous} && {tag}_curr"),
                    ir::CycleEdge::Falling => format!("{previous} && !{tag}_curr"),
                    ir::CycleEdge::Level => unreachable!(),
                };
                writeln!(out, "{INDENT}{INDENT}if ({cond}) {{").ok();
                writeln!(out, "{INDENT}{INDENT}{INDENT}{lambda}();").ok();
                writeln!(out, "{INDENT}{INDENT}}}").ok();
                writeln!(out, "{INDENT}{INDENT}{previous} = {tag}_curr;").ok();
            }
        }
        writeln!(out, "{INDENT}}});").ok();
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_test(
    out: &mut String,
    prog: &TbProgram,
    test: &ir::TestSchema,
    dut_type: &str,
    opts: &EmitOpts,
    randomize_snippets: &[String],
    runtime_cells: &ir::passes::runtime_cells::RuntimeCellPlan,
    dut_access: Option<&ir::passes::dut_access::DutAccessPlan>,
    // Native lifecycle functions emitted out-of-line for this layout.
    outofline_lifecycle: &HashMap<ir::FunctionId, func::LifecycleEmit>,
) -> Result<(), EmitError> {
    let tb = prog.testbench(test.testbench);
    let clocked = !test.clocks.is_empty();
    let qualified_clock_wait = prog.functions.iter().any(|function| {
        function
            .blocks
            .iter()
            .any(|block| matches!(block.terminator, ir::Terminator::WaitCycles(_, Some(_), _)))
    });
    let cosim = opts.cosim.is_some();
    // Declared clocks work under co-sim without a dedicated lowering:
    // the clocked scheduler's `dut-><clk> = level` writes go through the
    // DPI setter (real SV edges) and its `dut->eval()` calls map to
    // bridge settles, so the simulator sees the correct edge SEQUENCE.
    // Physical time is compressed (1 ps per edge instead of the declared
    // period) — fine for cycle-based tests; delay-dependent DUTs need
    // the future timing-faithful clocked lowering.
    let mt = opts.mt;
    // `(sched_var, slot_var)` pairs for actors that run on a dedicated
    // OS worker thread under `--mt`. Empty in cooperative mode (actor
    // slots go into the global `sched` instead). Collected as actors are
    // emitted; consumed by the worker-spawn / barrier dance below.
    let mut actor_threads: Vec<(String, String)> = Vec::new();

    let runtime_cell_stem = format!("t{}", test.id.0);
    let runtime_cells_type = runtime::test_runtime_cells_struct(
        out,
        prog,
        runtime_cells,
        test,
        &runtime_cell_stem,
        &opts.vec_lane_widths,
        dut_type,
        dut_access,
        None,
    )?;
    runtime::run_prologue(out, &test.name, dut_type, cosim);
    if clocked {
        runtime::clocked_scheduler(out, &test.clocks);
    } else {
        runtime::clockless_scheduler(out);
    }
    runtime::log_helpers_and_seed(out, qualified_clock_wait);
    let mut outer_names = tb
        .component_fields
        .iter()
        .map(|binding| binding.field.clone())
        .chain(
            tb.regblock_bindings
                .iter()
                .map(|binding| binding.field.clone()),
        )
        .collect::<HashSet<_>>();
    if needs_tb_struct(tb) {
        outer_names.insert("_tb".to_string());
    }
    let mut runtime_cells_name = "_harc_runtime_cells".to_string();
    while outer_names.contains(&runtime_cells_name) {
        runtime_cells_name = format!("_u_{runtime_cells_name}");
    }
    if let Some(runtime_cells_type) = &runtime_cells_type {
        writeln!(
            out,
            "{INDENT}{runtime_cells_type} {runtime_cells_name}{{}};"
        )
        .ok();
    }
    let runtime_cell_binding =
        runtime_cells_type
            .as_ref()
            .map(|_| expr::RuntimeCellRenderBinding {
                plan: runtime_cells,
                test,
                receiver: &runtime_cells_name,
            });

    if needs_tb_struct(tb) {
        writeln!(out, "{INDENT}{} _tb;", tb.name).ok();
    }
    // Regblock mirrors are explicit per-run state shared by every callable in
    // the owning test. Callback-bearing bindings additionally own one
    // recursion-depth counter.
    for b in &tb.regblock_bindings {
        let mirror_ty = &prog.records[prog.regblocks[b.regblock.index()].record.index()].name;
        writeln!(out, "{INDENT}{mirror_ty} {}{{}};", b.field).ok();
        if !b.callbacks.is_empty() {
            writeln!(out, "{INDENT}uint32_t {}_cb_depth = 0;", b.field).ok();
        }
    }
    // Composite-component test-scope instances (`let env : AnalysisEnv`,
    // `sb : ScoreboardWithMethods`) are shared by the run coroutine,
    // hook-triggered covergroup sampler registration, connect closures,
    // and lifecycle services. Declare them before covergroup hook
    // registration so a trigger like `@(sb.observe(t) post)` can push
    // onto `sb._harc_cov_observe_post`.
    for cf in &tb.component_fields {
        let cname = &prog.components[cf.component.index()].name;
        writeln!(out, "{INDENT}{cname} {};", cf.field).ok();
        emit_component_dut_bindings(out, prog, cf.component, &cf.field, dut_type, "dut", 1)?;
    }
    // Hook-triggered covergroups subscribe to receiver-owned vectors. Declare
    // every transactor receiver before registering any sampler so the
    // registration never depends on testbench field order.
    let mut emitted_state_ty = HashSet::new();
    for actor in &tb.unbound_state_actors {
        if !emitted_state_ty.insert(actor.transactor) {
            continue;
        }
        runtime::unbound_state_struct_decl(
            out,
            prog,
            actor.transactor,
            prog.transactor(actor.transactor),
            &prog.records,
            runtime_cells,
        )?;
    }
    for actor in &tb.unbound_state_actors {
        runtime::unbound_state_var(out, prog, actor.transactor, &actor.storage);
    }
    // Covergroup auto-sampler registration, in testbench-field
    // declaration order — the same `_checkers` slot v1 uses, so
    // sampling happens at the identical point in the cycle. Lowering
    // synthesized one SamplerAuto function per cov field; cross-check
    // the pairing so a lowering drift fails loudly here.
    let samplers: Vec<&ir::TbFunction> = prog
        .functions
        .iter()
        .filter(|f| {
            f.owner == Some(test.testbench)
                && matches!(f.kind, ir::FunctionKind::SamplerAuto { .. })
        })
        .collect();
    if samplers.len() != tb.cov_fields.len() {
        return Err(EmitError(format!(
            "tbir: test `{}` has {} cov field(s) but {} SamplerAuto function(s)",
            test.name,
            tb.cov_fields.len(),
            samplers.len()
        )));
    }
    for ((field, cg), sampler) in tb.cov_fields.iter().zip(&samplers) {
        let ir::FunctionKind::SamplerAuto { covgroup } = &sampler.kind else {
            unreachable!("filtered to SamplerAuto above");
        };
        if covgroup != cg {
            return Err(EmitError(format!(
                "tbir: sampler `{}` is bound to cg{} but field `{field}` expects cg{}",
                sampler.name, covgroup.0, cg.0
            )));
        }
        let schema = &prog.covgroups[cg.index()];
        match &schema.trigger {
            ir::CovTrigger::PosedgeDutClk => {
                covergroup::sampler_registration(
                    out,
                    prog,
                    schema,
                    &format!("_tb.{field}"),
                    &opts.vec_lane_widths,
                    &opts.dut_port_widths,
                    dut_access,
                )?;
            }
            ir::CovTrigger::Hook {
                receiver_path,
                method,
                side,
                ..
            } => {
                // Resolve the target method (the `covergroup_hooks` pass
                // already validated it) to learn its param signature, then
                // push the sample closure onto `<Type>_<method>_<side>`.
                let [receiver] = receiver_path.as_slice() else {
                    return Err(EmitError(format!(
                        "tbir: hook-triggered covergroup `{}` has nested receiver path `{}`",
                        schema.name,
                        receiver_path.join(".")
                    )));
                };
                let side_name = match side {
                    crate::ast::HookSide::Pre => "pre",
                    crate::ast::HookSide::Post => "post",
                };
                if let Some(binding) = tb
                    .component_fields
                    .iter()
                    .find(|binding| binding.field == *receiver)
                {
                    let component = &prog.components[binding.component.index()];
                    if let Some(target_method) = component.method(method) {
                        if !ir::component_mode_includes_activation(
                            binding.mode,
                            target_method.activation,
                        ) {
                            return Err(EmitError(format!(
                                "tbir: hook-triggered covergroup `{}` targets active-only \
                                 method `{receiver}.{method}` through passive component binding",
                                schema.name
                            )));
                        }
                    }
                }
                let target = tb
                    .transactor_fields
                    .iter()
                    .find_map(|(field, xid)| {
                        if field != receiver {
                            return None;
                        }
                        let xs = prog.transactor(*xid);
                        xs.method(method).and_then(|m| {
                            tb.unbound_state_actors
                                .iter()
                                .find(|actor| actor.field == *field && actor.transactor == *xid)
                                .map(|actor| {
                                    (
                                        format!(
                                            "{}.{}",
                                            actor.storage,
                                            runtime::transactor_coverage_hook_field(
                                                xs, &m.name, side_name,
                                            )
                                        ),
                                        m.function,
                                        m.param_names.len(),
                                    )
                                })
                        })
                    })
                    .or_else(|| {
                        tb.component_fields.iter().find_map(|binding| {
                            if binding.field != *receiver {
                                return None;
                            }
                            let comp = &prog.components[binding.component.index()];
                            comp.method(method).map(|m| {
                                (
                                    format!("{}._harc_cov_{}_{}", binding.field, m.name, side_name),
                                    m.function,
                                    m.param_names.len(),
                                )
                            })
                        })
                    })
                    .ok_or_else(|| {
                        EmitError(format!(
                            "tbir: hook-triggered covergroup `{}` references method \
                             `{receiver}.{method}` not found on any transactor/component field",
                            schema.name
                        ))
                    })?;
                covergroup::hook_sampler_registration(
                    out,
                    prog,
                    schema,
                    target.0,
                    target.1,
                    target.2,
                    &format!("_tb.{field}"),
                    &opts.vec_lane_widths,
                    &opts.dut_port_widths,
                    dut_access,
                )?;
            }
        }
    }
    // tseq generator lambdas — one `<Name>` per `tseq` declaration in the
    // file, declared before the run coroutine so its `[&]` capture sees
    // them (v1's `emit_tseq` placement). Each returns a
    // `std::vector<Record>` and runs the body's randomize/yield loop.
    for function in func::tseq_emit_order(prog)? {
        func::emit_tseq(
            out,
            prog,
            prog.function(function),
            &prog.records,
            randomize_snippets,
            1,
        )?;
    }
    // Transactor method lambdas — one `<Type>_<method>` per method of
    // every transactor the testbench instantiates, declared before the
    // run coroutine so its `[&]` capture sees them (v1 emission order).
    // Two fields of the same transactor type share one lambda set: the
    // subset carries no per-instance state (the DUT bind is static).
    // Per-instance state for the unbound DUT-poking transactors that carry
    // persistent state (`drv.last_read`). Declared BEFORE the method
    // lambdas so the lambdas' `[&]` capture binds the instance structs
    // (passed in as `self_state`) and the run/check coroutine reads them.
    //
    // State-receiver ABI (#494 P1b): one SHARED `_<Type>_state` struct type
    // per transactor type, then one storage VARIABLE per instance. A
    // demand-created stateless heartbeat object uses the schema's generated
    // storage symbol rather than its potentially-colliding source field. The
    // type-shared method lambda takes the receiver by reference, so any
    // number of active instances of one type coexist with independent
    // state (`Drv_go(a)` and `Drv_go(b)` mutate `a`/`b` separately).
    let mut emitted_xactors = HashSet::new();
    for (_, xid) in &tb.transactor_fields {
        if !emitted_xactors.insert(*xid) {
            continue;
        }
        // A passive-only transactor type still exposes its always-on methods.
        // Skip only `when active` methods: #763 permits calls to always-on
        // hookable methods through a passive instance, so omitting the whole
        // lambda set leaves those valid call sites dangling.
        let has_active_instance = tb
            .transactor_fields
            .iter()
            .any(|(f, x)| x == xid && !tb.passive_transactor_fields.contains(f));
        let schema = prog.transactor(*xid);
        let bound_bus = tb
            .bound_bus_binding(ir::BoundBusOwner::Transactor(*xid))
            .map_err(|detail| EmitError(format!("tbir: test `{}`: {detail}", test.name)))?;
        // On a passive-only type, emit only the always-on methods.
        for m in &schema.methods {
            if !has_active_instance && m.active_only {
                continue;
            }
            func::declare_method_slot(out, prog, *xid, schema, m, 1)?;
        }
        for m in &schema.methods {
            if !has_active_instance && m.active_only {
                continue;
            }
            func::emit_method(
                out,
                prog,
                test.testbench,
                *xid,
                schema,
                m,
                bound_bus,
                randomize_snippets,
                1,
            )?;
        }
    }
    // Bound-to target-side TLM responder instances: one per-instance
    // state struct (state fields + activity stamps), declared in the
    // test scope so both the run/check coroutine (`target.read_count`)
    // and the actor coroutines capture it by reference, then one
    // background-coroutine actor per target method.
    for actor in &tb.target_tlm_actors {
        if actor.host_component.is_some() {
            continue;
        }
        let schema = prog.transactor(actor.transactor);
        runtime::target_state_struct_inst(
            out,
            prog,
            actor.transactor,
            schema,
            &actor.instance,
            &prog.records,
            runtime_cells,
        )?;
    }
    for actor in &tb.target_tlm_actors {
        func::emit_target_actor(
            out,
            prog,
            actor,
            &tb.bus_bindings,
            mt,
            &mut actor_threads,
            None,
            1,
        )?;
    }

    // Ordinary component methods use the verified callable graph's
    // dependency-first order, so forward and cross-component calls see their
    // callees before lambda initialization. Lifecycle handlers remain below.
    for function in func::component_method_emit_order(prog)? {
        let body = prog.functions.get(function.index()).ok_or_else(|| {
            EmitError(format!(
                "tbir: component method order references missing fn{}",
                function.0
            ))
        })?;
        let ir::FunctionKind::ComponentMethod {
            component, member, ..
        } = body.kind
        else {
            return Err(EmitError(format!(
                "tbir: component method order references fn{} with kind {:?}",
                function.0, body.kind
            )));
        };
        let comp = prog.components.get(component.index()).ok_or_else(|| {
            EmitError(format!(
                "tbir: component method fn{} references missing component c{}",
                function.0, component.0
            ))
        })?;
        let method = comp
            .methods
            .get(member.index())
            .filter(|method| method.function == function)
            .ok_or_else(|| {
                EmitError(format!(
                    "tbir: component `{}` does not own method fn{} at member {}",
                    comp.name, function.0, member.0
                ))
            })?;
        let bound_bus = tb
            .bound_bus_binding(ir::BoundBusOwner::Component(component))
            .map_err(|detail| EmitError(format!("tbir: test `{}`: {detail}", test.name)))?;
        func::emit_component_method(
            out,
            prog,
            test.testbench,
            component,
            comp,
            method,
            bound_bus,
            dut_type,
            dut_access,
            &opts.vec_lane_widths,
            randomize_snippets,
            1,
        )?;
    }
    for ci in component_emit_order(prog) {
        let comp = &prog.components[ci];
        let bound_bus = tb
            .bound_bus_binding(ir::BoundBusOwner::Component(ir::ComponentId(ci as u32)))
            .map_err(|detail| EmitError(format!("tbir: test `{}`: {detail}", test.name)))?;
        for oh in &comp.on_handlers {
            func::emit_component_on_handler(
                out,
                prog,
                comp,
                oh,
                bound_bus,
                dut_type,
                dut_access,
                &opts.vec_lane_widths,
                randomize_snippets,
                1,
            )?;
        }
        for ph in &comp.periodic_handlers {
            func::emit_component_periodic_handler(
                out,
                prog,
                comp,
                ph,
                bound_bus,
                dut_type,
                dut_access,
                &opts.vec_lane_widths,
                randomize_snippets,
                1,
            )?;
        }
        for ch in &comp.cycle_handlers {
            func::emit_component_cycle_handler(
                out,
                prog,
                comp,
                ch,
                bound_bus,
                dut_type,
                dut_access,
                &opts.vec_lane_widths,
                randomize_snippets,
                1,
            )?;
        }
        if let Some(w) = &comp.watchdog {
            func::emit_component_watchdog(
                out,
                prog,
                comp,
                w,
                bound_bus,
                dut_type,
                dut_access,
                &opts.vec_lane_widths,
                randomize_snippets,
                1,
            )?;
        }
    }
    for function in func::testbench_method_emit_order(prog, tb.type_id)? {
        func::emit_testbench_method(
            out,
            prog,
            prog.function(function),
            test.testbench,
            &tb.bus_bindings,
            dut_type,
            dut_access,
            &opts.vec_lane_widths,
            randomize_snippets,
            1,
        )?;
    }
    // Closure-hook bodies (`on <obj>.<method> pre/post` method hooks and
    // `on regs.REG` per-register write callbacks) are free `[&]`-capturing
    // lambdas. Emit them only after every callable transactor/component
    // method has been declared: a valid hook body may call another method,
    // and C++ name lookup requires that callable to exist first. Their
    // subscription/dispatch sites live later in the run/check coroutine.
    for f in &prog.functions {
        if runtime::test_hook_belongs_to_test(f, test) {
            func::emit_test_hook(
                out,
                prog,
                f,
                dut_type,
                1,
                func::TestHookRenderBindings {
                    flow: func::FlowRenderBindings {
                        dut_receiver: Some("dut"),
                        dut_access,
                        dut_lane_widths: Some(&opts.vec_lane_widths),
                        clocks: Some(&test.clocks),
                        ..func::FlowRenderBindings::default()
                    },
                    runtime_cells: runtime_cell_binding,
                    common_contextual_tseqs: None,
                    durable_capture: false,
                },
            )?;
        }
    }
    // Composite-component connection and lifecycle setup for the instances
    // declared above. Keep this after component method lambdas so connect
    // closures can call `<SinkComp>_<method>(...)`.
    for edge in &tb.connects {
        emit_one_connect(out, prog, "", edge, None);
    }
    for cf in &tb.component_fields {
        // The top component's own `connect` edges (an env's source→sink, or
        // an agent instantiated directly at test scope), plus any nested
        // sub-component's connects (an env holding an agent whose own
        // `sequencer.dispatched -> drv.req` bridge must be installed).
        for edge in &cf.connects {
            if edge_is_enabled(prog, cf.component, cf.mode, edge) {
                emit_one_connect(out, prog, &cf.field, edge, None);
            }
        }
        emit_nested_connects(out, prog, cf.component, &cf.field, cf.mode, None);
        // An `active` bound event-driven transactor (`let drv : X active =
        // bind axil`) re-lowers its `on <ev>` driver into a queue-fed
        // worker-coroutine actor on its own `ThreadScheduler` under `--mt`,
        // mirroring v1's `try_emit_bound_driver_actor`: a pusher subscriber
        // makes `emit drv.req(t)` ENQUEUE (non-blocking), and a worker
        // coroutine drains the queue and drives the bus, yielding
        // (`co_await wait_cycles`) each cycle so it shares the per-posedge
        // barrier window with the bound monitor — which is exactly what lets
        // the cooperative `_checkers` monitor latch observe every handshake
        // under `--mt` (the gap #448 deferred).
        //
        // The synchronous on-handler subscriber (`emit_on_handler_regs`) is
        // SUPPRESSED for this instance under `--mt`: the worker coroutine IS
        // the driver now. (v1 leaves a second synchronous subscriber
        // registered too, but its `tick()`-spinning body would drive the bus
        // a SECOND time, and the tbir cooperative `_checkers` monitor latch
        // would then count each handshake twice — 10 writes instead of 5.
        // v1's monitor is itself a worker coroutine whose `wait_cycles(1)`
        // cadence happens to mask the redundant second drive; the tbir latch
        // does not, so emitting both double-counts. Running the driver as the
        // single concurrent worker is the correct execution model and yields
        // the right per-codegen verdict.) Cooperative default emits neither
        // queue nor worker and keeps the synchronous subscriber —
        // byte-identical output.
        let bound_drv = &prog.components[cf.component.index()];
        let bound_bus = tb
            .bound_bus_binding(ir::BoundBusOwner::Component(cf.component))
            .map_err(|detail| EmitError(format!("tbir: test `{}`: {detail}", test.name)))?;
        let relower_driver = mt
            && matches!(cf.mode, Some(ir::ComponentInstanceMode::Active))
            && bound_drv.bound_bus.is_some()
            && !bound_drv.on_handlers.is_empty();
        if relower_driver {
            func::emit_active_bound_driver_actor(
                out,
                prog,
                cf.component,
                &cf.field,
                dut_type,
                &tb.bus_bindings,
                bound_bus,
                &mut actor_threads,
                1,
            )?;
        }
        // `on <ev>(arg)` handler registrations, for this component and any
        // nested sub-components (an env holding an agent). Each subscribes
        // to the event field on its owning instance, bumps the instance's
        // `_last_in_cycle` activity stamp, then runs the handler body —
        // mirroring v1's `on`-subscriber registration. Suppressed for the
        // top component when its driver was re-lowered into a worker actor
        // above (the worker replaces the synchronous driver under `--mt`).
        emit_on_handler_regs(
            out,
            prog,
            cf.component,
            &cf.field,
            relower_driver,
            cf.mode,
            bound_bus,
            None,
            None,
        )?;
        // `on <N> cycles` periodic + `watchdog` lifecycle `_checkers`
        // closures, for this component and any nested sub-components.
        // Bound-bus handshake monitors stay cooperative `_checkers`
        // latches even under `--mt` (see the NOTE in
        // `emit_lifecycle_checkers`), so no actor registration is needed.
        emit_lifecycle_checkers(
            out,
            prog,
            runtime_cells,
            cf.component,
            &cf.field,
            cf.mode,
            dut_type,
            dut_access,
            &opts.vec_lane_widths,
            bound_bus,
            None,
            None,
        )?;
    }

    // Testbench-scoped `on <N> cycles [phase post_eval]` periodic services
    // (issue #485). Each registers a `_checkers` / `_post_eval_services`
    // closure that fires the handler's free lambda once every `period`
    // primary-clock cycles,
    // gated on a per-service last-fire stamp — the flow-scope analogue of
    // a component's `emit_lifecycle_checkers` periodic registration.
    if !tb.periodic_services.is_empty() || !tb.cycle_services.is_empty() {
        let runtime_cell_binding = runtime_cell_binding.ok_or_else(|| {
            EmitError(format!(
                "tbir: test `{}` has lifecycle services without runtime-cell storage",
                test.name
            ))
        })?;
        emit_tb_periodic_services(out, tb, runtime_cell_binding, None)?;
        emit_tb_cycle_services(
            out,
            prog,
            tb,
            dut_type,
            dut_access,
            &opts.vec_lane_widths,
            runtime_cell_binding,
            None,
        )?;
    }

    if qualified_clock_wait {
        writeln!(out, "{INDENT}_harc_advance_actors = [&]() {{").ok();
        if mt {
            for (scheduler, _) in &actor_threads {
                writeln!(out, "{INDENT}{INDENT}{scheduler}.tick();").ok();
            }
        } else {
            writeln!(
                out,
                "{INDENT}{INDENT}sched.tick_except(_harc_running_slot);"
            )
            .ok();
        }
        writeln!(out, "{INDENT}}};").ok();
    }
    writeln!(out, "{INDENT}harc_rt::ThreadSlot _run_slot;").ok();
    if qualified_clock_wait {
        writeln!(out, "{INDENT}_harc_running_slot = &_run_slot;").ok();
    }
    writeln!(out, "{INDENT}sched.slots.push_back(&_run_slot);").ok();
    writeln!(
        out,
        "{INDENT}auto _run_slot_lambda = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{"
    )
    .ok();
    if !tb.synthetic {
        writeln!(out, "{INDENT}{INDENT}_tb.dut = dut;").ok();
    }
    let run = prog.function(test.run);
    let run_hook_captures: HashSet<ir::LocalId> =
        ir::passes::runtime_cells::persistent_callback_captures(prog, run)
            .map_err(|error| EmitError(format!("tbir: {error}")))?
            .into_iter()
            .collect();
    func::emit_function(
        out,
        prog,
        run,
        &prog.records,
        &tb.bus_bindings,
        &opts.vec_lane_widths,
        randomize_snippets,
        dut_type,
        &run_hook_captures,
        2,
        runtime_cell_binding,
        func::FlowRenderBindings {
            dut_receiver: Some("dut"),
            dut_access,
            dut_lane_widths: Some(&opts.vec_lane_widths),
            clocks: Some(&test.clocks),
            ..func::FlowRenderBindings::default()
        },
        outofline_lifecycle,
    )?;
    if let Some(check) = test.check {
        let check = prog.function(check);
        let check_callback_captures: HashSet<ir::LocalId> =
            ir::passes::runtime_cells::persistent_callback_captures(prog, check)
                .map_err(|error| EmitError(format!("tbir: {error}")))?
                .into_iter()
                .collect();
        func::emit_function(
            out,
            prog,
            check,
            &prog.records,
            &tb.bus_bindings,
            &opts.vec_lane_widths,
            randomize_snippets,
            dut_type,
            &check_callback_captures,
            2,
            runtime_cell_binding,
            func::FlowRenderBindings {
                dut_receiver: Some("dut"),
                dut_access,
                dut_lane_widths: Some(&opts.vec_lane_widths),
                clocks: Some(&test.clocks),
                ..func::FlowRenderBindings::default()
            },
            outofline_lifecycle,
        )?;
    }
    writeln!(out, "{INDENT}{INDENT}co_return;").ok();
    writeln!(out, "{INDENT}}};").ok();
    writeln!(
        out,
        "{INDENT}_run_slot.thread = _run_slot_lambda(&_run_slot);"
    )
    .ok();

    // Bootstrap (single-threaded) → spawn workers (`--mt` only) → drive
    // loop → shutdown workers. Ordering is load-bearing: per-actor
    // schedulers must be bootstrapped before their OS threads start, and
    // the shutdown handshake must follow the loop. The worker-setup /
    // shutdown emitters are no-ops when `actor_threads` is empty, so the
    // cooperative single-thread output stays byte-identical to before.
    runtime::drive_bootstrap(out, &actor_threads);
    runtime::mt_worker_setup(out, &actor_threads, qualified_clock_wait);
    runtime::drive_loop(out, clocked, &actor_threads, qualified_clock_wait);
    runtime::mt_worker_shutdown(out, &actor_threads);
    let covers: Vec<(ir::CoverCheckId, &ir::CoverCheckSchema)> = test
        .cover_checks
        .iter()
        .map(|cover| (*cover, &prog.cover_checks[cover.index()]))
        .collect();
    runtime::run_epilogue(out, cosim, &covers, &actor_threads, runtime_cell_binding)?;
    Ok(())
}
