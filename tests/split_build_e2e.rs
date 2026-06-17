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
//!   1. `--cpp-split tests` at the default group size (6 tests → 2 shards
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

/// Names of the six tests emitted into the testbench below. Six tests at
/// the default group size of 4 yields two shards, so the default path
/// genuinely links more than one shard TU.
const TEST_NAMES: [&str; 6] = ["T1", "T2", "T3", "T4", "T5", "T6"];

fn harc_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_harc"))
}

/// Whether a `verilator` binary is on PATH. `--cpp-split` only needs a
/// working C++ toolchain + Verilator; unlike the trace-merge e2e it has no
/// minimum-version requirement (no `--trace-vcd`).
fn verilator_present() -> bool {
    Command::new("verilator")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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
    let mut s = String::new();
    for (name, (a, b, sum)) in TEST_NAMES.iter().zip(cases) {
        s.push_str(&format!(
            "test {name}\n    let dut : SplitAdder\n    run\n        dut.a = {a}\n        dut.b = {b}\n        wait 1 cycle\n        assert dut.sum == {sum}\n    end run\nend test {name}\n"
        ));
    }
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
        // 1. Default group size (4): 6 tests → 2 shards + dispatcher. This is
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
