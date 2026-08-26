//! End-to-end regression for `harc sim --cpp-split tests` (PR #330,
//! "split generated C++ test shards").
//!
//! The unit tests in `tests/codegen.rs`
//! (`split_tests_emit_dispatcher_and_one_unit_per_test`,
//! `grouped_split_reemits_per_test_suffixes`) assert on the *shape* of the
//! generated C++ — file names, `#include` lines, the presence of each
//! `run_<Test>` body. They never compile or link the split output, so the
//! load-bearing property of the feature — that the dispatcher TU plus N
//! shard TUs link into one working binary — is unguarded by CI.
//!
//! That property is non-trivial. Each shard is `emit_with_opts` on a
//! filtered source, so every shard re-emits the full file-scope scaffolding
//! (the `HarcTestContext` struct, all `static`/template helpers, `harc_rng`,
//! …). Linking several shards only works because those definitions have
//! internal linkage (or are inline/templates); the only external-linkage
//! symbols are the per-test `run_<Test>` functions (unique per shard) and
//! `main` (only in the dispatcher). A future refactor that promoted any of
//! that scaffolding to an external-linkage definition, or that desynced the
//! dispatcher's `extern int run_<Test>(...)` declarations from the emitted
//! definitions, would pass every existing string-level test but produce a
//! duplicate-symbol or undefined-symbol link error on a real build.
//!
//! This file drives the real CLI end to end:
//!   1. `--cpp-split tests` at the default group size (8 tests → 2 shards
//!      + dispatcher) must build, link, and dispatch every test by name.
//!   2. `--cpp-split-group-size 1` (the per-test shard path, which the
//!      default group size never exercises) must also build, link, and
//!      dispatch.
//! Both checks run under v1 and TBIR codegen.
//!
//! The DUT and multi-test testbench are written into the temp dir by the
//! test itself: the inputs are trivial and are most legible sitting next to
//! the assertions, and keeping them inline avoids committing fixtures whose
//! only purpose is "have more than `--cpp-split-group-size` tests".

use std::path::{Path, PathBuf};
use std::process::Command;

/// Names of the tests emitted into the testbench below. Eight tests at
/// the default group size of 4 yields two shards, so the default path
/// genuinely links more than one shard TU. `T7VecRecord` carries a
/// `Vec<Record, N>` struct field plus indexed element access — the
/// issue-523 blocker shape whose mere presence in the suite used to
/// abort whole-program TB-IR lowering and emit ZERO shards. `T8WideQueue`
/// adds direct `queue<uint<129>>` and non-word-aligned `queue<sint<65>>`
/// state, so split emission has to retain the wide runtime types in a shard.
const TEST_NAMES: [&str; 8] = [
    "T1",
    "T2",
    "T3",
    "T4",
    "T5",
    "T6",
    "T7VecRecord",
    "T8WideQueue",
];

fn harc_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_harc"))
}

/// Whether a `verilator` binary is on PATH. `--cpp-split` only needs a
/// working C++ toolchain + Verilator; unlike the trace-merge e2e it has no
/// minimum-version requirement (no `--trace-vcd`).
fn verilator_present() -> bool {
    let present = Command::new("verilator")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    // See the same guard in `tbir_wide_scoreboard_e2e.rs`. A skipped
    // end-to-end test reports `ok`, and an `ok` in 0.00s for a test that
    // builds a Verilator model reads exactly like a real pass in a CI
    // log — which is how harc#662 hid a regression for weeks. CI
    // installs Verilator for the `cargo test` job and sets this
    // variable, so the silent skip cannot come back unnoticed.
    assert!(
        present || std::env::var_os("HARC_REQUIRE_VERILATOR").is_none(),
        "HARC_REQUIRE_VERILATOR is set but `verilator` is not on PATH: this \
         end-to-end test would have skipped itself and reported success"
    );
    present
}

fn fresh_outdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "harc_split_build_e2e_{}_{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp outdir");
    dir
}

/// A trivial combinational adder DUT. Combinational-only keeps the build
/// fast and exercises the `if constexpr (requires { dut->clk; })` clock
/// guard added in PR #328 alongside the split work.
const ADDER_SV: &str = "\
module SplitAdder(input logic [7:0] a, input logic [7:0] b, output logic [8:0] sum);
  always_comb sum = a + b;
endmodule
";

const MIXED_DUT_SV: &str = "\
module SplitX(output logic done);
  always_comb done = 1'b1;
endmodule

module SplitY(output logic done);
  always_comb done = 1'b1;
endmodule
";

/// Six independent tests in the classic `test … let dut … run … end`
/// form the split path keys on. Each checks a distinct sum so a
/// mis-dispatch (running the wrong `run_<Test>`) would surface as an
/// assertion failure, not a false pass.
fn adder_tb() -> String {
    let cases = [
        (1u32, 2, 3),
        (10, 20, 30),
        (100, 50, 150),
        (5, 5, 10),
        (7, 8, 15),
        (200, 55, 255),
    ];
    // File-scope record decls shared by the whole suite. `Vec<SplitEntry,
    // 4>` is the issue-523 blocker-1 construct: whole-program lowering
    // must accept it for ANY shard to emit, and `T7VecRecord` exercises
    // the indexed element accesses inside a sharded test body.
    let mut s = String::from(
        "struct SplitEntry\n    tag : uint<8>\n    value : uint<32>\nend struct SplitEntry\n\n\
         struct SplitTable\n    entries : Vec<SplitEntry, 4>\nend struct SplitTable\n\n",
    );
    for (name, (a, b, sum)) in TEST_NAMES.iter().zip(cases) {
        s.push_str(&format!(
            "test {name}\n    let dut : SplitAdder\n    run\n        dut.a = {a}\n        dut.b = {b}\n        wait 1 cycle\n        assert dut.sum == {sum}\n    end run\nend test {name}\n"
        ));
    }
    s.push_str(
        "test T7VecRecord\n    let dut : SplitAdder\n    run\n        \
         let table : SplitTable\n        \
         table.entries[0].tag = 5\n        \
         let i : uint<8> = 3\n        \
         table.entries[i].value = 77\n        \
         let e : SplitEntry = table.entries[i]\n        \
         assert e.value == 77\n        \
         assert table.entries[0].tag == 5\n        \
         dut.a = 1\n        dut.b = 2\n        wait 1 cycle\n        \
         assert dut.sum == 3\n    end run\nend test T7VecRecord\n",
    );
    s.push_str(
        "testbench SplitWideQueueTb\n    dut : SplitAdder\n    values : queue<uint<129>>\n    \
         signed_values : queue<sint<65>>\nend testbench SplitWideQueueTb\n\n\
         impl T8WideQueue for SplitWideQueueTb\n    run\n        let one : uint<129> = 1\n        \
         values.push(one << 128)\n        let got = values.pop()\n        \
         assert (got >> 128) == 1\n        let signed_value : sint<65> = 5 as sint<65>\n        \
         signed_values.push(signed_value)\n        let signed_got = signed_values.pop()\n        \
         assert signed_got == signed_value\n        dut.a = 1\n        dut.b = 2\n        wait 1 cycle\n        \
         assert dut.sum == 3\n    end run\nend impl T8WideQueue\n",
    );
    s
}

fn mixed_dut_tb() -> String {
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
"
    .to_string()
}

/// Run `harc sim --sv … --cpp-split tests [--cpp-split-group-size N]` and
/// return (success, combined-output, binary-path).
fn run_split_build(
    outdir: &Path,
    sv: &Path,
    tb: &Path,
    codegen: &str,
    group_size: Option<u32>,
) -> (bool, String) {
    let mut cmd = Command::new(harc_bin());
    cmd.arg("sim")
        .arg("--sv")
        .arg(sv)
        .arg(tb)
        .arg("--top")
        .arg("SplitAdder")
        .arg("--codegen")
        .arg(codegen)
        .arg("--cpp-split")
        .arg("tests")
        // `--jobs 2` reaches Verilator as `-j 2`, so independent shard
        // translation units compile concurrently (issue-523 acceptance).
        .arg("--jobs")
        .arg("2")
        .arg("--outdir")
        .arg(outdir);
    if let Some(n) = group_size {
        cmd.arg("--cpp-split-group-size").arg(n.to_string());
    }
    let out = cmd.output().expect("spawn harc sim");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    (
        out.status.success(),
        format!("--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"),
    )
}

fn run_split_emit_only(
    outdir: &Path,
    sv: &Path,
    tb: &Path,
    codegen: &str,
    group_size: Option<u32>,
) -> (bool, String) {
    let mut cmd = Command::new(harc_bin());
    cmd.arg("sim")
        .arg("--sv")
        .arg(sv)
        .arg(tb)
        .arg("--top")
        .arg("SplitX")
        .arg("--codegen")
        .arg(codegen)
        .arg("--cpp-split")
        .arg("tests")
        .arg("--emit-only")
        .arg("--outdir")
        .arg(outdir);
    if let Some(n) = group_size {
        cmd.arg("--cpp-split-group-size").arg(n.to_string());
    }
    let out = cmd.output().expect("spawn harc sim --emit-only");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    (
        out.status.success(),
        format!("--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"),
    )
}

/// Run the freshly built dispatcher binary with `--test <name>` and assert
/// it reports a pass. The binary lives at `<outdir>/obj_dir/VSplitAdder`.
fn assert_each_test_dispatches(outdir: &Path, label: &str) {
    let bin = outdir.join("obj_dir/VSplitAdder");
    assert!(
        bin.exists(),
        "[{label}] split build did not produce a linked binary at {}",
        bin.display()
    );
    for name in TEST_NAMES {
        let out = Command::new(&bin)
            .arg("--test")
            .arg(name)
            .output()
            .unwrap_or_else(|e| panic!("[{label}] spawn {} --test {name}: {e}", bin.display()));
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            combined.contains("ALL TESTS PASSED"),
            "[{label}] dispatch of `{name}` did not pass; output:\n{combined}"
        );
        assert!(
            !combined.contains("TESTS FAILED"),
            "[{label}] dispatch of `{name}` reported a failure; output:\n{combined}"
        );
    }

    // An unknown test must be rejected, not silently routed to test 0.
    let out = Command::new(&bin)
        .arg("--test")
        .arg("NoSuchTest")
        .output()
        .expect("spawn unknown-test dispatch");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("unknown test"),
        "[{label}] unknown test name should be reported, got:\n{combined}"
    );
}

#[test]
fn split_build_links_and_dispatches_e2e() {
    if !verilator_present() {
        eprintln!(
            "SKIP split_build_links_and_dispatches_e2e: `verilator` not found on PATH. \
             This test compiles and links the generated split shards through Verilator."
        );
        return;
    }

    let dir = fresh_outdir("inputs");
    let sv = dir.join("SplitAdder.sv");
    let tb = dir.join("split_tb.harc");
    std::fs::write(&sv, ADDER_SV).expect("write DUT");
    std::fs::write(&tb, adder_tb()).expect("write TB");

    let mut outdirs = Vec::new();
    for codegen in ["v1", "tbir"] {
        // 1. Default group size (4): 8 tests → 2 shards + dispatcher. This is
        //    the path that links more than one shard TU together.
        let out_default = fresh_outdir(&format!("{codegen}_default"));
        let (ok, log) = run_split_build(&out_default, &sv, &tb, codegen, None);
        assert!(ok, "{codegen} default-group split build failed:\n{log}");
        assert_each_test_dispatches(&out_default, &format!("{codegen} group=default"));
        outdirs.push(out_default);

        // 2. group_size = 1: the per-test shard path. The default group size
        //    never reaches this branch.
        let out_g1 = fresh_outdir(&format!("{codegen}_g1"));
        let (ok, log) = run_split_build(&out_g1, &sv, &tb, codegen, Some(1));
        assert!(ok, "{codegen} group_size=1 split build failed:\n{log}");
        assert_each_test_dispatches(&out_g1, &format!("{codegen} group=1"));
        outdirs.push(out_g1);
    }

    let _ = std::fs::remove_dir_all(&dir);
    for outdir in outdirs {
        let _ = std::fs::remove_dir_all(outdir);
    }
}

#[test]
fn split_build_rejects_mixed_dut_before_sharding() {
    let dir = fresh_outdir("mixed_inputs");
    let sv = dir.join("MixedSplitDuts.sv");
    let tb = dir.join("mixed_split_tb.harc");
    std::fs::write(&sv, MIXED_DUT_SV).expect("write mixed DUTs");
    std::fs::write(&tb, mixed_dut_tb()).expect("write mixed-DUT TB");

    let mut outdirs = Vec::new();
    for codegen in ["v1", "tbir"] {
        let outdir = fresh_outdir(&format!("{codegen}_mixed_g1"));
        let (ok, log) = run_split_emit_only(&outdir, &sv, &tb, codegen, Some(1));
        assert!(
            !ok,
            "{codegen} split emit unexpectedly accepted mixed-DUT tests:\n{log}"
        );
        assert!(
            log.contains("multi-DUT") && log.contains("SplitX") && log.contains("SplitY"),
            "{codegen} mixed-DUT split error should name both DUTs; got:\n{log}"
        );
        outdirs.push(outdir);
    }

    let _ = std::fs::remove_dir_all(&dir);
    for outdir in outdirs {
        let _ = std::fs::remove_dir_all(outdir);
    }
}

/// `--emit-jobs` must not change a single generated byte, and an
/// unchanged rerun must still reuse every file.
///
/// Unlike the build tests above this needs no Verilator (`--emit-only`
/// stops after emission), so it is the one CLI-level guard on parallel
/// split emission that actually runs in CI.
#[test]
fn emit_jobs_does_not_change_generated_output() {
    let dir = fresh_outdir("emit_jobs_src");
    let sv = dir.join("SplitAdder.sv");
    let tb = dir.join("adder_tb.harc");
    std::fs::write(&sv, ADDER_SV).expect("write DUT");
    std::fs::write(&tb, adder_tb()).expect("write TB");

    let run = |outdir: &Path, jobs: &str| -> (bool, String) {
        let out = Command::new(harc_bin())
            .args(["sim", "--sv"])
            .arg(&sv)
            .arg(&tb)
            .args(["--top", "SplitAdder", "--codegen", "tbir"])
            .args(["--cpp-split", "tests", "--cpp-split-group-size", "2"])
            .args(["--emit-jobs", jobs])
            .arg("--emit-only")
            .arg("--outdir")
            .arg(outdir)
            .output()
            .expect("spawn harc sim --emit-only");
        (
            out.status.success(),
            format!(
                "--- stdout ---\n{}\n--- stderr ---\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    };

    let serial = fresh_outdir("emit_jobs_1");
    let parallel = fresh_outdir("emit_jobs_4");
    let (ok, log) = run(&serial, "1");
    assert!(ok, "serial split emit failed:\n{log}");
    let (ok, log) = run(&parallel, "4");
    assert!(ok, "parallel split emit failed:\n{log}");

    let generated = |outdir: &Path| -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(outdir)
            .expect("read outdir")
            .map(|e| e.expect("dir entry").path())
            .filter(|p| p.extension().is_some_and(|x| x == "cpp"))
            .map(|p| {
                (
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    std::fs::read(&p).expect("read generated file"),
                )
            })
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    };

    let want = generated(&serial);
    let got = generated(&parallel);
    assert!(
        want.len() >= 4,
        "expected a dispatcher plus multiple shards, got {} files",
        want.len()
    );
    assert_eq!(
        want.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        got.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        "--emit-jobs changed which files are generated"
    );
    for ((name, want_bytes), (_, got_bytes)) in want.iter().zip(&got) {
        assert!(
            want_bytes == got_bytes,
            "--emit-jobs 4 changed the bytes of {name}"
        );
    }

    // Re-emitting unchanged sources must reuse every file, so Verilator's
    // mtime-based skip still holds under parallel emission. Assert on
    // mtimes rather than on the log: the dispatcher reports `emitted
    // <path>` while a shard reports `..., emitted` line-final, so a
    // substring check is easy to write in a way that silently matches
    // nothing.
    let mtimes = |outdir: &Path| -> Vec<(String, std::time::SystemTime)> {
        generated(outdir)
            .into_iter()
            .map(|(name, _)| {
                let m = std::fs::metadata(outdir.join(&name)).expect("stat generated file");
                (name, m.modified().expect("mtime"))
            })
            .collect()
    };
    let before = mtimes(&parallel);
    let (ok, log) = run(&parallel, "4");
    assert!(ok, "rerun failed:\n{log}");
    let after = mtimes(&parallel);
    assert_eq!(
        before, after,
        "unchanged rerun rewrote a generated file (mtime moved):\n{log}"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&serial);
    let _ = std::fs::remove_dir_all(&parallel);
}

// --------------------------------------------------------------------------
// #619 M4 hardening (Phase A1): SHARED reusable-testbench lifecycle across a
// self-contained multi-shard split, REAL build + link + run.
//
// The in-process ownership test `m4b_self_contained_split_emits_static_coro_
// per_shard` (tests/tbir_lifecycle_ownership.rs) pins the LINKAGE KEYWORD by
// substring, but a byte/string compare cannot prove that the resulting shard
// TUs actually LINK: an external (non-`static`) Coro definition in two shard
// TUs is a duplicate-symbol link error, and a desynced prototype is an
// undefined-symbol link error — exactly the class of #619 M4b review defect
// #2, which no string test detects. This drives the real CLI: two impls at
// `--cpp-split-group-size 1` produce two self-contained shard TUs, each
// DEFINING the shared suspending (`Coro`) setup, and they must build, link
// into one binary, and dispatch every test.
// --------------------------------------------------------------------------

/// A clocked counter DUT for the reusable-testbench lifecycle e2e. Unlike
/// the combinational `SplitAdder`, a lifecycle `setup` that does `wait N
/// cycles` needs a real clock edge to advance.
const LC_COUNTER_SV: &str = "\
module LcCounter(input logic clk, input logic rst, input logic en, output logic [7:0] count_out);
  logic [7:0] c;
  always_ff @(posedge clk) begin
    if (rst) c <= 0;
    else if (en) c <= c + 8'd1;
  end
  assign count_out = c;
endmodule
";

/// A reusable testbench with a SHARED lifecycle: a suspending (`Coro`)
/// `setup` — `wait N cycles` directly in the body — plus a non-suspending
/// (`Plain`) `check`, bound by two impls. Under the default native
/// out-of-line lifecycle the self-contained split emits the setup coroutine
/// as a `static harc_rt::HarcThread` in EACH shard; only a real multi-shard
/// link proves those internal-linkage definitions do not collide.
const LC_SHARED_TB: &str = "\
testbench LcSharedTb
    dut : LcCounter
    expected : uint<32> default 0

    setup
        dut.rst = 1
        dut.en = 0
        wait 2 cycles
        dut.rst = 0
        wait 1 cycle
    end setup

    check
        assert dut.count_out == expected
            else fail(\"shared check: count=${dut.count_out} exp=${expected}\")
    end check
end testbench LcSharedTb

impl LcBumpThree for LcSharedTb
    run
        dut.en = 1
        wait 3 cycles
        dut.en = 0
        expected = 3
    end run
end impl LcBumpThree

impl LcBumpFive for LcSharedTb
    run
        dut.en = 1
        wait 5 cycles
        dut.en = 0
        expected = 5
    end run
end impl LcBumpFive
";

const LC_TEST_NAMES: [&str; 2] = ["LcBumpThree", "LcBumpFive"];

/// `harc sim --sv … --top LcCounter --cpp-split tests
/// --cpp-split-group-size N` for the lifecycle suite.
fn run_lc_split_build(
    outdir: &Path,
    sv: &Path,
    tb: &Path,
    codegen: &str,
    group_size: Option<u32>,
) -> (bool, String) {
    let mut cmd = Command::new(harc_bin());
    cmd.arg("sim")
        .arg("--sv")
        .arg(sv)
        .arg(tb)
        .arg("--top")
        .arg("LcCounter")
        .arg("--codegen")
        .arg(codegen)
        .arg("--cpp-split")
        .arg("tests")
        .arg("--jobs")
        .arg("2")
        .arg("--outdir")
        .arg(outdir);
    if let Some(n) = group_size {
        cmd.arg("--cpp-split-group-size").arg(n.to_string());
    }
    let out = cmd.output().expect("spawn harc sim");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    (
        out.status.success(),
        format!("--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"),
    )
}

/// Dispatch every lifecycle test by name against `<outdir>/obj_dir/VLcCounter`
/// and assert each passes (an unknown name is rejected).
fn assert_lc_tests_dispatch(outdir: &Path, label: &str) {
    let bin = outdir.join("obj_dir/VLcCounter");
    assert!(
        bin.exists(),
        "[{label}] lifecycle split build did not produce a linked binary at {}",
        bin.display()
    );
    for name in LC_TEST_NAMES {
        let out = Command::new(&bin)
            .arg("--test")
            .arg(name)
            .output()
            .unwrap_or_else(|e| panic!("[{label}] spawn {} --test {name}: {e}", bin.display()));
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            combined.contains("ALL TESTS PASSED"),
            "[{label}] dispatch of `{name}` did not pass; output:\n{combined}"
        );
        assert!(
            !combined.contains("TESTS FAILED"),
            "[{label}] dispatch of `{name}` reported a failure; output:\n{combined}"
        );
    }
    let out = Command::new(&bin)
        .arg("--test")
        .arg("NoSuchTest")
        .output()
        .expect("spawn unknown-test dispatch");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("unknown test"),
        "[{label}] unknown test name should be reported, got:\n{combined}"
    );
}

#[test]
fn split_build_shared_lifecycle_coro_links_and_dispatches_e2e() {
    if !verilator_present() {
        eprintln!(
            "SKIP split_build_shared_lifecycle_coro_links_and_dispatches_e2e: `verilator` not \
             found on PATH. This test compiles and links the generated split shards through \
             Verilator."
        );
        return;
    }

    let dir = fresh_outdir("lc_inputs");
    let sv = dir.join("LcCounter.sv");
    let tb = dir.join("lc_shared_tb.harc");
    std::fs::write(&sv, LC_COUNTER_SV).expect("write DUT");
    std::fs::write(&tb, LC_SHARED_TB).expect("write TB");

    let mut outdirs = Vec::new();
    for codegen in ["v1", "tbir"] {
        // group_size 1 → one impl per shard → two SELF-CONTAINED shard TUs
        // that each define the shared lifecycle bodies and link together.
        let out = fresh_outdir(&format!("lc_{codegen}_g1"));
        let (ok, log) = run_lc_split_build(&out, &sv, &tb, codegen, Some(1));
        assert!(ok, "{codegen} lifecycle split build failed:\n{log}");
        assert_lc_tests_dispatch(&out, &format!("{codegen} lc group=1"));

        // Under the native out-of-line default (`tbir`), each shard TU must
        // DEFINE the suspending setup coroutine with INTERNAL (`static`)
        // linkage — an external definition in two TUs is the duplicate-symbol
        // link error the real build above would have already rejected; this
        // pins WHY the link stayed clean so a future external-linkage
        // regression fails here with a legible message, not just a linker
        // wall of text. (v1 always inlines the lifecycle — no out-of-line
        // symbol — so this shape is tbir-only.)
        if codegen == "tbir" {
            let shards: Vec<PathBuf> = std::fs::read_dir(&out)
                .expect("read outdir")
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    let n = p.file_name().unwrap().to_string_lossy();
                    n.ends_with(".cpp") && n.contains("__test_")
                })
                .collect();
            assert!(
                shards.len() >= 2,
                "group_size 1 must produce >=2 self-contained shard TUs, got {}",
                shards.len()
            );
            for s in &shards {
                let body = std::fs::read_to_string(s).expect("read shard");
                assert!(
                    body.contains(
                        "static harc_rt::HarcThread _harc_lc__tb_lifecycle_LcSharedTb_Setup("
                    ),
                    "shard {} must define the shared setup coroutine with internal \
                     (static) linkage",
                    s.display()
                );
                // The `\n` anchor excludes the `static …` match above (which is
                // preceded by `static `, not a newline).
                assert!(
                    !body.contains("\nharc_rt::HarcThread _harc_lc__tb_lifecycle_LcSharedTb_Setup("),
                    "shard {} must NOT emit an EXTERNAL Coro definition (would be a \
                     duplicate-symbol link error across shards)",
                    s.display()
                );
            }
        }
        outdirs.push(out);
    }

    let _ = std::fs::remove_dir_all(&dir);
    for outdir in outdirs {
        let _ = std::fs::remove_dir_all(outdir);
    }
}
