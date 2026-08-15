//! TB-IR split-emission tests: the plan/shard API, its byte-for-byte
//! agreement with the pre-streaming clone-and-filter emitter, and the
//! determinism guarantees of bounded parallel emission.
//!
//! These run in-process against the `harc` library on purpose.
//! `tests/split_build_e2e.rs` covers the same path end-to-end but is gated
//! on a `verilator` binary and silently skips in CI, so it cannot be the
//! regression net for emitter refactors — this file is.

use harc::codegen::{cpp_tb, merge, tbir};
use harc::ir::{self, lower, verify};
use harc::parser::parse_source;
use std::path::Path;

fn fixture(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn merged_src(src: &str) -> harc::ast::SourceFile {
    let parsed = parse_source(src).expect("source parses");
    merge::merge_for_sim(std::slice::from_ref(&parsed), None).expect("merge")
}

fn merged_fixtures(names: &[&str]) -> harc::ast::SourceFile {
    let parsed: Vec<_> = names
        .iter()
        .map(|n| parse_source(&fixture(n)).unwrap_or_else(|e| panic!("{n} parses: {e:?}")))
        .collect();
    merge::merge_for_sim(&parsed, None).expect("merge")
}

/// Lower + verify one merged source, ready for split emission.
fn program(merged: &harc::ast::SourceFile) -> ir::TbProgram {
    let prog = lower::lower_program(merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    prog
}

/// Independent restatement of `cpp_tb::sanitize_file_component`, which is
/// crate-private. Part of the oracle: the expected filenames are spelled
/// out here rather than borrowed from the implementation.
fn sanitize(name: &str) -> String {
    let out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        "test".to_string()
    } else {
        out
    }
}

/// Independent reimplementation of the PRE-REFACTOR split algorithm,
/// built only out of the public `tbir::emit`: clone the program, filter
/// `.tests` down to the shard, emit the whole thing, then strip the
/// trailing dispatcher `main()`.
///
/// This is the anchor for the whole refactor. If the planner, the
/// index-based test selection, the suite-global scaffold reuse, the shard
/// filenames, or the tail normalization drift by even one byte, this
/// diverges. Keep it written against `emit` alone — the moment it shares
/// code with the implementation it stops being an oracle.
fn reference_split(
    prog: &ir::TbProgram,
    file: &harc::ast::SourceFile,
    opts: &cpp_tb::EmitOpts,
    prefix: &str,
    group_size: usize,
) -> Vec<(String, String)> {
    let group_size = group_size.max(1);
    let names: Vec<String> = prog.tests.iter().map(|t| t.name.clone()).collect();
    let mut out = Vec::new();

    for (shard_idx, shard_names) in names.chunks(group_size).enumerate() {
        let mut shard_prog = prog.clone();
        shard_prog.tests = prog
            .tests
            .iter()
            .filter(|t| shard_names.iter().any(|n| n == &t.name))
            .cloned()
            .collect();
        let cpp = tbir::emit(&shard_prog, file, opts).expect("shard emits");
        let marker = "\nint main(int argc, char** argv) {";
        let idx = cpp
            .rfind(marker)
            .expect("whole-program emission ends in a dispatcher main()");
        let mut body = cpp[..idx].trim_end().to_string();
        body.push('\n');

        let filename = if group_size == 1 {
            format!("{prefix}test_{}.cpp", sanitize(&shard_names[0]))
        } else {
            format!("{prefix}shard{}.cpp", shard_idx + 1)
        };
        out.push((filename, body));
    }
    out
}

/// Emit every shard through the streaming API and collect it in shard
/// order, so results are comparable regardless of completion order.
fn emit_shards_sorted(
    prog: &ir::TbProgram,
    file: &harc::ast::SourceFile,
    opts: &cpp_tb::EmitOpts,
    plan: &tbir::SplitCppPlan,
    jobs: usize,
) -> Vec<(usize, String, String)> {
    let mut got: Vec<(usize, String, String)> = Vec::new();
    tbir::emit_split_shards(prog, file, opts, plan, jobs, |shard, cpp, _| {
        got.push((shard.index, shard.filename.clone(), cpp));
        Ok(())
    })
    .expect("shards emit");
    got.sort_by_key(|(i, _, _)| *i);
    got
}

/// Assert the plan/stream path reproduces `reference_split` byte-for-byte
/// at one group size.
fn assert_matches_reference(
    label: &str,
    merged: &harc::ast::SourceFile,
    prog: &ir::TbProgram,
    group_size: usize,
) {
    let opts = cpp_tb::EmitOpts::default();
    let prefix = "suite__";
    let want = reference_split(prog, merged, &opts, prefix, group_size);
    let plan = tbir::plan_split_tests(prog, merged, &opts, prefix, group_size).expect("plans");
    let got = emit_shards_sorted(prog, merged, &opts, &plan, 1);

    assert_eq!(
        got.len(),
        want.len(),
        "{label} @ group {group_size}: shard count"
    );
    for (idx, ((_, got_name, got_cpp), (want_name, want_cpp))) in got.iter().zip(&want).enumerate() {
        assert_eq!(got_name, want_name, "{label} @ group {group_size}: shard {idx} filename");
        assert_eq!(
            got_cpp.len(),
            want_cpp.len(),
            "{label} @ group {group_size}: shard {idx} ({got_name}) byte length differs \
             from the pre-refactor emitter"
        );
        assert!(
            got_cpp == want_cpp,
            "{label} @ group {group_size}: shard {idx} ({got_name}) bytes differ \
             from the pre-refactor emitter"
        );
    }

    // The dispatcher is planned up front and must still list the whole
    // suite, not any one shard's slice.
    assert_eq!(plan.dispatcher.filename, format!("{prefix}main.cpp"));
    for t in &prog.tests {
        assert!(
            plan.dispatcher
                .contents
                .contains(&format!("extern int run_{}(int argc, char** argv);", t.name)),
            "{label}: dispatcher declares run_{}",
            t.name
        );
    }
}

// ---------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------

/// A suite whose tests differ in the features that drive suite-global
/// emission state: only `Probed` reads a DUT-internal probe, and only
/// `Rand` randomizes. Both `has_probes` and the solver problem table are
/// computed over UNFILTERED program tables, so every shard — including
/// the ones carrying neither test — must still receive them. This is the
/// fixture that catches the tempting-but-wrong "narrow the suite-global
/// scaffold to the selected tests" optimization.
fn mixed_feature_suite() -> String {
    "\
transaction Pkt
    addr : uint<8>
    data : uint<8>
end transaction Pkt

test Plain
    let dut : SplitAdder
    run
        dut.a = 1
        dut.b = 2
        wait 1 cycle
        assert dut.sum == 3
    end run
end test Plain

test Probed
    let dut : SplitAdder
        probe inner_sum : uint<9> at sum
    end let dut
    run
        dut.a = 4
        dut.b = 5
        wait 1 cycle
        assert dut.inner_sum == 9
    end run
end test Probed

test Rand
    let dut : SplitAdder
    run
        let p : Pkt
        randomize(p) with
            p.addr == 7
        end randomize
        assert p.addr == 7
        dut.a = 1
        dut.b = 1
        wait 1 cycle
    end run
end test Rand

test Quiet
    let dut : SplitAdder
    run
        dut.a = 6
        dut.b = 6
        wait 1 cycle
        assert dut.sum == 12
    end run
end test Quiet
"
    .to_string()
}

/// `n` interchangeable tests — enough shards to make parallel emission
/// actually race.
fn wide_suite(n: usize) -> String {
    let mut s = String::new();
    for i in 0..n {
        s.push_str(&format!(
            "test T{i}\n    let dut : SplitAdder\n    run\n        dut.a = {i}\n        \
             dut.b = 1\n        wait 1 cycle\n    end run\nend test T{i}\n\n"
        ));
    }
    s
}

/// Names that need `sanitize_file_component` to produce a legal filename.
fn awkward_name_suite() -> String {
    "\
test Alpha
    let dut : SplitAdder
    run
        wait 1 cycle
    end run
end test Alpha

test Beta_2
    let dut : SplitAdder
    run
        wait 1 cycle
    end run
end test Beta_2
"
    .to_string()
}

// ---------------------------------------------------------------------
// (a) Byte identity against the pre-refactor emitter
// ---------------------------------------------------------------------

#[test]
fn split_output_matches_pre_refactor_emitter_across_group_sizes() {
    let merged = merged_src(&mixed_feature_suite());
    let prog = program(&merged);
    assert_eq!(prog.tests.len(), 4, "suite shape");
    for group_size in [1usize, 2, 3, 4, 8] {
        assert_matches_reference("mixed_feature_suite", &merged, &prog, group_size);
    }
}

#[test]
fn suite_global_scaffold_reaches_every_shard() {
    let merged = merged_src(&mixed_feature_suite());
    let prog = program(&merged);
    let opts = cpp_tb::EmitOpts::default();
    // Group size 1 => one shard per test, so `Plain` and `Quiet` get
    // shards that own neither the probe nor the randomize site.
    let plan = tbir::plan_split_tests(&prog, &merged, &opts, "suite__", 1).expect("plans");
    let shards = emit_shards_sorted(&prog, &merged, &opts, &plan, 1);
    assert_eq!(shards.len(), 4);

    for (idx, name, cpp) in &shards {
        assert!(
            cpp.contains("___024root.h"),
            "shard {idx} ({name}) must keep the suite-wide probe include even though \
             its own test does not probe"
        );
        assert!(
            cpp.contains("_harc_runtime_random_problem_table"),
            "shard {idx} ({name}) must keep the suite-wide solver problem table even \
             though its own test does not randomize"
        );
    }
}

#[test]
fn split_output_matches_pre_refactor_emitter_for_feature_fixtures() {
    // Single-test fixtures still matter: the one-shard case is exactly
    // where shard emission has to reproduce whole-program `emit`'s bytes.
    for name in [
        "cov_cross_bins_test.harc",
        "scoreboard_basic_test.harc",
        "transaction_basic_test.harc",
        "env_quiesced_phase_test.harc",
        "analysis_sink_connect_test.harc",
        "tlm_method_blocking_bus_test.harc",
        "tlm_bind_remap_test.harc",
        "sequencer_connect_test.harc",
        "regblock_subset_test.harc",
        "probe_force_test.harc",
    ] {
        let merged = merged_src(&fixture(name));
        let prog = program(&merged);
        for group_size in [1usize, 2] {
            assert_matches_reference(name, &merged, &prog, group_size);
        }
    }
}

#[test]
fn split_output_matches_pre_refactor_emitter_for_randomize_fixture() {
    // The highest-value case: the only one exercising reuse of both the
    // runtime problem table and the per-site randomize snippets.
    let merged = merged_fixtures(&["axilite_constraint_test.harc", "axilite_regs_test.harc"]);
    let prog = program(&merged);
    for group_size in [1usize, 2] {
        assert_matches_reference("axilite_constraint_test.harc", &merged, &prog, group_size);
    }
}

#[test]
fn batch_split_api_matches_streaming_api() {
    let merged = merged_src(&mixed_feature_suite());
    let prog = program(&merged);
    let opts = cpp_tb::EmitOpts::default();

    let batch = tbir::emit_split_tests_with_file_prefix(&prog, &merged, opts.clone(), "suite__", 2)
        .expect("batch emits");
    let plan = tbir::plan_split_tests(&prog, &merged, &opts, "suite__", 2).expect("plans");
    let streamed = emit_shards_sorted(&prog, &merged, &opts, &plan, 1);

    assert_eq!(batch.files.len(), streamed.len() + 1, "dispatcher + shards");
    assert_eq!(batch.files[0].filename, plan.dispatcher.filename);
    assert_eq!(batch.files[0].contents, plan.dispatcher.contents);
    for (batched, (_, name, cpp)) in batch.files[1..].iter().zip(&streamed) {
        assert_eq!(&batched.filename, name);
        assert_eq!(&batched.contents, cpp);
    }
    assert_eq!(
        batch.test_names,
        prog.tests.iter().map(|t| t.name.clone()).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------
// (b) Job-count invariance
// ---------------------------------------------------------------------

#[test]
fn split_emission_is_job_count_invariant() {
    let merged = merged_src(&wide_suite(8));
    let prog = program(&merged);
    let opts = cpp_tb::EmitOpts::default();
    // Group size 2 over 8 tests => 4 shards, so jobs=4 genuinely races.
    let plan = tbir::plan_split_tests(&prog, &merged, &opts, "suite__", 2).expect("plans");
    assert_eq!(plan.shards.len(), 4);

    let baseline = emit_shards_sorted(&prog, &merged, &opts, &plan, 1);
    for jobs in [2usize, 3, 4, 8] {
        let got = emit_shards_sorted(&prog, &merged, &opts, &plan, jobs);
        assert_eq!(got, baseline, "emit-jobs {jobs} must match serial emission");
    }
}

#[test]
fn repeated_parallel_emission_is_stable() {
    let merged = merged_src(&wide_suite(8));
    let prog = program(&merged);
    let opts = cpp_tb::EmitOpts::default();
    let plan = tbir::plan_split_tests(&prog, &merged, &opts, "suite__", 2).expect("plans");
    let baseline = emit_shards_sorted(&prog, &merged, &opts, &plan, 1);
    // Scheduling-dependent bugs do not reproduce every run; repeat cheaply.
    for round in 0..20 {
        let got = emit_shards_sorted(&prog, &merged, &opts, &plan, 4);
        assert_eq!(got, baseline, "round {round}");
    }
}

#[test]
fn emission_inputs_are_shareable_across_threads() {
    // A future `Rc`/`RefCell` field on any of these would otherwise fail
    // deep inside the worker closure with an opaque error.
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ir::TbProgram>();
    assert_send_sync::<harc::ast::SourceFile>();
    assert_send_sync::<cpp_tb::EmitOpts>();
    assert_send_sync::<tbir::SplitCppPlan>();
    assert_send_sync::<tbir::SplitShardPlan>();
}

#[test]
fn resolve_emit_jobs_clamps_to_shards_and_cap() {
    // Explicit request: clamped to the shard count, never below 1.
    assert_eq!(tbir::resolve_emit_jobs(8, 3), 3);
    assert_eq!(tbir::resolve_emit_jobs(2, 9), 2);
    assert_eq!(tbir::resolve_emit_jobs(1, 9), 1);
    // Degenerate inputs stay usable rather than producing a zero-worker pool.
    assert_eq!(tbir::resolve_emit_jobs(0, 0), 1);
    assert_eq!(tbir::resolve_emit_jobs(9, 0), 1);
    // Automatic mode is bounded by both the shard count and the memory cap.
    assert_eq!(tbir::resolve_emit_jobs(0, 1), 1);
    let auto = tbir::resolve_emit_jobs(0, 64);
    assert!((1..=4).contains(&auto), "auto jobs {auto} within the cap of 4");
}

// ---------------------------------------------------------------------
// (c) Plan shape
// ---------------------------------------------------------------------

#[test]
fn plan_groups_tests_and_names_files() {
    let merged = merged_src(&wide_suite(5));
    let prog = program(&merged);
    let opts = cpp_tb::EmitOpts::default();

    let one = tbir::plan_split_tests(&prog, &merged, &opts, "pfx__", 1).expect("plans");
    assert_eq!(one.shards.len(), 5);
    for (i, shard) in one.shards.iter().enumerate() {
        assert_eq!(shard.index, i);
        assert_eq!(shard.test_indices, vec![i]);
        assert_eq!(shard.filename, format!("pfx__test_T{i}.cpp"));
    }

    let two = tbir::plan_split_tests(&prog, &merged, &opts, "pfx__", 2).expect("plans");
    assert_eq!(two.shards.len(), 3);
    assert_eq!(
        two.shards.iter().map(|s| s.test_indices.clone()).collect::<Vec<_>>(),
        vec![vec![0, 1], vec![2, 3], vec![4]]
    );
    for (i, shard) in two.shards.iter().enumerate() {
        assert_eq!(shard.filename, format!("pfx__shard{}.cpp", i + 1));
        assert_eq!(shard.index, i);
    }

    // Group size 0 clamps to 1 rather than dividing by zero.
    let zero = tbir::plan_split_tests(&prog, &merged, &opts, "pfx__", 0).expect("plans");
    assert_eq!(zero.shards.len(), 5);

    // A group at least as large as the suite is ONE shard, and it is a
    // `shardN.cpp` — the per-test filename form is reserved for group 1.
    let whole = tbir::plan_split_tests(&prog, &merged, &opts, "pfx__", 99).expect("plans");
    assert_eq!(whole.shards.len(), 1);
    assert_eq!(whole.shards[0].filename, "pfx__shard1.cpp");
    assert_eq!(whole.shards[0].test_indices, vec![0, 1, 2, 3, 4]);

    // Every test lands in exactly one shard, in ascending order.
    for plan in [&one, &two, &zero, &whole] {
        let mut seen: Vec<usize> = plan
            .shards
            .iter()
            .flat_map(|s| s.test_indices.iter().copied())
            .collect();
        assert!(
            plan.shards
                .iter()
                .all(|s| s.test_indices.windows(2).all(|w| w[0] < w[1])),
            "shard test indices ascending"
        );
        let planned = seen.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen, (0..prog.tests.len()).collect::<Vec<_>>());
        assert_eq!(planned.len(), seen.len(), "no test emitted twice");
    }
}

#[test]
fn plan_sanitizes_per_test_filenames() {
    let merged = merged_src(&awkward_name_suite());
    let prog = program(&merged);
    let opts = cpp_tb::EmitOpts::default();
    let plan = tbir::plan_split_tests(&prog, &merged, &opts, "pfx__", 1).expect("plans");
    let names: Vec<&str> = plan.shards.iter().map(|s| s.filename.as_str()).collect();
    assert_eq!(names, vec!["pfx__test_Alpha.cpp", "pfx__test_Beta_2.cpp"]);
}

#[test]
fn split_plan_emits_no_shared_headers() {
    // Guards the removed common-prefix factoring (see
    // docs/separate-compilation-plan.md): shards are self-contained
    // translation units, so nothing but `.cpp` may be planned. The
    // suite-global scaffold is a Rust-side value, NOT a generated header.
    let merged = merged_src(&mixed_feature_suite());
    let prog = program(&merged);
    let opts = cpp_tb::EmitOpts::default();
    let plan = tbir::plan_split_tests(&prog, &merged, &opts, "suite__", 2).expect("plans");
    let mut planned: Vec<&str> = vec![plan.dispatcher.filename.as_str()];
    planned.extend(plan.shards.iter().map(|s| s.filename.as_str()));
    for name in planned {
        assert!(
            name.ends_with(".cpp"),
            "split output must stay self-contained .cpp; got {name}"
        );
    }
}

#[test]
fn plan_rejects_suite_wide_failures_before_any_shard() {
    let opts = cpp_tb::EmitOpts::default();

    // Multi-DUT is a suite-wide rejection, so it must surface from the
    // planner rather than from whichever shard happens to run first.
    let merged = merged_src(
        "\
test UsesX
    let dut : SplitX
    run
        wait 1 cycle
    end run
end test UsesX

test UsesY
    let dut : SplitY
    run
        wait 1 cycle
    end run
end test UsesY
",
    );
    let prog = program(&merged);
    let msg = match tbir::plan_split_tests(&prog, &merged, &opts, "suite__", 1) {
        Ok(_) => panic!("multi-DUT split must be rejected"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("multi-DUT tests in one split binary"),
        "planner reports the split-scoped diagnostic: {msg}"
    );
}

// ---------------------------------------------------------------------
// (d) Failure behavior
// ---------------------------------------------------------------------

#[test]
fn callback_error_propagates_unchanged_at_every_job_count() {
    let merged = merged_src(&wide_suite(8));
    let prog = program(&merged);
    let opts = cpp_tb::EmitOpts::default();
    let plan = tbir::plan_split_tests(&prog, &merged, &opts, "suite__", 2).expect("plans");

    // Stands in for a `write_if_changed` I/O failure in the driver.
    for jobs in [1usize, 2, 4, 8] {
        let err = tbir::emit_split_shards(&prog, &merged, &opts, &plan, jobs, |shard, _, _| {
            if shard.index == 2 {
                Err(cpp_tb::EmitError("write failed".into()))
            } else {
                Ok(())
            }
        })
        .expect_err("callback failure aborts the build");
        assert_eq!(err.to_string(), "write failed", "jobs={jobs}");
    }
}

#[test]
fn every_shard_is_offered_exactly_once() {
    let merged = merged_src(&wide_suite(9));
    let prog = program(&merged);
    let opts = cpp_tb::EmitOpts::default();
    let plan = tbir::plan_split_tests(&prog, &merged, &opts, "suite__", 1).expect("plans");
    assert_eq!(plan.shards.len(), 9);

    for jobs in [1usize, 2, 4, 16] {
        let mut seen: Vec<usize> = Vec::new();
        tbir::emit_split_shards(&prog, &merged, &opts, &plan, jobs, |shard, _, _| {
            seen.push(shard.index);
            Ok(())
        })
        .expect("shards emit");
        seen.sort_unstable();
        assert_eq!(
            seen,
            (0..9).collect::<Vec<_>>(),
            "jobs={jobs}: every shard delivered exactly once"
        );
    }
}
