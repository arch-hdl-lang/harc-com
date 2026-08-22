//! End-to-end regression for `harc sim --cpp-split tests
//! --cpp-split-layout common` (issue #643, v1-first common-object C++
//! emission for scalable HARC suites).
//!
//! The common layout is the compile-once/run-many contract: reusable
//! runtime/testbench/component infrastructure compiles into shared
//! objects; each test emits a small stable capsule; an explicit
//! registry dispatches one selected test per process. These tests pin:
//!
//! 1. artifact shape (header + runtime + per-test capsules + registry,
//!    stable filenames independent of test order);
//! 2. structural non-duplication (shared sentinels appear once, test
//!    bodies only in their own capsule);
//! 3. real Verilator build + dispatch (default/selected/unknown);
//! 4. incremental-build identity (edit-one-test / add-first-test /
//!    seed-only runs rewrite exactly the required artifact set).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn harc_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_harc"))
}

fn verilator_present() -> bool {
    Command::new("verilator")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn fresh_outdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "harc_common_split_e2e_{}_{}",
        tag,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp outdir");
    dir
}

const ADDER_SV: &str = "\
module SplitAdder(input logic [7:0] a, input logic [7:0] b, output logic [8:0] sum);
  always_comb sum = a + b;
endmodule
";

/// Three tests with distinct sums so a mis-dispatch surfaces as an
/// assertion failure rather than a false pass.
const TB_THREE_TESTS: &str = "\
test T1Add
    let dut : SplitAdder
    run
        dut.a = 1
        dut.b = 2
        wait 1 cycle
        assert dut.sum == 3
    end run
end test T1Add

test T2Add
    let dut : SplitAdder
    run
        dut.a = 10
        dut.b = 20
        wait 1 cycle
        assert dut.sum == 30
    end run
end test T2Add

test T3Add
    let dut : SplitAdder
    run
        dut.a = 100
        dut.b = 50
        wait 1 cycle
        assert dut.sum == 150
    end run
end test T3Add
";

fn write_suite(dir: &Path, tb: &str) -> PathBuf {
    let sv = dir.join("dut.sv");
    fs::write(&sv, ADDER_SV).unwrap();
    let tb_path = dir.join("tb.harc");
    fs::write(&tb_path, tb).unwrap();
    tb_path
}

/// Run `harc sim --sv … --codegen v1 --cpp-split tests
/// --cpp-split-layout common` and return (success, combined output).
fn run_common_split(
    tb: &Path,
    sv: &Path,
    outdir: &Path,
    extra_args: &[&str],
) -> (bool, String) {
    let mut cmd = Command::new(harc_bin());
    cmd.arg("sim")
        .arg(tb)
        .arg("--sv")
        .arg(sv)
        .arg("--codegen")
        .arg("v1")
        .arg("--cpp-split")
        .arg("tests")
        .arg("--cpp-split-layout")
        .arg("common")
        .arg("--outdir")
        .arg(outdir);
    for a in extra_args {
        cmd.arg(a);
    }
    let out = cmd.output().expect("spawn harc sim");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

#[test]
fn common_layout_requires_v1_and_tests_mode() {
    let dir = fresh_outdir("gates");
    let tb = write_suite(&dir, TB_THREE_TESTS);

    // Default (tbir) codegen must be rejected before emission.
    let out = Command::new(harc_bin())
        .arg("sim")
        .arg(&tb)
        .arg("--sv")
        .arg(dir.join("dut.sv"))
        .arg("--cpp-split")
        .arg("tests")
        .arg("--cpp-split-layout")
        .arg("common")
        .arg("--outdir")
        .arg(dir.join("o1"))
        .output()
        .unwrap();
    assert!(!out.status.success(), "tbir + common must be rejected");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("--codegen v1"), "got: {text}");

    // The layout flag requires `--cpp-split tests`.
    let out = Command::new(harc_bin())
        .arg("sim")
        .arg(&tb)
        .arg("--sv")
        .arg(dir.join("dut.sv"))
        .arg("--codegen")
        .arg("v1")
        .arg("--cpp-split-layout")
        .arg("common")
        .arg("--outdir")
        .arg(dir.join("o2"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("--cpp-split tests"), "got: {text}");

    // Group size is meaningless in per-test-capsule mode — reject
    // instead of silently changing ownership.
    let (ok, msg) = run_common_split(
        &tb,
        &dir.join("dut.sv"),
        &dir.join("o3"),
        &["--cpp-split-group-size", "2"],
    );
    assert!(!ok);
    assert!(msg.contains("group-size"), "got: {msg}");

    fs::remove_dir_all(&dir).ok();
}

/// Artifact shape + structural non-duplication, no Verilator needed.
#[test]
fn common_layout_emits_expected_artifacts_without_duplication() {
    let dir = fresh_outdir("shape");
    let tb = write_suite(&dir, TB_THREE_TESTS);
    let outdir = dir.join("out");
    let (ok, msg) = run_common_split(&tb, &dir.join("dut.sv"), &outdir, &["--emit-only"]);
    assert!(ok, "emit failed: {msg}");

    // Exactly the contracted artifact set, plus the manifest.
    let names: Vec<String> = ["tb__suite_api.hpp", "tb__runtime.cpp", "tb__registry.cpp"]
        .iter()
        .map(|s| s.to_string())
        .chain(
            ["T1Add", "T2Add", "T3Add"]
                .iter()
                .map(|t| format!("tb__test_{t}.cpp")),
        )
        .collect();
    for n in &names {
        assert!(
            outdir.join(n).is_file(),
            "missing artifact {n}; emitted:\n{msg}"
        );
    }
    assert!(outdir.join("tb__artifacts.json").is_file());

    // Manifest records both fingerprints and the exact test list.
    let manifest = fs::read_to_string(outdir.join("tb__artifacts.json")).unwrap();
    assert!(manifest.contains("\"schema_version\":1"));
    assert!(manifest.contains("\"interface_abi\""));
    assert!(manifest.contains("\"build_profile\""));
    for t in ["T1Add", "T2Add", "T3Add"] {
        assert!(manifest.contains(&format!("\"{t}\"")));
    }

    // Structural non-duplication: every capsule defines its own run_
    // entry and no other test's scenario appears in it.
    for (i, t) in ["T1Add", "T2Add", "T3Add"].iter().enumerate() {
        let capsule = fs::read_to_string(outdir.join(format!("tb__test_{t}.cpp"))).unwrap();
        assert!(
            capsule.contains(&format!("int run_{t}(int argc, char** argv)")),
            "{t} capsule missing its run entry"
        );
        assert!(capsule.contains("HarcSuiteRuntime ctx;"));
        assert!(capsule.contains("harc_rt_current_run = &ctx;"));
        assert!(
            !capsule.contains("struct HarcSuiteRuntime {"),
            "capsule must not redefine the runtime"
        );
        for (j, other) in ["T1Add", "T2Add", "T3Add"].iter().enumerate() {
            if i == j {
                continue;
            }
            assert!(
                !capsule.contains(&format!("int run_{other}(")),
                "{t} capsule leaked {other}'s dispatcher symbol"
            );
        }
    }

    // Common TU owns the runtime machinery exactly once.
    let runtime = fs::read_to_string(outdir.join("tb__runtime.cpp")).unwrap();
    assert_eq!(
        runtime.matches("void HarcSuiteGlue::tick()").count(),
        1,
        "tick defined once"
    );
    assert_eq!(
        runtime.matches("HarcSuiteGlue::HarcSuiteGlue(").count(),
        2,
        "both glue constructors defined in the common TU"
    );

    // Registry is table-driven and references every descriptor.
    let registry = fs::read_to_string(outdir.join("tb__registry.cpp")).unwrap();
    for t in ["T1Add", "T2Add", "T3Add"] {
        assert!(registry.contains(&format!("&harc_test_{t}")));
    }
    assert!(registry.contains("harc_report_unknown_test"));

    // Interface ABI anchor present in header and referenced by every TU.
    let header = fs::read_to_string(outdir.join("tb__suite_api.hpp")).unwrap();
    assert!(header.contains("// === iface-begin ==="));
    assert!(header.contains("// === iface-end ==="));
    let anchor_line = header
        .lines()
        .find(|l| l.starts_with("extern const char harc_suite_abi_"))
        .expect("anchor declaration");
    let anchor = anchor_line
        .trim_start_matches("extern const char ")
        .split('[')
        .next()
        .unwrap()
        .to_string();
    assert!(runtime.contains(&format!("const char {anchor}[] =")), "common TU defines the anchor");
    for t in ["T1Add", "T2Add", "T3Add"] {
        let capsule = fs::read_to_string(outdir.join(format!("tb__test_{t}.cpp"))).unwrap();
        assert!(capsule.contains(&anchor), "{t} capsule references the ABI anchor");
    }

    fs::remove_dir_all(&dir).ok();
}

/// Determinism: two clean emissions are byte-identical.
#[test]
fn common_layout_emission_is_deterministic() {
    let dir = fresh_outdir("determinism");
    let tb = write_suite(&dir, TB_THREE_TESTS);
    let o1 = dir.join("run1");
    let o2 = dir.join("run2");
    let (ok, msg) = run_common_split(&tb, &dir.join("dut.sv"), &o1, &["--emit-only"]);
    assert!(ok, "{msg}");
    let (ok, msg) = run_common_split(&tb, &dir.join("dut.sv"), &o2, &["--emit-only"]);
    assert!(ok, "{msg}");
    for n in [
        "tb__suite_api.hpp",
        "tb__runtime.cpp",
        "tb__registry.cpp",
        "tb__test_T1Add.cpp",
        "tb__test_T2Add.cpp",
        "tb__test_T3Add.cpp",
        "tb__artifacts.json",
    ] {
        let a = fs::read(o1.join(n)).unwrap_or_else(|_| panic!("{n} missing in run1"));
        let b = fs::read(o2.join(n)).unwrap_or_else(|_| panic!("{n} missing in run2"));
        assert_eq!(a, b, "{n} differs across identical runs");
    }
    fs::remove_dir_all(&dir).ok();
}

/// Seed/test-selection provenance never enters generated bytes.
#[test]
fn seed_changes_nothing_on_disk() {
    let dir = fresh_outdir("seed");
    let tb = write_suite(&dir, TB_THREE_TESTS);
    let outdir = dir.join("out");
    let (ok, _) = run_common_split(&tb, &dir.join("dut.sv"), &outdir, &["--emit-only"]);
    assert!(ok);
    let snap_before: Vec<(String, Vec<u8>)> = ["tb__suite_api.hpp", "tb__runtime.cpp"]
        .iter()
        .map(|n| (n.to_string(), fs::read(outdir.join(n)).unwrap()))
        .collect();
    // Re-run under a different HARC_SEED: every artifact must be reused.
    let mut cmd = Command::new(harc_bin());
    cmd.arg("sim")
        .arg(&tb)
        .arg("--sv")
        .arg(dir.join("dut.sv"))
        .arg("--codegen")
        .arg("v1")
        .arg("--cpp-split")
        .arg("tests")
        .arg("--cpp-split-layout")
        .arg("common")
        .arg("--outdir")
        .arg(&outdir)
        .env("HARC_SEED", "424242");
    let out = cmd.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    // 6 suite artifacts (manifest write is silent when unchanged).
    assert_eq!(text.matches("(unchanged)").count(), 6, "expected all artifacts reused: {text}");
    for (n, bytes) in snap_before {
        assert_eq!(bytes, fs::read(outdir.join(&n)).unwrap(), "{n}");
    }
    fs::remove_dir_all(&dir).ok();
}

/// Real build + dispatch. Skipped when Verilator is absent (same policy
/// as the self-contained split e2e).
#[test]
fn common_layout_builds_links_and_dispatches() {
    if !verilator_present() {
        eprintln!("skipping: verilator not on PATH");
        return;
    }
    let dir = fresh_outdir("build");
    let tb = write_suite(&dir, TB_THREE_TESTS);
    let outdir = dir.join("out");

    // Default selection = first test in canonical source order.
    let (ok, msg) = run_common_split(&tb, &dir.join("dut.sv"), &outdir, &[]);
    assert!(ok, "default run failed: {msg}");
    assert!(msg.contains("ALL TESTS PASSED"), "{msg}");

    // Every explicit selection passes against the same binary.
    for t in ["T1Add", "T2Add", "T3Add"] {
        let (ok, msg) =
            run_common_split(&tb, &dir.join("dut.sv"), &outdir, &["--test", t]);
        assert!(ok, "{t} failed: {msg}");
        assert!(msg.contains("ALL TESTS PASSED"), "{t}: {msg}");
    }

    // Unknown selection fails clearly without falling through to a
    // different test.
    let bin = outdir.join("obj_dir").join("VSplitAdder");
    let unknown = Command::new(&bin).arg("--test").arg("NoSuch").output().unwrap();
    assert!(!unknown.status.success());
    let text = String::from_utf8_lossy(&unknown.stderr);
    assert!(text.contains("unknown test: NoSuch"), "{text}");
    assert!(text.contains("available:"), "{text}");

    fs::remove_dir_all(&dir).ok();
}

/// Incremental-build identity (issue #643 invalidation matrix).
#[test]
fn common_layout_incremental_rewrites_only_required_artifacts() {
    if !verilator_present() {
        eprintln!("skipping: verilator not on PATH");
        return;
    }
    let dir = fresh_outdir("incr");
    let tb_path = dir.join("tb.harc");
    let sv = dir.join("dut.sv");
    fs::write(&sv, ADDER_SV).unwrap();

    // Baseline: three tests.
    fs::write(&tb_path, TB_THREE_TESTS).unwrap();
    let outdir = dir.join("out");
    let (ok, msg) = run_common_split(&tb_path, &sv, &outdir, &[]);
    assert!(ok, "{msg}");
    let read = |n: &str| fs::read_to_string(outdir.join(n)).unwrap();
    let baseline: Vec<(String, String)> = [
        "tb__suite_api.hpp",
        "tb__runtime.cpp",
        "tb__test_T1Add.cpp",
        "tb__test_T2Add.cpp",
        "tb__test_T3Add.cpp",
        "tb__registry.cpp",
    ]
    .iter()
    .map(|n| (n.to_string(), read(n)))
    .collect();

    // 1) Edit ONE test body → only that capsule changes.
    // Same-sum operand change: bytes must shift, verdict must not.
    let edited = TB_THREE_TESTS.replacen("dut.a = 10\n        dut.b = 20", "dut.a = 12\n        dut.b = 18", 1);
    assert_ne!(edited, TB_THREE_TESTS);
    fs::write(&tb_path, &edited).unwrap();
    let (ok, msg) = run_common_split(&tb_path, &sv, &outdir, &[]);
    assert!(ok, "{msg}");
    for (n, before) in &baseline {
        let after = read(n);
        if n == "tb__test_T2Add.cpp" {
            assert_ne!(before, &after, "{n} should have been rewritten");
        } else {
            assert_eq!(before, &after, "{n} must stay byte-identical after a T2 edit");
        }
    }
    // And the edited suite still passes end to end.
    let bin = outdir.join("obj_dir").join("VSplitAdder");
    let run = Command::new(&bin).arg("--test").arg("T2Add").output().unwrap();
    assert!(run.status.success());
    // Restore baseline for the next mutation.
    fs::write(&tb_path, TB_THREE_TESTS).unwrap();
    let (ok, _) = run_common_split(&tb_path, &sv, &outdir, &[]);
    assert!(ok);

    // 2) Add a new FIRST-sorted test → new capsule + registry only;
    //    every prior capsule and the common artifacts stay identical.
    let with_new = format!("{TB_THREE_TESTS}\ntest A0First\n    let dut : SplitAdder\n    run\n        dut.a = 7\n        dut.b = 8\n        wait 1 cycle\n        assert dut.sum == 15\n    end run\nend test A0First\n");
    fs::write(&tb_path, &with_new).unwrap();
    let (ok, msg) = run_common_split(&tb_path, &sv, &outdir, &[]);
    assert!(ok, "{msg}");
    assert!(outdir.join("tb__test_A0First.cpp").is_file());
    for (n, before) in &baseline {
        let after = read(n);
        match n.as_str() {
            "tb__registry.cpp" => assert_ne!(before, &after, "registry must gain A0First"),
            _ => assert_eq!(before, &after, "{n} must be untouched by add-test"),
        }
    }
    // New test dispatches from the same binary.
    let run = Command::new(&bin).arg("--test").arg("A0First").output().unwrap();
    assert!(run.status.success());
    // Restore baseline.
    fs::write(&tb_path, TB_THREE_TESTS).unwrap();
    let (ok, _) = run_common_split(&tb_path, &sv, &outdir, &[]);
    assert!(ok);

    // 3) Delete a test → registry updates; stale capsule omitted by the
    //    manifest contract (removed from disk by regeneration).
    let removed = TB_THREE_TESTS
        .replace(
            "test T3Add\n    let dut : SplitAdder\n    run\n        dut.a = 100\n        dut.b = 50\n        wait 1 cycle\n        assert dut.sum == 150\n    end run\nend test T3Add\n",
            "",
        )
        .trim_end()
        .to_string();
    fs::write(&tb_path, &removed).unwrap();
    let (ok, msg) = run_common_split(&tb_path, &sv, &outdir, &[]);
    assert!(ok, "{msg}");
    assert!(!outdir.join("tb__test_T3Add.cpp").exists(), "stale capsule must be cleaned up");
    let manifest = read("tb__artifacts.json");
    assert!(!manifest.contains("T3Add"));
    assert!(manifest.contains("T1Add"));
    let run = Command::new(&bin).arg("--test").arg("T1Add").output().unwrap();
    assert!(run.status.success(), "remaining suite still runs");
    let unknown = Command::new(&bin).arg("--test").arg("T3Add").output().unwrap();
    // The relinked binary no longer contains T3Add's descriptor or
    // object, so selection fails clearly instead of falling through.
    let text = String::from_utf8_lossy(&unknown.stderr);
    assert!(
        text.contains("unknown test: T3Add"),
        "expected clear unknown-test failure, got: {text}"
    );

    fs::remove_dir_all(&dir).ok();
}

/// Suites with suite-level functions that call each other (#301 shape),
/// a hookable transactor with pre/post hooks, and a watchdog — the
/// classes of composition the adder suite cannot exercise. Emit-level
/// only: verifies shared callables land in the common TU exactly once
/// and capsules reference them through `_glue.`.
#[test]
fn common_layout_composes_functions_hooks_and_watchdogs() {
    let dir = fresh_outdir("compose");
    let sv = dir.join("dut.sv");
    fs::write(&sv, ADDER_SV).unwrap();
    let tb = dir.join("tb.harc");
    fs::write(
        &tb,
        r#"transaction RegOp
    addr : uint<8>
end transaction RegOp

transactor Drv
    stamp : uint<8> = 0
    hookable send(t: RegOp)
        self.stamp = t.addr
    end hookable
    watchdog
        period 50 cycles
        max_idle 100000 cycles
    end watchdog
end transactor Drv

function inner(x: uint<8>) -> uint<8>
    return x + 1
end function inner

function outer(x: uint<8>) -> uint<8>
    return inner(x) * 2
end function outer

test TCompose
    let dut : SplitAdder
    let drv : Drv = drv()
    on drv.send pre
        log(info, "pre-send")
    end on
    run
        dut.a = outer(3)
        dut.b = 1
        wait 1 cycle
        let t : RegOp
        t.addr = 7
        drv.send(t)
        assert drv.stamp == 7 else fail("hookable call")
        assert dut.sum == 9
    end run
end test TCompose
"#,
    )
    .unwrap();
    let outdir = dir.join("out");
    let (ok, msg) = run_common_split(&tb, &sv, &outdir, &["--emit-only"]);
    assert!(ok, "emit failed: {msg}");

    // Shared callables defined ONCE in the common TU as glue members.
    let runtime = fs::read_to_string(outdir.join("tb__runtime.cpp")).unwrap();
    assert_eq!(runtime.matches("uint64_t HarcSuiteGlue::inner(").count(), 1);
    assert_eq!(runtime.matches("uint64_t HarcSuiteGlue::outer(").count(), 1);
    assert_eq!(runtime.matches("void HarcSuiteGlue::Drv_send(").count(), 1);
    assert_eq!(runtime.matches("void HarcSuiteGlue::Drv_watchdog(").count(), 1);
    // `outer` calls `inner` WITHOUT a _glue prefix inside the member body.
    assert!(
        !runtime.contains("_glue.inner("),
        "common-TU members resolve siblings by bare name"
    );

    // Hook registries live on the runtime; capsule registers into them.
    let header = fs::read_to_string(outdir.join("tb__suite_api.hpp")).unwrap();
    assert!(header.contains("Drv_send_pre"));
    assert!(header.contains("Drv_watchdog_pre"));
    let capsule = fs::read_to_string(outdir.join("tb__test_TCompose.cpp")).unwrap();
    assert!(capsule.contains("_glue.Drv_send(drv") || capsule.contains("_glue.Drv_send("));

    fs::remove_dir_all(&dir).ok();
}

/// Dual-clock suites keep legacy edge semantics through the glue clock
/// machine (initial level 0, first rising edge at half-period,
/// cycle_count on primary rising edges).
#[test]
fn common_layout_dual_clock_builds_and_passes() {
    if !verilator_present() {
        eprintln!("skipping: verilator not on PATH");
        return;
    }
    let dir = fresh_outdir("dualclk");
    let sv = dir.join("dut.sv");
    fs::write(
        &sv,
        "module DualClk(input logic clk_a, input logic clk_b, input logic d, output logic q);\n\
         always_ff @(posedge clk_a) q <= d;\nendmodule\n",
    )
    .unwrap();
    let tb = dir.join("tb.harc");
    fs::write(
        &tb,
        "test TDual\n    let dut : DualClk\n    clock clk_a = 10ns\n    clock clk_b = 4ns\n    run\n        dut.d = 1\n        wait 2 cycles\n        assert dut.q == 1 else fail(\"dual clk capture\")\n    end run\nend test TDual\n",
    )
    .unwrap();
    let outdir = dir.join("out");
    let (ok, msg) = run_common_split(&tb, &sv, &outdir, &[]);
    assert!(ok, "dual-clock run failed: {msg}");
    assert!(msg.contains("ALL TESTS PASSED"), "{msg}");

    // Compile-skip proof: an unchanged rerun must not recompile any
    // object (mtime preservation is the whole point of write_if_changed
    // + stable bytes).
    let obj_dir = outdir.join("obj_dir");
    let mtime = |p: &Path| fs::metadata(p).unwrap().modified().unwrap();
    let before: Vec<(PathBuf, std::time::SystemTime)> = ["tb__runtime", "tb__test_TDual"]
        .iter()
        .map(|stem| {
            let p = obj_dir.join(format!("{stem}.o"));
            (p.clone(), mtime(&p))
        })
        .collect();
    let (ok, msg) = run_common_split(&tb, &sv, &outdir, &[]);
    assert!(ok, "{msg}");
    for (p, t) in before {
        assert_eq!(t, mtime(&p), "{} was recompiled on an unchanged rerun", p.display());
    }

    fs::remove_dir_all(&dir).ok();
}
