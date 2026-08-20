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

mod covergroup;
mod expr;
mod func;
mod runtime;

use crate::ast::SourceFile;
use crate::codegen::cpp_tb::{EmitError, EmitOpts, GeneratedCppFile, SplitCppOutput};
use crate::ir::{self, TbProgram};
use std::collections::HashSet;
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
    !tb.synthetic || !tb.state_fields.is_empty()
}

/// Whether this testbench installs a user hook on this transactor method.
/// Both test-scope and statement-position registrations are represented by
/// `MethodHookSubscribe`; restricting the scan to `owner` prevents a shared
/// transactor schema from leaking hook vectors between tests.
pub(super) fn has_transactor_method_hook_subscription(
    prog: &TbProgram,
    owner: ir::TestbenchId,
    transactor: ir::TransactorId,
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
                            target: ir::MethodHookTarget::Transactor {
                                transactor: target,
                                method: target_method,
                                ..
                            },
                            ..
                        } if *target == transactor && target_method == method
                    )
                })
            })
        })
}

/// Component counterpart of `has_transactor_method_hook_subscription`.
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
struct SuiteScaffold {
    /// From `tests[0]`'s testbench, but `validate_tests_share_dut` proves
    /// every test agrees, so the string is the same for any test subset.
    dut_type: String,
    // The three fields below are suite-invariant BY DESIGN — do NOT narrow
    // them to the selected tests (harc#538). Each reads an unfiltered
    // program table, so today every shard is emitted with identical bytes,
    // including shards whose own tests use no probe and no `randomize`.
    has_probes: bool,
    problem_table_cpp: String,
    randomize_snippets: Vec<String>,
}

impl SuiteScaffold {
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

        // `generate_if`-gated bus signals: lowering kept every binding's gates
        // intact but could not evaluate them (no param env). Resolve each
        // ACCESSED bus-bound signal's gate against the effective param env now
        // (the emitter has `EmitOpts` + the `SourceFile`), erroring on a
        // gated-OFF access exactly as v1's `bus_signal_present` / gated-OFF
        // diagnostic does. A gated-OFF signal that is never accessed is silent.
        check_gated_bus_access(prog, file, opts)?;

        // Constraint-solver wiring (randomize sites). The runtime problem
        // table + per-site Z3-solve snippets are emitted by v1's shared
        // constraint codegen ("only the call site moves to the IR backend").
        // Empty when the program has no randomize site — the TB then never
        // links Z3, exactly like v1.
        let problem_table_cpp = if prog.constraint_sites.is_empty() {
            String::new()
        } else {
            let solver_table = crate::solver::problem_table::build_typed_solver_problem_table(file);
            let runtime_table =
                crate::solver::runtime::RuntimeProblemTable::from_typed_solver_table(&solver_table);
            if runtime_table.problems.is_empty() {
                String::new()
            } else {
                runtime_table.render_cpp_table("_harc_runtime_random_problem_table")
            }
        };
        // Per-`ConstraintRef` Z3-solve snippets, emitted at the loop-switch
        // body depth (run/check fn = depth 2 → block stmts at depth 5).
        let randomize_snippets =
            crate::codegen::cpp_tb::emit_randomize_snippets(file, opts, &prog.constraint_sites, 5)?;

        // Probe reads/forces dereference `dut->rootp->...`, which needs the
        // root struct's full definition (`V<Top>___024root.h`) — the `rootp`
        // member in `V<Top>.h` is only a forward-declared pointer. Mirrors
        // v1's `aggregated_probes` include gate. See docs/probe-signals.md.
        let has_probes = program_has_probes(prog);
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
            problem_table_cpp,
            randomize_snippets,
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
        scaffold.has_probes,
        opts.cosim.as_ref(),
    );

    // Transaction value-record structs, in declaration order. Mirrors
    // v1's `emit_record_struct` shape (field defaults as member
    // initializers, `operator==`/`!=`). v1's other record companions
    // — `randomize_<T>` and the pack/unpack helpers — are NOT emitted:
    // every construct that could reach them (`randomize`, bus sends)
    // is rejected at lowering, so they would be dead text here. They
    // land with their constructs.
    // Emit in TOPOLOGICAL order (a record after every record it nests) so
    // an inner struct's definition and `harc_pack_*` precede any outer
    // struct that holds it by value — C++ needs the complete inner type.
    for i in record_emit_order(&prog.records) {
        record_struct(&mut out, &prog.records[i], &prog.records);
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
    for sb in &prog.scoreboards {
        runtime::scoreboard_struct(&mut out, sb, &prog.records);
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
            &prog.components[ci],
            &prog.components,
            &prog.scoreboards,
            &prog.records,
        );
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
        func::emit_helper_prototype(&mut out, h);
    }
    if !helpers.is_empty() {
        writeln!(out).ok();
    }
    crate::codegen::cpp_tb::emit_extern_fn_decls(&mut out, file);
    // Concurrent-`cover` hit counters. File scope so the end-of-test
    // summary — emitted in the test's `run_*` function, outside the run
    // coroutine that registers the witness closure — can read them.
    if !prog.cover_checks.is_empty() {
        for c in &prog.cover_checks {
            writeln!(
                &mut out,
                "static uint64_t {} = 0;",
                func::cover_counter_name(&c.tag)
            )
            .ok();
        }
        writeln!(out).ok();
    }
    // Covergroup structs are leaf observables, but hook-triggered sampler
    // bodies may call pure helpers or extern reference functions, so their
    // forward declarations must be visible before the struct definition.
    for cg in &prog.covgroups {
        covergroup::covgroup_struct(&mut out, cg);
    }
    for h in &helpers {
        func::emit_helper_function(&mut out, h)?;
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
    for tb in &prog.testbenches {
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
                &tb.name,
                dut_type,
                &cov_fields,
                &tb.state_fields,
                &sb_fields,
                &prog.records,
            );
        }
    }
    runtime::context_struct(&mut out, dut_type);

    for &i in test_indices {
        emit_test(
            &mut out,
            prog,
            &prog.tests[i],
            dut_type,
            opts,
            &scaffold.randomize_snippets,
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
    if prog
        .functions
        .iter()
        .any(|f| f.blocks.iter().any(|b| b.stmts.iter().any(stmt_has_probe)))
    {
        return true;
    }
    let mut found = false;
    for_each_check_body_expr(prog, |e| found |= expr_has_probe(e));
    found
}

fn port_is_probe(p: &ir::PortRef) -> bool {
    !matches!(p.access, ir::PortAccess::Port)
}

fn expr_has_probe(e: &ir::Expr) -> bool {
    use ir::Expr::*;
    match e {
        Port(p) => port_is_probe(p),
        Binary(_, a, b) => expr_has_probe(a) || expr_has_probe(b),
        Unary(_, a) => expr_has_probe(a),
        BitSlice { target, .. } => expr_has_probe(target),
        BitSliceDyn { target, hi, lo } => {
            expr_has_probe(target) || expr_has_probe(hi) || expr_has_probe(lo)
        }
        Ternary(c, a, b) => expr_has_probe(c) || expr_has_probe(a) || expr_has_probe(b),
        WidthCast { inner, .. } => expr_has_probe(inner),
        Call(_, args) => args.iter().any(expr_has_probe),
        ComponentIdle { n, .. } => expr_has_probe(n),
        ComponentVecElement { index, .. } => expr_has_probe(index),
        _ => false,
    }
}

fn fmt_has_probe(args: &ir::FmtArgs) -> bool {
    args.args.iter().any(|a| expr_has_probe(&a.expr))
}

fn stmt_has_probe(s: &ir::Stmt) -> bool {
    use ir::Stmt::*;
    match s {
        DutWrite(p, e) => port_is_probe(p) || expr_has_probe(e),
        DutRead(_, p) | ProbeRelease(p) => port_is_probe(p),
        Assign(_, e)
        | RecordFieldWrite { value: e, .. }
        | RecordWriteCb { value: e, .. }
        | TbFieldWrite { value: e, .. }
        | TbQueuePush { value: e, .. }
        | TransactorStateWrite { value: e, .. }
        | TransactorStateRecordFieldWrite { value: e, .. }
        | ComponentFieldWrite { value: e, .. }
        | TransactorCall { call: e, .. }
        | TransactorSelfCall { call: e, .. } => expr_has_probe(e),
        ComponentVecElementWrite { index, value, .. } => {
            expr_has_probe(index) || expr_has_probe(value)
        }
        AssertCheck { cond, on_fail } | AssumeCheck { cond, on_fail } => {
            expr_has_probe(cond) || fmt_has_probe(on_fail)
        }
        Log { args, .. } => fmt_has_probe(args),
        FailDiag { guard, args } => {
            guard.as_ref().is_some_and(expr_has_probe) || fmt_has_probe(args)
        }
        ScoreboardOp { op, .. } => match op {
            ir::ScoreboardOp::QueuePush { value, .. }
            | ir::ScoreboardOp::ScalarWrite { value, .. } => expr_has_probe(value),
            ir::ScoreboardOp::QueuePop { .. } => false,
        },
        ComponentEmit { args, .. } | ComponentCall { args, .. } => args.iter().any(expr_has_probe),
        SeqPush { value, .. }
        | ComponentQueuePush { value, .. }
        | TransactorStateQueuePush { value, .. } => expr_has_probe(value),
        ComponentQueuePop { .. }
        | ComponentSubAssign { .. }
        | TransactorStateQueuePop { .. }
        | TbQueuePop { .. } => false,
        TlmFork(desc) => desc.args.iter().any(expr_has_probe),
        TlmJoinAll(pending) => pending.iter().any(|p| p.args.iter().any(expr_has_probe)),
        // The check BODY is a program-level schema, not a statement
        // operand — `program_has_probes` walks `property_checks` /
        // `cover_checks` directly so a probe read inside a concurrent
        // property is still seen.
        EventEmit { args, .. } => args.iter().any(expr_has_probe),
        EventSubscribe { .. } | MethodHookSubscribe { .. } => false,
        PropertyCheck(_) | CoverCheck(_) | CycleHandler(_) => false,
        RecordInit(_, _) | CovReport(_) => false,
    }
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
        if let ir::CycleHandlerKind::Trigger { trigger, .. } = &h.kind {
            f(trigger);
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
fn check_gated_bus_access(
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
    use ir::Stmt::*;
    match s {
        DutWrite(p, e) => {
            f(p);
            for_each_port_in_expr(e, f);
        }
        DutRead(_, p) | ProbeRelease(p) => f(p),
        // `RecordFieldWrite` carries an optional element `index` (`rec.f[i]
        // = v`) — walk it too, so a gated DUT port nested in a write-index
        // is scanned (#454). Today a write-index hoists to a `DutRead`, so
        // this is defensive completeness; without it, any future inline
        // write-index would silently escape the gated-port scan.
        RecordFieldWrite {
            value,
            mid_indices,
            index,
            ..
        } => {
            for_each_port_in_expr(value, f);
            for (_, idx) in mid_indices {
                for_each_port_in_expr(idx, f);
            }
            if let Some(idx) = index {
                for_each_port_in_expr(idx, f);
            }
        }
        Assign(_, e)
        | RecordWriteCb { value: e, .. }
        | TbFieldWrite { value: e, .. }
        | TbQueuePush { value: e, .. }
        | TransactorStateWrite { value: e, .. }
        | TransactorStateRecordFieldWrite { value: e, .. }
        | ComponentFieldWrite { value: e, .. }
        | TransactorCall { call: e, .. }
        | TransactorSelfCall { call: e, .. } => for_each_port_in_expr(e, f),
        ComponentVecElementWrite { index, value, .. } => {
            for_each_port_in_expr(index, f);
            for_each_port_in_expr(value, f);
        }
        AssertCheck { cond, on_fail } | AssumeCheck { cond, on_fail } => {
            for_each_port_in_expr(cond, f);
            for_each_port_in_fmt(on_fail, f);
        }
        Log { args, .. } => for_each_port_in_fmt(args, f),
        FailDiag { guard, args } => {
            if let Some(g) = guard {
                for_each_port_in_expr(g, f);
            }
            for_each_port_in_fmt(args, f);
        }
        ScoreboardOp { op, .. } => match op {
            ir::ScoreboardOp::QueuePush { value, .. }
            | ir::ScoreboardOp::ScalarWrite { value, .. } => for_each_port_in_expr(value, f),
            ir::ScoreboardOp::QueuePop { .. } => {}
        },
        ComponentEmit { args, .. } | ComponentCall { args, .. } => {
            args.iter().for_each(|a| for_each_port_in_expr(a, f))
        }
        SeqPush { value, .. }
        | ComponentQueuePush { value, .. }
        | TransactorStateQueuePush { value, .. } => for_each_port_in_expr(value, f),
        ComponentQueuePop { .. }
        | ComponentSubAssign { .. }
        | TransactorStateQueuePop { .. }
        | TbQueuePop { .. } => {}
        TlmFork(desc) => desc.args.iter().for_each(|a| for_each_port_in_expr(a, f)),
        TlmJoinAll(pending) => pending
            .iter()
            .for_each(|p| p.args.iter().for_each(|a| for_each_port_in_expr(a, f))),
        // Check bodies are walked at program level — see
        // `for_each_check_body_expr`.
        EventEmit { args, .. } => args.iter().for_each(|a| for_each_port_in_expr(a, f)),
        EventSubscribe { .. } | MethodHookSubscribe { .. } => {}
        PropertyCheck(_) | CoverCheck(_) | CycleHandler(_) => {}
        RecordInit(_, _) | CovReport(_) => {}
    }
}

/// Invoke `f` on every `PortRef` in a block terminator's expression
/// operands (`Branch`/`WaitCycles`/`WaitUntil` conditions can read a bus
/// signal).
fn for_each_port_in_term(t: &ir::Terminator, f: &mut impl FnMut(&ir::PortRef)) {
    use ir::Terminator::*;
    match t {
        Branch(e, _, _) | WaitCycles(e, _, _) | WaitCyclesSync(e, _) => for_each_port_in_expr(e, f),
        WaitUntil { preds, .. } => preds.iter().for_each(|p| for_each_port_in_expr(&p.expr, f)),
        WaitUntilTimeout { preds, cycles, .. } => {
            preds.iter().for_each(|p| for_each_port_in_expr(&p.expr, f));
            for_each_port_in_expr(cycles, f);
        }
        Fatal(args) => for_each_port_in_fmt(args, f),
        Jump(_) | WaitTimePs(_, _) | Randomize { .. } | Return => {}
    }
}

fn for_each_port_in_fmt(args: &ir::FmtArgs, f: &mut impl FnMut(&ir::PortRef)) {
    args.args
        .iter()
        .for_each(|a| for_each_port_in_expr(&a.expr, f));
}

/// Invoke `f` on every `PortRef` in an expression tree. Parallels
/// `expr_has_probe`'s structural traversal.
fn for_each_port_in_expr(e: &ir::Expr, f: &mut impl FnMut(&ir::PortRef)) {
    use ir::Expr::*;
    match e {
        Port(p) => f(p),
        RecordField {
            mid_indices, index, ..
        } => {
            for (_, i) in mid_indices {
                for_each_port_in_expr(i, f);
            }
            if let Some(i) = index {
                for_each_port_in_expr(i, f);
            }
        }
        Binary(_, a, b) => {
            for_each_port_in_expr(a, f);
            for_each_port_in_expr(b, f);
        }
        Unary(_, a) => for_each_port_in_expr(a, f),
        BitSlice { target, .. } => for_each_port_in_expr(target, f),
        BitSliceDyn { target, hi, lo } => {
            for_each_port_in_expr(target, f);
            for_each_port_in_expr(hi, f);
            for_each_port_in_expr(lo, f);
        }
        Ternary(c, a, b) => {
            for_each_port_in_expr(c, f);
            for_each_port_in_expr(a, f);
            for_each_port_in_expr(b, f);
        }
        WidthCast { inner, .. } => for_each_port_in_expr(inner, f),
        SeqIndex { index, .. } => for_each_port_in_expr(index, f),
        Call(_, args) => args.iter().for_each(|a| for_each_port_in_expr(a, f)),
        ComponentIdle { n, .. } => for_each_port_in_expr(n, f),
        ComponentVecElement { index, .. } => for_each_port_in_expr(index, f),
        _ => {}
    }
}

/// C++ storage type for a record field's scalar (or Vec element) type,
/// using the same width-aware integer policy as standalone locals.
fn field_scalar_cty(ty: &ir::IrType) -> String {
    match ty {
        ir::IrType::Bool => "bool".to_string(),
        _ => local_scalar_cty(ty),
    }
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
/// inner record's total width (v1 parity). `None` for a widthless scalar
/// (no defined layout); lowering already rejects those for Vec fields,
/// and scalar fields never reach the pack helpers when any sum is
/// undefined. For a `Vec<T, N>` field this is the per-element width `T`;
/// the caller multiplies by `N`.
fn field_packed_width(ty: &ir::IrType, records: &[ir::RecordSchema]) -> Option<usize> {
    match ty {
        ir::IrType::Bool => Some(1),
        ir::IrType::UInt(w) | ir::IrType::SInt(w) => w.map(|w| w as usize),
        ir::IrType::Record(rid) => record_packed_width(records.get(rid.index())?, records),
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

fn record_struct(out: &mut String, r: &ir::RecordSchema, records: &[ir::RecordSchema]) {
    writeln!(out, "struct {} {{", r.name).ok();
    for f in &r.fields {
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
    } else {
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
/// width (a widthless scalar field) — exactly v1's `try_fold` guard.
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
    ir::component_mode_includes_activation(source_mode, edge.src_activation)
        && ir::component_mode_includes_activation(sink_mode, edge.sink_activation)
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
) {
    let comp = &prog.components[component.index()];
    if !skip_top_on_handlers {
        for oh in &comp.on_handlers {
            if !ir::component_mode_includes_activation(mode, oh.activation) {
                continue;
            }
            let lambda = func::on_handler_lambda_name(comp, oh);
            writeln!(
                out,
                "{INDENT}{inst_path}.{}.push_back([&](auto _t) {{ {inst_path}._last_in_cycle = (uint64_t)cycle_count; {lambda}({inst_path}, _t); }});",
                oh.event
            )
            .ok();
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
            );
        }
    }
}

/// Emit one resolved `connect` edge's subscriber push_back, rooted at
/// `inst_path` (the instance reaching the connect's owning component).
fn emit_one_connect(
    out: &mut String,
    prog: &TbProgram,
    inst_path: &str,
    edge: &ir::ConnectEdgeSchema,
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
            writeln!(
                out,
                "{INDENT}{src}.{}.push_back([&](auto _t) {{ {sink_comp}_{method}({sink}, _t); }});",
                edge.src_event
            )
            .ok();
        }
        ir::ConnectSink::Event { event } => {
            // event→event bridge: forward each emit on the source event into
            // the sink event's own subscriber list, firing the sink driver's
            // registered `on <ev>` handler(s). Mirrors v1's
            // `for (auto& _s : <sink>.<event>) _s(_t);` bridge closure.
            writeln!(
                out,
                "{INDENT}{src}.{}.push_back([&](auto _t) {{ for (auto& _s : {sink}.{event}) _s(_t); }});",
                edge.src_event
            )
            .ok();
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
                    emit_one_connect(out, prog, &sub_path, edge);
                }
            }
            emit_nested_connects(out, prog, *sub, &sub_path, sub_mode);
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
/// sub-component), each gated on a per-instance static last-fire stamp.
/// Mirrors v1's `emit_watchdog_checker` / periodic `_checkers` shape:
/// every cycle the closure re-reads the period (so a field-backed period
/// stays test-overridable), fires once it is due, and — for the watchdog
/// — runs the user body then the idle check.
fn emit_lifecycle_checkers(
    out: &mut String,
    prog: &TbProgram,
    component: ir::ComponentId,
    inst_path: &str,
    mode: Option<ir::ComponentInstanceMode>,
) -> Result<(), EmitError> {
    let comp = &prog.components[component.index()];
    // A valid C++ identifier for the static tag (`env.agent` → `env_agent`).
    let inst_tag = inst_path.replace('.', "_");

    for ph in &comp.periodic_handlers {
        if !ir::component_mode_includes_activation(mode, ph.activation) {
            continue;
        }
        let lambda = func::periodic_handler_lambda_name(comp, ph);
        let period = func::clause_expr_cpp(prog, ph.function, inst_path, &ph.period)?;
        let tag = format!("_per_{inst_tag}_{}", ph.function.0);
        let svc = ph.phase.service_vec();
        writeln!(out, "{INDENT}{svc}.push_back([&]() {{").ok();
        writeln!(out, "{INDENT}{INDENT}static int64_t {tag}_last = 0;").ok();
        writeln!(
            out,
            "{INDENT}{INDENT}int64_t {tag}_period = (int64_t)({period});"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}{INDENT}if ({tag}_period > 0 && (int64_t)cycle_count - {tag}_last >= {tag}_period) {{"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}{tag}_last = (int64_t)cycle_count;"
        )
        .ok();
        writeln!(out, "{INDENT}{INDENT}{INDENT}{lambda}({inst_path});").ok();
        writeln!(out, "{INDENT}{INDENT}}}").ok();
        writeln!(out, "{INDENT}}});").ok();
    }

    // Cycle-trigger handlers (`on <bool-expr>`). Each installs a
    // `_checkers` closure that re-evaluates the trigger predicate every
    // primary-clock cycle and fires the body when the predicate satisfies
    // the requested edge mode. Mirrors v1's `emit_cycle_trigger`.
    for ch in &comp.cycle_handlers {
        if !ir::component_mode_includes_activation(mode, ch.activation) {
            continue;
        }
        let lambda = func::cycle_handler_lambda_name(comp, ch);
        let trigger = func::clause_expr_cpp(prog, ch.function, inst_path, &ch.trigger)?;
        let tag = format!("_cyc_{inst_tag}_{}", ch.function.0);
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
            // The `_checkers` pass runs once per primary cycle at the same
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
            // `_checkers` latch is trace-correct under both `--mt` and the
            // default — the latch already fires at the right phases on
            // every `tick()`.
            writeln!(out, "{INDENT}_checkers.push_back([&]() {{").ok();
            writeln!(out, "{INDENT}{INDENT}static bool {tag}_cool = false;").ok();
            writeln!(out, "{INDENT}{INDENT}if ({tag}_cool) {{").ok();
            writeln!(out, "{INDENT}{INDENT}{INDENT}{tag}_cool = false;").ok();
            writeln!(out, "{INDENT}{INDENT}}} else if ((bool)({trigger})) {{").ok();
            writeln!(out, "{INDENT}{INDENT}{INDENT}{lambda}({inst_path});").ok();
            writeln!(out, "{INDENT}{INDENT}{INDENT}{tag}_cool = true;").ok();
            writeln!(out, "{INDENT}{INDENT}}}").ok();
            writeln!(out, "{INDENT}}});").ok();
            continue;
        }
        writeln!(out, "{INDENT}_checkers.push_back([&]() {{").ok();
        match ch.edge {
            ir::CycleEdge::Level => {
                writeln!(out, "{INDENT}{INDENT}if ((bool)({trigger})) {{").ok();
                writeln!(out, "{INDENT}{INDENT}{INDENT}{lambda}({inst_path});").ok();
                writeln!(out, "{INDENT}{INDENT}}}").ok();
            }
            ir::CycleEdge::Rising | ir::CycleEdge::Falling => {
                writeln!(out, "{INDENT}{INDENT}static bool {tag}_prev = false;").ok();
                writeln!(out, "{INDENT}{INDENT}bool {tag}_curr = (bool)({trigger});").ok();
                let cond = match ch.edge {
                    ir::CycleEdge::Rising => format!("!{tag}_prev && {tag}_curr"),
                    ir::CycleEdge::Falling => format!("{tag}_prev && !{tag}_curr"),
                    ir::CycleEdge::Level => unreachable!(),
                };
                writeln!(out, "{INDENT}{INDENT}if ({cond}) {{").ok();
                writeln!(out, "{INDENT}{INDENT}{INDENT}{lambda}({inst_path});").ok();
                writeln!(out, "{INDENT}{INDENT}}}").ok();
                writeln!(out, "{INDENT}{INDENT}{tag}_prev = {tag}_curr;").ok();
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
                Some(e) => func::clause_expr_cpp(prog, w.function, inst_path, e)?,
                None => WATCHDOG_DEFAULT_PERIOD.to_string(),
            };
            let max_idle = match &w.max_idle {
                Some(e) => func::clause_expr_cpp(prog, w.function, inst_path, e)?,
                None => WATCHDOG_DEFAULT_MAX_IDLE.to_string(),
            };
            let tag = format!("_wdog_{inst_tag}_{}", w.function.0);
            writeln!(out, "{INDENT}_checkers.push_back([&]() {{").ok();
            writeln!(out, "{INDENT}{INDENT}static int64_t {tag}_last = 0;").ok();
            writeln!(
                out,
                "{INDENT}{INDENT}int64_t {tag}_period = (int64_t)({period});"
            )
            .ok();
            writeln!(
            out,
            "{INDENT}{INDENT}if ({tag}_period > 0 && (int64_t)cycle_count - {tag}_last >= {tag}_period) {{"
        )
        .ok();
            writeln!(
                out,
                "{INDENT}{INDENT}{INDENT}{tag}_last = (int64_t)cycle_count;"
            )
            .ok();
            // 1. User body (typically a heartbeat log).
            writeln!(out, "{INDENT}{INDENT}{INDENT}{lambda}({inst_path});").ok();
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
             && (int64_t)((uint64_t)cycle_count - {inst_path}._last_in_cycle) >= {tag}_max_idle \
             && (int64_t)((uint64_t)cycle_count - {inst_path}._last_out_cycle) >= {tag}_max_idle) {{"
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
            )?;
        }
    }
    Ok(())
}

/// Register each testbench-scoped `on <N> cycles [phase post_eval]`
/// periodic service (issue #485). The handler body was already emitted as
/// a free zero-arg `[&]`-capturing lambda (via the `emit_test_hook` loop),
/// so this only installs the registration closure: a per-cycle
/// `_checkers` / `_post_eval_services` push that fires the lambda once
/// every `period` primary-clock cycles, gated on a static last-fire stamp.
/// Flow-scope analogue of a component's periodic `emit_lifecycle_checkers`
/// registration; the period is a compile-time literal in this subset.
fn emit_tb_periodic_services(
    out: &mut String,
    prog: &TbProgram,
    tb: &ir::TestbenchSchema,
    dut_type: &str,
) -> Result<(), EmitError> {
    for svc in &tb.periodic_services {
        // Emit the body lambda HERE (not in the early test-hook loop) so
        // it sees the composite scoreboard/component instances + method
        // lambdas declared just above.
        func::emit_test_hook(out, prog, prog.function(svc.function), dut_type, 1)?;
        let lambda = &prog.function(svc.function).name;
        let tag = format!("_tbper_{}", svc.function.0);
        let period = svc.period;
        let vec = svc.phase.service_vec();
        writeln!(out, "{INDENT}{vec}.push_back([&]() {{").ok();
        writeln!(out, "{INDENT}{INDENT}static int64_t {tag}_last = 0;").ok();
        writeln!(
            out,
            "{INDENT}{INDENT}if ((int64_t)cycle_count - {tag}_last >= {period}) {{"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}{tag}_last = (int64_t)cycle_count;"
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
) -> Result<(), EmitError> {
    for svc in &tb.cycle_services {
        // Emit the body lambda HERE (after the composite instances +
        // method lambdas are declared, same as the periodic path) so a
        // body reading those symbols resolves.
        func::emit_test_hook(out, prog, prog.function(svc.function), dut_type, 1)?;
        let lambda = &prog.function(svc.function).name;
        let trigger = func::tb_service_expr_cpp(prog, svc.function, dut_type, &svc.trigger)?;
        let tag = format!("_tbcyc_{}", svc.function.0);
        let vec = svc.phase.service_vec();
        // Re-evaluate the predicate every primary-clock cycle and fire the
        // body per the recorded edge mode. Mirrors v1's `emit_cycle_trigger`
        // and the transactor cycle-trigger closure in `emit_lifecycle_checkers`.
        writeln!(out, "{INDENT}{vec}.push_back([&]() {{").ok();
        match svc.edge {
            ir::CycleEdge::Level => {
                writeln!(out, "{INDENT}{INDENT}if ((bool)({trigger})) {{").ok();
                writeln!(out, "{INDENT}{INDENT}{INDENT}{lambda}();").ok();
                writeln!(out, "{INDENT}{INDENT}}}").ok();
            }
            ir::CycleEdge::Rising | ir::CycleEdge::Falling => {
                writeln!(out, "{INDENT}{INDENT}static bool {tag}_prev = false;").ok();
                writeln!(out, "{INDENT}{INDENT}bool {tag}_curr = (bool)({trigger});").ok();
                let cond = match svc.edge {
                    ir::CycleEdge::Rising => format!("!{tag}_prev && {tag}_curr"),
                    ir::CycleEdge::Falling => format!("{tag}_prev && !{tag}_curr"),
                    ir::CycleEdge::Level => unreachable!(),
                };
                writeln!(out, "{INDENT}{INDENT}if ({cond}) {{").ok();
                writeln!(out, "{INDENT}{INDENT}{INDENT}{lambda}();").ok();
                writeln!(out, "{INDENT}{INDENT}}}").ok();
                writeln!(out, "{INDENT}{INDENT}{tag}_prev = {tag}_curr;").ok();
            }
        }
        writeln!(out, "{INDENT}}});").ok();
    }
    Ok(())
}

fn emit_test(
    out: &mut String,
    prog: &TbProgram,
    test: &ir::TestSchema,
    dut_type: &str,
    opts: &EmitOpts,
    randomize_snippets: &[String],
) -> Result<(), EmitError> {
    let tb = prog.testbench(test.testbench);
    let clocked = !test.clocks.is_empty();
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

    runtime::run_prologue(out, &test.name, dut_type, cosim);
    if clocked {
        runtime::clocked_scheduler(out, &test.clocks);
    } else {
        runtime::clockless_scheduler(out);
    }
    runtime::log_helpers_and_seed(out);

    if needs_tb_struct(tb) {
        writeln!(out, "{INDENT}{} _tb;", tb.name).ok();
    }
    // Transaction/struct-typed testbench fields are shared host state.
    // Lowering gives every owning function a synthetic record local of
    // this name for record-field resolution, while codegen declares the
    // actual object once here so helper/run/check/hook bodies capture the
    // same C++ record by reference.
    for (field, rid) in &tb.record_fields {
        let rec_ty = &prog.records[rid.index()].name;
        writeln!(out, "{INDENT}{rec_ty} {field}{{}};").ok();
    }
    // Closure-hook regblock mirrors: a binding with `on regs.REG` write
    // callbacks holds its mirror struct + recursion-depth counter as
    // SHARED test-scope state (declared once, captured by `[&]`), so the
    // run coroutine and every callback lambda hit the same cell. The
    // per-function mirror locals (Run + callbacks) are name-matched to
    // these and skipped at declaration time. Plain regblock bindings keep
    // their run-local mirror (no callbacks → no sharing needed).
    for b in &tb.regblock_bindings {
        if b.callbacks.is_empty() {
            continue;
        }
        let mirror_ty = &prog.records[prog.regblocks[b.regblock.index()].record.index()].name;
        writeln!(out, "{INDENT}{mirror_ty} {}{{}};", b.field).ok();
        writeln!(out, "{INDENT}uint32_t {}_cb_depth = 0;", b.field).ok();
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
    }
    // Hook-vector spine for hook-triggered covergroups
    // (`covergroup G @(drv.send(t) post)`). One
    // `std::vector<std::function<void(args)>> <Type>_<method>_pre/_post`
    // per transactor method that any cov field subscribes to, declared
    // here so both the cov sample-closure push (below) and the method
    // fan-out (`emit_method`) reach the same vectors by `[&]` capture.
    // Mirrors v1's `emit_hook_vectors`. Only methods used by THIS
    // testbench's transactor fields are declared.
    let mut hook_vector_xactors = HashSet::new();
    for (_field, xid) in &tb.transactor_fields {
        if !hook_vector_xactors.insert(*xid) {
            continue;
        }
        let schema = prog.transactor(*xid);
        for m in &schema.methods {
            if m.cov_hook_subs.is_empty()
                && !has_transactor_method_hook_subscription(prog, test.testbench, *xid, &m.name)
            {
                continue;
            }
            covergroup::transactor_hook_vector_decls(out, prog, schema, m, INDENT)?;
        }
    }
    // User method hooks on components follow v1's type-scoped vector
    // contract (`<Component>_<method>_pre/post`). Keep these separate from
    // per-instance covergroup vectors stored on the component struct.
    for (ci, component) in prog.components.iter().enumerate() {
        for method in &component.methods {
            if has_component_method_hook_subscription(
                prog,
                test.testbench,
                ir::ComponentId(ci as u32),
                &method.name,
            ) {
                covergroup::hook_vector_decls(
                    out,
                    prog,
                    &component.name,
                    &method.name,
                    method.function,
                    method.param_names.len(),
                    INDENT,
                )?;
            }
        }
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
                    schema,
                    &format!("_tb.{field}"),
                    &opts.vec_lane_widths,
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
                        xs.method(method).map(|m| {
                            (
                                format!("{}_{}", xs.name, m.name),
                                m.function,
                                m.param_names.len(),
                            )
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
                                    format!("{}._harc_cov_{}", binding.field, m.name),
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
                    *side,
                    &format!("_tb.{field}"),
                    &opts.vec_lane_widths,
                )?;
            }
        }
    }
    // tseq generator lambdas — one `<Name>` per `tseq` declaration in the
    // file, declared before the run coroutine so its `[&]` capture sees
    // them (v1's `emit_tseq` placement). Each returns a
    // `std::vector<Record>` and runs the body's randomize/yield loop.
    for f in &prog.functions {
        if let ir::FunctionKind::Tseq { .. } = f.kind {
            func::emit_tseq(out, prog, f, &prog.records, randomize_snippets, 1)?;
        }
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
    // per transactor type, then one instance VARIABLE per instance. The
    // type-shared method lambda takes the receiver by reference, so any
    // number of active instances of one type coexist with independent
    // state (`Drv_go(a)` and `Drv_go(b)` mutate `a`/`b` separately).
    let mut emitted_state_ty = HashSet::new();
    for (_, xid) in &tb.unbound_state_actors {
        if !emitted_state_ty.insert(*xid) {
            continue;
        }
        runtime::unbound_state_struct_decl(out, prog.transactor(*xid), &prog.records);
    }
    for (instance, xid) in &tb.unbound_state_actors {
        runtime::unbound_state_var(out, prog.transactor(*xid), instance);
    }
    let mut emitted_xactors = HashSet::new();
    for (_, xid) in &tb.transactor_fields {
        if !emitted_xactors.insert(*xid) {
            continue;
        }
        // A transactor type instantiated ONLY as `passive` never has its
        // `when active` method bodies filled with an instance name (the
        // methods are not callable on a passive instance), so emitting
        // them would produce uncompilable code with unfilled
        // `TransactorState` placeholders. Skip the method lambdas for such
        // a type; its per-instance state structs (and any always-on `on`
        // handlers) are emitted elsewhere. A type with at least one active
        // instance still emits its (filled) methods as before. (#494
        // P0a/P1b)
        let has_active_instance = tb
            .transactor_fields
            .iter()
            .any(|(f, x)| x == xid && !tb.passive_transactor_fields.contains(f));
        if !has_active_instance {
            continue;
        }
        let schema = prog.transactor(*xid);
        for m in &schema.methods {
            func::declare_method_slot(out, prog, schema, m, 1)?;
        }
        for m in &schema.methods {
            func::emit_method(
                out,
                prog,
                test.testbench,
                *xid,
                schema,
                m,
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
        let schema = prog.transactor(actor.transactor);
        runtime::target_state_struct_inst(out, schema, &actor.instance, &prog.records);
    }
    for actor in &tb.target_tlm_actors {
        func::emit_target_actor(
            out,
            prog,
            actor,
            &tb.bus_bindings,
            mt,
            &mut actor_threads,
            1,
        )?;
    }

    // Composite-component method lambdas — one `<Comp>_<method>` per
    // method of every component in the file, declared before the run
    // coroutine so its `[&]` capture (and the connect push_backs below)
    // see them. Dependency order (subs before holders) so a method body
    // that calls a sub-component's method sees that lambda first.
    for ci in component_emit_order(prog) {
        let comp = &prog.components[ci];
        for m in &comp.methods {
            func::emit_component_method(
                out,
                prog,
                test.testbench,
                ir::ComponentId(ci as u32),
                comp,
                m,
                randomize_snippets,
                1,
            )?;
        }
        for oh in &comp.on_handlers {
            func::emit_component_on_handler(out, prog, comp, oh, randomize_snippets, 1)?;
        }
        for ph in &comp.periodic_handlers {
            func::emit_component_periodic_handler(out, prog, comp, ph, randomize_snippets, 1)?;
        }
        for ch in &comp.cycle_handlers {
            func::emit_component_cycle_handler(out, prog, comp, ch, randomize_snippets, 1)?;
        }
        if let Some(w) = &comp.watchdog {
            func::emit_component_watchdog(out, prog, comp, w, randomize_snippets, 1)?;
        }
    }
    // Closure-hook bodies (`on <obj>.<method> pre/post` method hooks and
    // `on regs.REG` per-register write callbacks) are free `[&]`-capturing
    // lambdas. Emit them only after every callable transactor/component
    // method has been declared: a valid hook body may call another method,
    // and C++ name lookup requires that callable to exist first. Their
    // subscription/dispatch sites live later in the run/check coroutine.
    // Periodic/cycle service functions are emitted with their registration
    // below and are excluded here.
    let tb_service_fns: HashSet<ir::FunctionId> = tb
        .periodic_services
        .iter()
        .map(|s| s.function)
        .chain(tb.cycle_services.iter().map(|s| s.function))
        .collect();
    for f in &prog.functions {
        if matches!(f.kind, ir::FunctionKind::TestHook)
            && f.owner == Some(test.testbench)
            && !tb_service_fns.contains(&f.id)
        {
            func::emit_test_hook(out, prog, f, dut_type, 1)?;
        }
    }
    // Composite-component connection and lifecycle setup for the instances
    // declared above. Keep this after component method lambdas so connect
    // closures can call `<SinkComp>_<method>(...)`.
    for edge in &tb.connects {
        emit_one_connect(out, prog, "", edge);
    }
    for cf in &tb.component_fields {
        // The top component's own `connect` edges (an env's source→sink, or
        // an agent instantiated directly at test scope), plus any nested
        // sub-component's connects (an env holding an agent whose own
        // `sequencer.dispatched -> drv.req` bridge must be installed).
        for edge in &cf.connects {
            if edge_is_enabled(prog, cf.component, cf.mode, edge) {
                emit_one_connect(out, prog, &cf.field, edge);
            }
        }
        emit_nested_connects(out, prog, cf.component, &cf.field, cf.mode);
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
                &tb.bus_bindings,
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
        emit_on_handler_regs(out, prog, cf.component, &cf.field, relower_driver, cf.mode);
        // `on <N> cycles` periodic + `watchdog` lifecycle `_checkers`
        // closures, for this component and any nested sub-components.
        // Bound-bus handshake monitors stay cooperative `_checkers`
        // latches even under `--mt` (see the NOTE in
        // `emit_lifecycle_checkers`), so no actor registration is needed.
        emit_lifecycle_checkers(out, prog, cf.component, &cf.field, cf.mode)?;
    }

    // Testbench-scoped `on <N> cycles [phase post_eval]` periodic services
    // (issue #485). Each registers a `_checkers` / `_post_eval_services`
    // closure that fires the handler's free lambda once every `period`
    // primary-clock cycles,
    // gated on a per-service last-fire stamp — the flow-scope analogue of
    // a component's `emit_lifecycle_checkers` periodic registration.
    emit_tb_periodic_services(out, prog, tb, dut_type)?;
    emit_tb_cycle_services(out, prog, tb, dut_type)?;

    writeln!(out, "{INDENT}harc_rt::ThreadSlot _run_slot;").ok();
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
    let run_hook_captures: HashSet<ir::LocalId> = run
        .blocks
        .iter()
        .flat_map(|block| &block.stmts)
        .filter_map(|stmt| match stmt {
            ir::Stmt::MethodHookSubscribe { captures, .. } => Some(captures.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    func::declare_flow_hook_captures(out, prog, run, &run_hook_captures, 2)?;
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
    )?;
    if let Some(check) = test.check {
        func::emit_function(
            out,
            prog,
            prog.function(check),
            &prog.records,
            &tb.bus_bindings,
            &opts.vec_lane_widths,
            randomize_snippets,
            dut_type,
            &HashSet::new(),
            2,
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
    runtime::mt_worker_setup(out, &actor_threads);
    runtime::drive_loop(out, clocked, &actor_threads);
    runtime::mt_worker_shutdown(out, &actor_threads);
    let covers: Vec<&ir::CoverCheckSchema> = test
        .cover_checks
        .iter()
        .map(|c| &prog.cover_checks[c.index()])
        .collect();
    runtime::run_epilogue(out, cosim, &covers);
    Ok(())
}
