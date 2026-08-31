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
fn run_common_split(tb: &Path, sv: &Path, outdir: &Path, extra_args: &[&str]) -> (bool, String) {
    run_common_split_codegen("v1", tb, sv, outdir, extra_args)
}

fn run_common_split_codegen(
    codegen: &str,
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
        .arg(codegen)
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
fn common_layout_requires_tests_mode_and_tbir_accepts_multi_test_suites() {
    let dir = fresh_outdir("gates");
    let tb = write_suite(&dir, TB_THREE_TESTS);

    // The default backend is TB-IR, and explicit TB-IR selection must expose
    // the same multi-test common-layout surface.
    for explicit_codegen in [false, true] {
        let outdir = if explicit_codegen {
            dir.join("o1_explicit")
        } else {
            dir.join("o1_default")
        };
        let mut cmd = Command::new(harc_bin());
        cmd.arg("sim").arg(&tb).arg("--sv").arg(dir.join("dut.sv"));
        if explicit_codegen {
            cmd.arg("--codegen").arg("tbir");
        }
        let out = cmd
            .arg("--cpp-split")
            .arg("tests")
            .arg("--cpp-split-layout")
            .arg("common")
            .arg("--outdir")
            .arg(&outdir)
            .arg("--emit-only")
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "TB-IR common emit failed: {text}");
        assert!(outdir.join("tb__runtime.cpp").is_file(), "got: {text}");
        assert!(outdir.join("tb__registry.cpp").is_file(), "got: {text}");
    }

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
    let manifest_json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(manifest_json["schema_version"], 2);
    assert_eq!(manifest_json["backend"], "v1");
    assert_eq!(manifest_json["layout"], "common");
    assert!(manifest_json["interface_abi"].is_string());
    assert!(manifest_json["build_profile"].is_string());
    assert!(manifest_json["artifacts"]
        .as_array()
        .is_some_and(|artifacts| artifacts.iter().all(|artifact| {
            artifact["filename"].is_string()
                && artifact["role"].is_string()
                && artifact["owner"].is_string()
                && artifact["tests"].is_array()
                && artifact["dependencies"].is_array()
        })));
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
        assert!(!capsule.contains("thread_local"));
        assert!(!capsule.contains("harc_rt_current_run"));
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
    assert!(
        runtime.contains(&format!("const char {anchor}[] =")),
        "common TU defines the anchor"
    );
    for t in ["T1Add", "T2Add", "T3Add"] {
        let capsule = fs::read_to_string(outdir.join(format!("tb__test_{t}.cpp"))).unwrap();
        assert!(
            capsule.contains(&anchor),
            "{t} capsule references the ABI anchor"
        );
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn v1_common_probe_stub_is_typed_and_manifest_owned() {
    let dir = fresh_outdir("v1_probe_manifest");
    let sv = dir.join("probe.sv");
    fs::write(
        &sv,
        "module ProbeTop(input logic clk); logic internal_status; endmodule\n",
    )
    .unwrap();
    let tb = dir.join("probe.harc");
    fs::write(
        &tb,
        r#"test ProbeTest
    let dut : ProbeTop
        probe status : uint<1> at internal_status
    end let dut
    run
        wait 1 cycle
        assert dut.status == 0
    end run
end test ProbeTest
"#,
    )
    .unwrap();
    let outdir = dir.join("out");
    let (ok, msg) = run_common_split(&tb, &sv, &outdir, &["--emit-only"]);
    assert!(ok, "{msg}");
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(outdir.join("probe__artifacts.json")).unwrap())
            .unwrap();
    assert!(manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|artifact| {
            artifact["filename"] == "probe__probe_stub.sv" && artifact["role"] == "probe_stub"
        }));
    assert!(outdir.join("probe__probe_stub.sv").is_file());
    assert!(!outdir.join("__harc_probe_ProbeTop.sv").exists());
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn common_manifest_owns_every_bundled_runtime_header() {
    let runtime_headers = [
        "harc_thread_rt.h",
        "harc_random_rt.h",
        "harc_queue_rt.h",
        "harc_trace_rt.h",
        "harc_log_rt.h",
        "harc_z3_rt.h",
    ];

    for codegen in ["v1", "tbir"] {
        let dir = fresh_outdir(&format!("runtime_header_manifest_{codegen}"));
        let tb = write_suite(&dir, TB_THREE_TESTS);
        let outdir = dir.join("out");
        let (ok, msg) =
            run_common_split_codegen(codegen, &tb, &dir.join("dut.sv"), &outdir, &["--emit-only"]);
        assert!(ok, "{codegen} common emit failed: {msg}");

        let manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(outdir.join("tb__artifacts.json")).unwrap())
                .unwrap();
        let artifacts = manifest["artifacts"].as_array().unwrap();
        for header in runtime_headers {
            assert!(
                artifacts.iter().any(|artifact| {
                    artifact["filename"] == header && artifact["role"] == "runtime_header"
                }),
                "{codegen} manifest does not own {header}: {manifest}"
            );
            assert!(outdir.join(header).is_file(), "missing {codegen} {header}");
        }
        fs::remove_dir_all(&dir).ok();
    }
}

#[test]
fn runtime_header_promotion_failure_never_publishes_a_manifest() {
    for codegen in ["v1", "tbir"] {
        let dir = fresh_outdir(&format!("runtime_header_failure_{codegen}"));
        let tb = write_suite(&dir, TB_THREE_TESTS);
        let outdir = dir.join("out");
        fs::create_dir_all(outdir.join("harc_trace_rt.h")).unwrap();

        let (ok, msg) =
            run_common_split_codegen(codegen, &tb, &dir.join("dut.sv"), &outdir, &["--emit-only"]);
        assert!(
            !ok,
            "{codegen} runtime-header failure unexpectedly passed: {msg}"
        );
        assert!(
            !outdir.join("tb__artifacts.json").exists(),
            "{codegen} published a trusted manifest before all runtime headers"
        );
        assert!(outdir.join(".tb__artifacts.json.pending").is_file());
        fs::remove_dir_all(&dir).ok();
    }
}

#[test]
fn trace_macro_mode_changes_the_common_interface_abi() {
    for codegen in ["v1", "tbir"] {
        let dir = fresh_outdir(&format!("trace_abi_{codegen}"));
        let tb = write_suite(&dir, TB_THREE_TESTS);
        let sv = dir.join("dut.sv");
        let read_abi = |outdir: &Path| {
            let manifest: serde_json::Value = serde_json::from_str(
                &fs::read_to_string(outdir.join("tb__artifacts.json")).unwrap(),
            )
            .unwrap();
            manifest["interface_abi"].as_str().unwrap().to_string()
        };

        let plain = dir.join("plain");
        let (ok, msg) = run_common_split_codegen(codegen, &tb, &sv, &plain, &["--emit-only"]);
        assert!(ok, "{codegen} no-trace emit failed: {msg}");

        let vcd = dir.join("vcd");
        let (ok, msg) = run_common_split_codegen(
            codegen,
            &tb,
            &sv,
            &vcd,
            &["--emit-only", "--waves", "--wave-format", "vcd"],
        );
        assert!(ok, "{codegen} VCD emit failed: {msg}");

        let fst = dir.join("fst");
        let (ok, msg) = run_common_split_codegen(
            codegen,
            &tb,
            &sv,
            &fst,
            &["--emit-only", "--waves", "--wave-format", "fst"],
        );
        assert!(ok, "{codegen} FST emit failed: {msg}");

        assert_ne!(
            read_abi(&plain),
            read_abi(&vcd),
            "{codegen} no-trace/VCD ABI"
        );
        assert_ne!(read_abi(&vcd), read_abi(&fst), "{codegen} VCD/FST ABI");
        fs::remove_dir_all(&dir).ok();
    }
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

#[test]
fn manifest_sources_reports_only_typed_owned_native_units() {
    let dir = fresh_outdir("manifest_sources");
    let tb = write_suite(&dir, TB_THREE_TESTS);
    let outdir = dir.join("out");
    let (ok, msg) = run_common_split(&tb, &dir.join("dut.sv"), &outdir, &["--emit-only"]);
    assert!(ok, "{msg}");
    let stale = outdir.join("tb__test_Stale.cpp");
    fs::write(&stale, "this must never be compiled\n").unwrap();

    let output = Command::new(harc_bin())
        .arg("manifest-sources")
        .arg(outdir.join("tb__artifacts.json"))
        .output()
        .expect("read manifest source list");
    assert!(
        output.status.success(),
        "manifest reader failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sources = String::from_utf8(output.stdout).unwrap();
    let expected = [
        "tb__runtime.cpp",
        "tb__test_T1Add.cpp",
        "tb__test_T2Add.cpp",
        "tb__test_T3Add.cpp",
        "tb__registry.cpp",
    ];
    assert_eq!(
        sources.lines().collect::<Vec<_>>(),
        expected
            .iter()
            .map(|name| outdir.join(name).display().to_string())
            .collect::<Vec<_>>()
    );
    assert!(!sources.contains("Stale"));

    let output = Command::new(harc_bin())
        .arg("manifest-sources")
        .arg("--all-artifacts")
        .arg(outdir.join("tb__artifacts.json"))
        .output()
        .expect("read complete manifest build-input list");
    assert!(
        output.status.success(),
        "manifest all-artifact reader failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let build_inputs = String::from_utf8(output.stdout).unwrap();
    for name in [
        "tb__suite_api.hpp",
        "tb__runtime.cpp",
        "tb__test_T1Add.cpp",
        "tb__test_T2Add.cpp",
        "tb__test_T3Add.cpp",
        "tb__registry.cpp",
        "harc_thread_rt.h",
        "harc_random_rt.h",
        "harc_queue_rt.h",
        "harc_trace_rt.h",
        "harc_log_rt.h",
        "harc_z3_rt.h",
    ] {
        assert!(
            build_inputs
                .lines()
                .any(|path| path == outdir.join(name).display().to_string()),
            "manifest build inputs omitted {name}: {build_inputs}"
        );
    }
    assert!(!build_inputs.contains("Stale"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn manifest_sources_remains_backward_compatible_with_schema_one() {
    let dir = fresh_outdir("manifest_sources_v1");
    let tb = write_suite(&dir, TB_THREE_TESTS);
    let outdir = dir.join("out");
    let (ok, msg) = run_common_split(&tb, &dir.join("dut.sv"), &outdir, &["--emit-only"]);
    assert!(ok, "{msg}");
    let manifest_path = outdir.join("tb__artifacts.json");
    let current: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
    let artifacts = current["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|artifact| artifact["role"] != "runtime_header")
        .map(|artifact| artifact["filename"].as_str().unwrap())
        .collect::<Vec<_>>();
    let legacy = serde_json::json!({
        "schema_version": 1,
        "interface_abi": current["interface_abi"],
        "build_profile": current["build_profile"],
        "tests": current["tests"],
        "artifacts": artifacts,
    });
    fs::write(
        &manifest_path,
        format!("{}\n", serde_json::to_string(&legacy).unwrap()),
    )
    .unwrap();

    let output = Command::new(harc_bin())
        .arg("manifest-sources")
        .arg(&manifest_path)
        .output()
        .expect("read schema-one manifest source list");
    assert!(
        output.status.success(),
        "schema-one manifest reader failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        [
            "tb__runtime.cpp",
            "tb__test_T1Add.cpp",
            "tb__test_T2Add.cpp",
            "tb__test_T3Add.cpp",
            "tb__registry.cpp",
        ]
        .iter()
        .map(|name| outdir.join(name).display().to_string())
        .collect::<Vec<_>>()
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn common_native_build_ignores_unowned_lookalike_sources() {
    if !verilator_present() {
        eprintln!("skipping: verilator not on PATH");
        return;
    }
    let dir = fresh_outdir("manifest_build_sources");
    let tb = write_suite(&dir, TB_THREE_TESTS);
    let sv = dir.join("dut.sv");
    let outdir = dir.join("out");
    let (ok, msg) = run_common_split(&tb, &sv, &outdir, &["--emit-only"]);
    assert!(ok, "{msg}");
    fs::write(
        outdir.join("tb__test_Unowned.cpp"),
        "#error unowned lookalike source was compiled\n",
    )
    .unwrap();

    let (ok, msg) = run_common_split(&tb, &sv, &outdir, &[]);
    assert!(ok, "native build consumed an unowned source:\n{msg}");
    assert!(outdir.join("tb__test_Unowned.cpp").is_file());
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn switching_common_and_self_contained_layouts_invalidates_native_objects() {
    if !verilator_present() {
        eprintln!("skipping: verilator not on PATH");
        return;
    }
    let dir = fresh_outdir("layout_switch");
    let tb = write_suite(&dir, TB_THREE_TESTS);
    let sv = dir.join("dut.sv");
    let outdir = dir.join("out");
    let (ok, msg) = run_common_split(&tb, &sv, &outdir, &[]);
    assert!(ok, "{msg}");
    let stale_marker = outdir.join("obj_dir/stale-common-object.o");
    fs::write(&stale_marker, "stale").unwrap();

    let output = Command::new(harc_bin())
        .arg("sim")
        .arg(&tb)
        .arg("--sv")
        .arg(&sv)
        .arg("--codegen")
        .arg("v1")
        .arg("--cpp-split")
        .arg("tests")
        .arg("--cpp-split-layout")
        .arg("self-contained")
        .arg("--cpp-split-group-size")
        .arg("1")
        .arg("--outdir")
        .arg(&outdir)
        .output()
        .expect("switch layout");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "layout switch failed:\n{log}");
    assert!(
        !stale_marker.exists(),
        "layout switch reused the common-object build directory"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn switching_v1_tbir_common_and_tbir_self_contained_never_reuses_native_objects() {
    if !verilator_present() {
        eprintln!("skipping: verilator not on PATH");
        return;
    }
    let dir = fresh_outdir("backend_layout_switch");
    let tb = write_suite(&dir, TB_THREE_TESTS);
    let sv = dir.join("dut.sv");
    let outdir = dir.join("out");

    let (ok, msg) = run_common_split(&tb, &sv, &outdir, &[]);
    assert!(ok, "v1 common build failed:\n{msg}");
    let stale_v1 = outdir.join("obj_dir/stale-v1-common-object.o");
    fs::write(&stale_v1, "stale").unwrap();

    let (ok, msg) = run_common_split_codegen("tbir", &tb, &sv, &outdir, &[]);
    assert!(ok, "TBIR common build failed after v1 common:\n{msg}");
    assert!(!stale_v1.exists(), "backend switch reused v1 objects");
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(outdir.join("tb__artifacts.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["backend"], "tbir");

    let object_paths = [
        "tb__runtime.o",
        "tb__test_T1Add.o",
        "tb__test_T2Add.o",
        "tb__test_T3Add.o",
        "tb__registry.o",
    ]
    .map(|name| outdir.join("obj_dir").join(name));
    let mtimes = object_paths
        .iter()
        .map(|path| fs::metadata(path).unwrap().modified().unwrap())
        .collect::<Vec<_>>();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let (ok, msg) = run_common_split_codegen("tbir", &tb, &sv, &outdir, &[]);
    assert!(ok, "unchanged TBIR common rebuild failed:\n{msg}");
    for (path, expected) in object_paths.iter().zip(mtimes) {
        assert_eq!(
            fs::metadata(path).unwrap().modified().unwrap(),
            expected,
            "unchanged TBIR common build recompiled {}",
            path.display()
        );
    }

    let stale_common = outdir.join("obj_dir/stale-tbir-common-object.o");
    fs::write(&stale_common, "stale").unwrap();
    let output = Command::new(harc_bin())
        .arg("sim")
        .arg(&tb)
        .arg("--sv")
        .arg(&sv)
        .arg("--codegen")
        .arg("tbir")
        .arg("--cpp-split")
        .arg("tests")
        .arg("--cpp-split-layout")
        .arg("self-contained")
        .arg("--cpp-split-group-size")
        .arg("1")
        .arg("--outdir")
        .arg(&outdir)
        .output()
        .expect("switch TBIR layout");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "TBIR layout switch failed:\n{log}");
    assert!(
        !stale_common.exists(),
        "TBIR layout switch reused common-layout objects"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn tbir_common_native_incrementality_is_capsule_local_and_ignores_deleted_objects() {
    if !verilator_present() {
        eprintln!("skipping: verilator not on PATH");
        return;
    }
    let dir = fresh_outdir("tbir_native_incrementality");
    let tb = write_suite(&dir, TB_THREE_TESTS);
    let sv = dir.join("dut.sv");
    let outdir = dir.join("out");
    let (ok, msg) = run_common_split_codegen("tbir", &tb, &sv, &outdir, &[]);
    assert!(ok, "baseline TBIR common build failed:\n{msg}");

    let manifest_profile = || {
        let manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(outdir.join("tb__artifacts.json")).unwrap())
                .unwrap();
        manifest["build_profile"].as_str().unwrap().to_string()
    };
    let baseline_profile = manifest_profile();
    let object = |stem: &str| outdir.join("obj_dir").join(format!("{stem}.o"));
    let stable_objects = ["tb__runtime", "tb__test_T1Add", "tb__test_T3Add"];
    let stable_mtimes = stable_objects
        .iter()
        .map(|stem| fs::metadata(object(stem)).unwrap().modified().unwrap())
        .collect::<Vec<_>>();
    let edited_object = object("tb__test_T2Add");
    let edited_before = fs::metadata(&edited_object).unwrap().modified().unwrap();
    let registry_object = object("tb__registry");
    let registry_before = fs::metadata(&registry_object).unwrap().modified().unwrap();
    let edited = TB_THREE_TESTS.replacen(
        "dut.a = 10\n        dut.b = 20",
        "dut.a = 12\n        dut.b = 18",
        1,
    );
    fs::write(&tb, &edited).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let (ok, msg) = run_common_split_codegen("tbir", &tb, &sv, &outdir, &[]);
    assert!(ok, "edited TBIR common build failed:\n{msg}");
    assert_eq!(manifest_profile(), baseline_profile);
    for (stem, expected) in stable_objects.iter().zip(&stable_mtimes) {
        assert_eq!(
            fs::metadata(object(stem)).unwrap().modified().unwrap(),
            *expected,
            "test-body edit recompiled unrelated object {stem}"
        );
    }
    assert!(
        fs::metadata(&edited_object).unwrap().modified().unwrap() > edited_before,
        "test-body edit did not recompile its capsule"
    );
    assert_eq!(
        fs::metadata(&registry_object).unwrap().modified().unwrap(),
        registry_before,
        "test-body edit recompiled the unchanged registry"
    );

    let existing_sources = [
        "tb__runtime.cpp",
        "tb__test_T1Add.cpp",
        "tb__test_T2Add.cpp",
        "tb__test_T3Add.cpp",
    ];
    let before_add = existing_sources
        .iter()
        .map(|name| fs::read(outdir.join(name)).unwrap())
        .collect::<Vec<_>>();
    let registry_before_add = fs::read(outdir.join("tb__registry.cpp")).unwrap();
    let with_new = format!(
        "{edited}\ntest A0First\n    let dut : SplitAdder\n    run\n        dut.a = 7\n        dut.b = 8\n        wait 1 cycle\n        assert dut.sum == 15\n    end run\nend test A0First\n"
    );
    fs::write(&tb, &with_new).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let (ok, msg) = run_common_split_codegen("tbir", &tb, &sv, &outdir, &[]);
    assert!(ok, "added-test TBIR common build failed:\n{msg}");
    assert_eq!(manifest_profile(), baseline_profile);
    for (name, expected) in existing_sources.iter().zip(&before_add) {
        assert_eq!(
            fs::read(outdir.join(name)).unwrap(),
            *expected,
            "adding a test rewrote existing source {name}"
        );
    }
    assert!(object("tb__test_A0First").is_file());
    assert_ne!(
        fs::read(outdir.join("tb__registry.cpp")).unwrap(),
        registry_before_add,
        "adding a test did not rewrite the registry"
    );

    let before_delete = existing_sources
        .iter()
        .map(|name| fs::read(outdir.join(name)).unwrap())
        .collect::<Vec<_>>();
    fs::write(object("tb__test_A0First"), "not an object file\n").unwrap();
    fs::write(&tb, &edited).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let (ok, msg) = run_common_split_codegen("tbir", &tb, &sv, &outdir, &[]);
    assert!(
        ok,
        "deleted-test TBIR common build linked a stale object:\n{msg}"
    );
    assert_eq!(manifest_profile(), baseline_profile);
    assert!(!outdir.join("tb__test_A0First.cpp").exists());
    assert_eq!(
        fs::read(object("tb__test_A0First")).unwrap(),
        b"not an object file\n",
        "the stale capsule object was rebuilt or consumed instead of ignored"
    );
    for (name, expected) in existing_sources.iter().zip(&before_delete) {
        assert_eq!(
            fs::read(outdir.join(name)).unwrap(),
            *expected,
            "deleting a test rewrote surviving source {name}"
        );
    }
    let run = Command::new(outdir.join("obj_dir/VSplitAdder"))
        .args(["--test", "T2Add"])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "suite failed after deleting a stale capsule: {}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn artifact_promotion_failure_stops_before_manifest_or_build_and_recovers() {
    let dir = fresh_outdir("artifact_write_failure");
    let tb = write_suite(&dir, TB_THREE_TESTS);
    let outdir = dir.join("out");
    fs::create_dir_all(&outdir).unwrap();
    fs::create_dir(outdir.join("tb__runtime.cpp")).unwrap();

    let (ok, msg) = run_common_split(&tb, &dir.join("dut.sv"), &outdir, &[]);
    assert!(
        !ok,
        "second artifact promotion unexpectedly succeeded: {msg}"
    );
    assert!(
        outdir.join("tb__suite_api.hpp").is_file(),
        "the first planned artifact should have been promoted before the injected failure"
    );
    assert!(outdir.join("tb__runtime.cpp").is_dir());
    assert!(
        !outdir.join("tb__artifacts.json").exists(),
        "an incomplete publication must not publish its manifest"
    );
    assert!(outdir.join(".tb__artifacts.json.pending").is_file());
    assert!(
        !outdir.join("obj_dir").exists(),
        "native build must not start after an artifact promotion failure"
    );

    fs::remove_dir(outdir.join("tb__runtime.cpp")).unwrap();
    let (ok, msg) = run_common_split(&tb, &dir.join("dut.sv"), &outdir, &["--emit-only"]);
    assert!(
        ok,
        "publication did not recover after removing obstacle: {msg}"
    );
    assert!(outdir.join("tb__runtime.cpp").is_file());
    assert!(outdir.join("tb__artifacts.json").is_file());
    assert!(!outdir.join(".tb__artifacts.json.pending").exists());
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn manifest_replace_failure_stops_before_build_and_recovers() {
    let dir = fresh_outdir("manifest_replace_failure");
    let tb = write_suite(&dir, TB_THREE_TESTS);
    let outdir = dir.join("out");
    fs::create_dir_all(&outdir).unwrap();
    fs::create_dir(outdir.join("tb__artifacts.json")).unwrap();

    let (ok, msg) = run_common_split(&tb, &dir.join("dut.sv"), &outdir, &[]);
    assert!(!ok, "manifest replacement unexpectedly succeeded: {msg}");
    for artifact in [
        "tb__suite_api.hpp",
        "tb__runtime.cpp",
        "tb__test_T1Add.cpp",
        "tb__test_T2Add.cpp",
        "tb__test_T3Add.cpp",
        "tb__registry.cpp",
    ] {
        assert!(
            !outdir.join(artifact).exists(),
            "artifact {artifact} escaped staging before manifest-path validation"
        );
    }
    assert!(
        !outdir.join("obj_dir").exists(),
        "native build must not start after manifest publication fails"
    );
    assert!(outdir.join(".tb__artifacts.json.pending").is_file());

    fs::remove_dir(outdir.join("tb__artifacts.json")).unwrap();
    let (ok, msg) = run_common_split(&tb, &dir.join("dut.sv"), &outdir, &["--emit-only"]);
    assert!(ok, "manifest publication did not recover: {msg}");
    assert!(outdir.join("tb__artifacts.json").is_file());
    assert!(!outdir.join(".tb__artifacts.json.pending").exists());
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn stale_cleanup_failure_invalidates_manifest_stops_build_and_recovers() {
    let dir = fresh_outdir("cleanup_failure");
    let tb = write_suite(&dir, TB_THREE_TESTS);
    let outdir = dir.join("out");
    let (ok, msg) = run_common_split(&tb, &dir.join("dut.sv"), &outdir, &["--emit-only"]);
    assert!(ok, "initial publication failed: {msg}");

    let manifest_path = outdir.join("tb__artifacts.json");
    let old_manifest = fs::read(&manifest_path).unwrap();
    let stale = outdir.join("tb__test_T3Add.cpp");
    fs::remove_file(&stale).unwrap();
    fs::create_dir(&stale).unwrap();
    let without_t3 = TB_THREE_TESTS.replace(
        "test T3Add\n    let dut : SplitAdder\n    run\n        dut.a = 100\n        dut.b = 50\n        wait 1 cycle\n        assert dut.sum == 150\n    end run\nend test T3Add\n",
        "",
    );
    fs::write(&tb, without_t3).unwrap();

    let (ok, msg) = run_common_split(&tb, &dir.join("dut.sv"), &outdir, &[]);
    assert!(!ok, "stale cleanup unexpectedly succeeded: {msg}");
    assert!(
        !manifest_path.exists(),
        "cleanup failure must not leave a trusted canonical manifest over a partial update"
    );
    assert_eq!(
        fs::read(outdir.join(".tb__artifacts.json.previous")).unwrap(),
        old_manifest,
        "cleanup recovery must retain the strictly validated prior ownership manifest"
    );
    assert!(outdir.join(".tb__artifacts.json.pending").is_file());
    assert!(
        !outdir.join("obj_dir").exists(),
        "native build must not start after stale cleanup fails"
    );

    fs::remove_dir(&stale).unwrap();
    let (ok, msg) = run_common_split(&tb, &dir.join("dut.sv"), &outdir, &["--emit-only"]);
    assert!(ok, "cleanup did not recover after removing obstacle: {msg}");
    assert!(!stale.exists());
    assert!(!outdir.join(".tb__artifacts.json.previous").exists());
    assert!(!outdir.join(".tb__artifacts.json.pending").exists());
    let manifest = fs::read_to_string(&manifest_path).unwrap();
    assert!(!manifest.contains("T3Add"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn malformed_and_unknown_manifests_never_authorize_deletion() {
    let dir = fresh_outdir("untrusted_manifest");
    let tb = write_suite(&dir, TB_THREE_TESTS);

    let unknown_out = dir.join("unknown");
    fs::create_dir_all(&unknown_out).unwrap();
    let unknown_sentinel = unknown_out.join("tb__test_Old.cpp");
    fs::write(&unknown_sentinel, "do not delete").unwrap();
    fs::write(
        unknown_out.join("tb__artifacts.json"),
        r#"{"schema_version":2,"interface_abi":"0123456789abcdef","build_profile":"fedcba9876543210","tests":["Old"],"artifacts":["tb__test_Old.cpp"]}"#,
    )
    .unwrap();
    let (ok, msg) = run_common_split(&tb, &dir.join("dut.sv"), &unknown_out, &["--emit-only"]);
    assert!(ok, "unknown-schema recovery failed: {msg}");
    assert_eq!(
        fs::read_to_string(&unknown_sentinel).unwrap(),
        "do not delete"
    );

    let malformed_out = dir.join("malformed");
    fs::create_dir_all(&malformed_out).unwrap();
    let traversal_victim = dir.join("victim.cpp");
    fs::write(&traversal_victim, "outside output directory").unwrap();
    fs::write(
        malformed_out.join("tb__artifacts.json"),
        r#"{"schema_version":1,"interface_abi":"0123456789abcdef","build_profile":"fedcba9876543210","tests":["Old"],"artifacts":["../victim.cpp"]}"#,
    )
    .unwrap();
    let (ok, msg) = run_common_split(&tb, &dir.join("dut.sv"), &malformed_out, &["--emit-only"]);
    assert!(ok, "malformed-manifest recovery failed: {msg}");
    assert_eq!(
        fs::read_to_string(&traversal_victim).unwrap(),
        "outside output directory"
    );

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
        // `--emit-only` deliberately: the property under test is that seed
        // provenance never reaches the generated BYTES, which is settled at
        // emission. Letting this one invocation run the native build made the
        // test silently require Verilator — every other build-dependent test
        // here is gated on `verilator_present()`, and this one was not, so it
        // failed outright on a runner without Verilator.
        .arg("--emit-only")
        .env("HARC_SEED", "424242");
    let out = cmd.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    // 6 suite artifacts (manifest write is silent when unchanged).
    assert_eq!(
        text.matches("(unchanged)").count(),
        6,
        "expected all artifacts reused: {text}"
    );
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
        let (ok, msg) = run_common_split(&tb, &dir.join("dut.sv"), &outdir, &["--test", t]);
        assert!(ok, "{t} failed: {msg}");
        assert!(msg.contains("ALL TESTS PASSED"), "{t}: {msg}");
    }

    // Unknown selection fails clearly without falling through to a
    // different test.
    let bin = outdir.join("obj_dir").join("VSplitAdder");
    let unknown = Command::new(&bin)
        .arg("--test")
        .arg("NoSuch")
        .output()
        .unwrap();
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
    let edited = TB_THREE_TESTS.replacen(
        "dut.a = 10\n        dut.b = 20",
        "dut.a = 12\n        dut.b = 18",
        1,
    );
    assert_ne!(edited, TB_THREE_TESTS);
    fs::write(&tb_path, &edited).unwrap();
    let (ok, msg) = run_common_split(&tb_path, &sv, &outdir, &[]);
    assert!(ok, "{msg}");
    for (n, before) in &baseline {
        let after = read(n);
        if n == "tb__test_T2Add.cpp" {
            assert_ne!(before, &after, "{n} should have been rewritten");
        } else {
            assert_eq!(
                before, &after,
                "{n} must stay byte-identical after a T2 edit"
            );
        }
    }
    // And the edited suite still passes end to end.
    let bin = outdir.join("obj_dir").join("VSplitAdder");
    let run = Command::new(&bin)
        .arg("--test")
        .arg("T2Add")
        .output()
        .unwrap();
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
    let run = Command::new(&bin)
        .arg("--test")
        .arg("A0First")
        .output()
        .unwrap();
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
    assert!(
        !outdir.join("tb__test_T3Add.cpp").exists(),
        "stale capsule must be cleaned up"
    );
    let manifest = read("tb__artifacts.json");
    assert!(!manifest.contains("T3Add"));
    assert!(manifest.contains("T1Add"));
    let run = Command::new(&bin)
        .arg("--test")
        .arg("T1Add")
        .output()
        .unwrap();
    assert!(run.status.success(), "remaining suite still runs");
    let unknown = Command::new(&bin)
        .arg("--test")
        .arg("T3Add")
        .output()
        .unwrap();
    // The relinked binary no longer contains T3Add's descriptor or
    // object, so selection fails clearly instead of falling through.
    let text = String::from_utf8_lossy(&unknown.stderr);
    assert!(
        text.contains("unknown test: T3Add"),
        "expected clear unknown-test failure, got: {text}"
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn common_layout_distinguishes_shared_body_and_shared_interface_edits() {
    let dir = fresh_outdir("shared_incrementality");
    let sv = dir.join("dut.sv");
    fs::write(&sv, ADDER_SV).unwrap();
    let tb = dir.join("tb.harc");
    let baseline_source = r#"function shared_value() -> uint<8>
    return 1
end function shared_value

test SharedA
    let dut : SplitAdder
    run
        dut.a = 1
        dut.b = 2
        wait 1 cycle
        assert dut.sum == 3
    end run
end test SharedA

test SharedB
    let dut : SplitAdder
    run
        dut.a = 1
        dut.b = 3
        wait 1 cycle
        assert dut.sum == 4
    end run
end test SharedB
"#;
    for codegen in ["v1", "tbir"] {
        fs::write(&tb, baseline_source).unwrap();
        let outdir = dir.join(format!("out_{codegen}"));
        let (ok, msg) = run_common_split_codegen(codegen, &tb, &sv, &outdir, &["--emit-only"]);
        assert!(ok, "{codegen}: {msg}");
        let read = |name: &str| fs::read(outdir.join(name)).unwrap();
        let baseline_header = read("tb__suite_api.hpp");
        let baseline_runtime = read("tb__runtime.cpp");
        let baseline_a = read("tb__test_SharedA.cpp");
        let baseline_b = read("tb__test_SharedB.cpp");
        let baseline_registry = read("tb__registry.cpp");
        let baseline_manifest = read("tb__artifacts.json");

        let body_edit = baseline_source.replace("return 1", "return 2");
        fs::write(&tb, body_edit).unwrap();
        let (ok, msg) = run_common_split_codegen(codegen, &tb, &sv, &outdir, &["--emit-only"]);
        assert!(ok, "{codegen}: {msg}");
        assert_eq!(read("tb__suite_api.hpp"), baseline_header, "{codegen}");
        assert_ne!(read("tb__runtime.cpp"), baseline_runtime, "{codegen}");
        assert_eq!(read("tb__test_SharedA.cpp"), baseline_a, "{codegen}");
        assert_eq!(read("tb__test_SharedB.cpp"), baseline_b, "{codegen}");
        assert_eq!(read("tb__registry.cpp"), baseline_registry, "{codegen}");
        assert_eq!(read("tb__artifacts.json"), baseline_manifest, "{codegen}");

        let interface_edit = baseline_source.replace("-> uint<8>", "-> uint<128>");
        fs::write(&tb, interface_edit).unwrap();
        let (ok, msg) = run_common_split_codegen(codegen, &tb, &sv, &outdir, &["--emit-only"]);
        assert!(ok, "{codegen}: {msg}");
        assert_ne!(read("tb__suite_api.hpp"), baseline_header, "{codegen}");
        assert_ne!(read("tb__runtime.cpp"), baseline_runtime, "{codegen}");
        assert_ne!(read("tb__test_SharedA.cpp"), baseline_a, "{codegen}");
        assert_ne!(read("tb__test_SharedB.cpp"), baseline_b, "{codegen}");
        assert_ne!(read("tb__registry.cpp"), baseline_registry, "{codegen}");
        assert_ne!(read("tb__artifacts.json"), baseline_manifest, "{codegen}");
    }
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn common_layout_runtime_selectors_do_not_change_artifact_identity() {
    let dir = fresh_outdir("runtime_only_identity");
    let tb = write_suite(&dir, TB_THREE_TESTS);
    let sv = dir.join("dut.sv");
    for codegen in ["v1", "tbir"] {
        let outdir = dir.join(format!("out_{codegen}"));
        let trace_a = dir.join(format!("{codegen}_a.jsonl"));
        let trace_b = dir.join(format!("{codegen}_b.jsonl"));
        let wave_a = dir.join(format!("{codegen}_a.vcd"));
        let wave_b = dir.join(format!("{codegen}_b.vcd"));
        let (ok, msg) = run_common_split_codegen(
            codegen,
            &tb,
            &sv,
            &outdir,
            &[
                "--emit-only",
                "--test",
                "T1Add",
                "--seed",
                "1",
                "--record-trace",
                trace_a.to_str().unwrap(),
                "--waves",
                "--wave-format",
                "vcd",
                "--wave-file",
                wave_a.to_str().unwrap(),
            ],
        );
        assert!(ok, "{codegen}: {msg}");
        let names = [
            "tb__suite_api.hpp",
            "tb__runtime.cpp",
            "tb__test_T1Add.cpp",
            "tb__test_T2Add.cpp",
            "tb__test_T3Add.cpp",
            "tb__registry.cpp",
            "tb__artifacts.json",
        ];
        let before = names
            .iter()
            .map(|name| (name, fs::read(outdir.join(name)).unwrap()))
            .collect::<Vec<_>>();

        let (ok, msg) = run_common_split_codegen(
            codegen,
            &tb,
            &sv,
            &outdir,
            &[
                "--emit-only",
                "--test",
                "T2Add",
                "--seed",
                "999",
                "--record-trace",
                trace_b.to_str().unwrap(),
                "--waves",
                "--wave-format",
                "vcd",
                "--wave-file",
                wave_b.to_str().unwrap(),
            ],
        );
        assert!(ok, "{codegen}: {msg}");
        for (name, expected) in before {
            assert_eq!(
                fs::read(outdir.join(name)).unwrap(),
                expected,
                "{codegen}: {name}"
            );
        }
    }
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn common_layout_external_native_input_changes_only_the_build_profile_identity() {
    let dir = fresh_outdir("external_profile");
    let tb = write_suite(&dir, TB_THREE_TESTS);
    let sv = dir.join("dut.sv");
    for codegen in ["v1", "tbir"] {
        let outdir = dir.join(format!("out_{codegen}"));
        let (ok, msg) = run_common_split_codegen(
            codegen,
            &tb,
            &sv,
            &outdir,
            &["--emit-only", "--build-profile-input", "assertions=off"],
        );
        assert!(ok, "{codegen}: {msg}");
        let first: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(outdir.join("tb__artifacts.json")).unwrap())
                .unwrap();

        let (ok, msg) = run_common_split_codegen(
            codegen,
            &tb,
            &sv,
            &outdir,
            &["--emit-only", "--build-profile-input", "assertions=on"],
        );
        assert!(ok, "{codegen}: {msg}");
        let second: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(outdir.join("tb__artifacts.json")).unwrap())
                .unwrap();
        assert_eq!(first["interface_abi"], second["interface_abi"], "{codegen}");
        assert_ne!(first["build_profile"], second["build_profile"], "{codegen}");
    }
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
    let drv : Drv active
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
    assert_eq!(
        runtime
            .matches("uint64_t HarcSuiteGlue::harc_user_callable_inner_")
            .count(),
        1
    );
    assert_eq!(
        runtime
            .matches("uint64_t HarcSuiteGlue::harc_user_callable_outer_")
            .count(),
        1
    );
    assert_eq!(runtime.matches("void HarcSuiteGlue::Drv_send(").count(), 1);
    assert_eq!(
        runtime.matches("void HarcSuiteGlue::Drv_watchdog(").count(),
        1
    );
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

    // A record-typed hook parameter must render with the SAME type in the
    // header as in the definition. Rendering it through the free
    // `c_type_for` yields `VRegOp*` — a Verilator DUT handle for a type
    // that is not a DUT — and the suite stops compiling. Asserting on the
    // text alone is not enough (that is exactly what this test used to
    // do), so the build below is the real gate; this keeps the failure
    // legible when it regresses.
    assert!(
        !header.contains("VRegOp"),
        "record hook param must not render as a Verilator handle:\n{header}"
    );
    assert!(
        header.contains("void Drv_send(Drv& self, RegOp t);"),
        "{header}"
    );

    // The emit-only assertions above cannot catch a suite that emits
    // cleanly and then fails to compile, which is the failure mode this
    // whole layout is prone to. Build and run it.
    if !verilator_present() {
        eprintln!("skipping build half: verilator not on PATH");
        fs::remove_dir_all(&dir).ok();
        return;
    }
    let built = dir.join("built");
    let (ok, msg) = run_common_split(&tb, &sv, &built, &[]);
    assert!(ok, "compose suite failed to build/run: {msg}");
    assert!(msg.contains("ALL TESTS PASSED"), "{msg}");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn common_layout_heartbeat_only_transactor_state_compiles_and_runs() {
    if !verilator_present() {
        eprintln!("skipping: verilator not on PATH");
        return;
    }
    let dir = fresh_outdir("heartbeat_only_state");
    let sv = dir.join("dut.sv");
    fs::write(
        &sv,
        "module IdleDut(input logic clk, input logic [7:0] d, output logic [7:0] q);\n\
         always_ff @(posedge clk) q <= d;\n\
         endmodule\n",
    )
    .unwrap();
    let tb = dir.join("tb.harc");
    fs::write(
        &tb,
        r#"transaction _PlainDriver_state
    value : uint<8>
end transaction _PlainDriver_state

transaction HarcTransactorState_PlainDriver
    value : uint<8>
end transaction HarcTransactorState_PlainDriver

transaction _A_state
    value : uint<8>
end transaction _A_state

transaction _A_1_state
    value : uint<8>
end transaction _A_1_state

transaction HarcTransactorState_A
    value : uint<8>
end transaction HarcTransactorState_A

transactor A
    dut : IdleDut
    when active
        function ping()
            dut.d = 1
        end ping
    end when
end transactor A

transactor A_1
    dut : IdleDut
    when active
        function ping()
            dut.d = 2
        end ping
    end when
end transactor A_1

agent _Foo
    hookable bar_state()
        log(info, "callable/type collision")
    end bar_state
end agent _Foo

transactor Foo_bar
    dut : IdleDut
    when active
        function ping()
            dut.d = 4
        end ping
    end when
end transactor Foo_bar

transactor PlainDriver
    dut : IdleDut
    when active
        function drive(value: uint<8>)
            dut.d = value
        end drive
    end when
end transactor PlainDriver

testbench IdleTb
    dut : IdleDut
    helper : _Foo
    driver : PlainDriver active
    collision_driver : Foo_bar active
    function driver_idle(cycles: uint<8>) -> bool
        return driver.idle(cycles)
    end function driver_idle
    function collision_driver_idle(cycles: uint<8>) -> bool
        return collision_driver.idle(cycles)
    end function collision_driver_idle
end testbench IdleTb

impl HeartbeatOnlyState for IdleTb
    clock clk = 10ns
    run
        let rec : _PlainDriver_state
        rec.value = 3
        assert rec.value == 3 else fail("colliding record type was shadowed")
        helper.bar_state()
        collision_driver.ping()
        driver.drive(7)
        wait 2 cycles
        assert driver_idle(1) else fail("heartbeat-only state was not idle")
        assert collision_driver_idle(1) else fail("callable/type collision state was not idle")
    end run
end impl HeartbeatOnlyState

impl HeartbeatOnlyStateSecond for IdleTb
    clock clk = 10ns
    run
        driver.drive(9)
        wait 2 cycles
        assert driver_idle(1) else fail("second heartbeat-only state was not idle")
    end run
end impl HeartbeatOnlyStateSecond"#,
    )
    .unwrap();

    let outdir = dir.join("out");
    let (ok, msg) = run_common_split_codegen("tbir", &tb, &sv, &outdir, &["--top", "IdleDut"]);
    assert!(ok, "heartbeat-only common suite failed: {msg}");
    assert!(msg.contains("ALL TESTS PASSED"), "{msg}");
    let interface = fs::read_to_string(outdir.join("tb__suite_api.hpp")).unwrap();
    assert!(
        interface.contains("struct HarcTransactorState_PlainDriver_1"),
        "{interface}"
    );
    assert!(
        interface.contains("HarcTransactorState_PlainDriver_1& _harc_tb_transactor_state_driver"),
        "{interface}"
    );
    assert!(interface.contains("struct HarcTransactorState_A_1 {"));
    assert!(interface.contains("struct HarcTransactorState_A_1_1 {"));

    let self_outdir = dir.join("self");
    let output = Command::new(harc_bin())
        .arg("sim")
        .arg(&tb)
        .arg("--sv")
        .arg(&sv)
        .arg("--top")
        .arg("IdleDut")
        .arg("--codegen")
        .arg("tbir")
        .arg("--cpp-split")
        .arg("tests")
        .arg("--cpp-split-layout")
        .arg("self-contained")
        .arg("--cpp-split-group-size")
        .arg("1")
        .arg("--outdir")
        .arg(&self_outdir)
        .output()
        .expect("spawn self-contained heartbeat suite");
    let self_msg = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "heartbeat-only self-contained suite failed: {self_msg}"
    );
    assert!(self_msg.contains("ALL TESTS PASSED"), "{self_msg}");

    fs::remove_dir_all(&dir).ok();
}

/// Bus-bound transactor hookables cannot become glue members: `bus.<ch>`
/// resolves at codegen time against a PER-TEST bind, and the common TU is
/// per-suite. They stay capsule-local (one copy per test) and must still
/// build and run. Regression for the `use of undeclared identifier 'bus'`
/// class.
#[test]
fn common_layout_bus_bound_hookables_stay_capsule_local() {
    if !verilator_present() {
        eprintln!("skipping: verilator not on PATH");
        return;
    }
    let dir = fresh_outdir("busbound");
    let sv = dir.join("dut.sv");
    fs::write(
        &sv,
        "module BusDut(input logic clk, input logic p_cmd_valid, output logic p_cmd_ready,\n\
         input logic [7:0] p_cmd_addr, output logic [7:0] q);\n\
         assign p_cmd_ready = 1'b1;\n\
         always_ff @(posedge clk) if (p_cmd_valid) q <= p_cmd_addr + 8'd1;\n\
         endmodule\n",
    )
    .unwrap();
    let tb = dir.join("tb.harc");
    fs::write(
        &tb,
        r#"bus PokeBus
    handshake_channel cmd: send kind: valid_ready
        addr: uint<8>
    end handshake_channel cmd
end bus PokeBus

transactor Poker bound to PokeBus
    when active
        hookable poke(a: uint<8>)
            bus.cmd.send(a)
        end poke
    end when
end transactor Poker

test TBusBound
    let dut : BusDut
    let p : PokeBus = bind dut
    let drv : Poker active = bind p
    run
        drv.poke(7)
        wait 2 cycles
        assert dut.q == 8 else fail("bus-bound hookable drove nothing")
    end run
end test TBusBound
"#,
    )
    .unwrap();
    let outdir = dir.join("out");
    let (ok, msg) = run_common_split(&tb, &sv, &outdir, &[]);
    assert!(ok, "bus-bound suite failed: {msg}");
    assert!(msg.contains("ALL TESTS PASSED"), "{msg}");

    // The body must live in the capsule, resolved to concrete DUT ports —
    // NOT in the shared runtime, where `bus` has no meaning.
    let runtime = fs::read_to_string(outdir.join("tb__runtime.cpp")).unwrap();
    // (the runtime legitimately mentions `Poker_poke_pre`/`_post` — those
    // registries stay on the per-run runtime. What must NOT exist is a
    // shared DEFINITION of the callable itself.)
    assert!(
        !runtime.contains("HarcSuiteGlue::Poker_poke("),
        "bus-bound hookable must not be promoted to the common TU:\n{runtime}"
    );
    let capsule = fs::read_to_string(outdir.join("tb__test_TBusBound.cpp")).unwrap();
    assert!(capsule.contains("auto Poker_poke = [&]"), "{capsule}");
    // ...and the capsule must call it WITHOUT the glue prefix.
    assert!(!capsule.contains("_glue.Poker_poke"), "{capsule}");
    // The bus path must have been resolved to a real DUT port.
    assert!(capsule.contains("p_cmd_addr"), "{capsule}");
    // Its hook registries still live on the runtime, so per-run ownership
    // is unaffected by the capsule-local carve-out.
    let header = fs::read_to_string(outdir.join("tb__suite_api.hpp")).unwrap();
    assert!(header.contains("Poker_poke_pre"), "{header}");

    fs::remove_dir_all(&dir).ok();
}

/// Test-local `randomize` state belongs to the capsule, while immutable
/// problem descriptors belong to the common runtime TU. Neither is part of
/// the shared interface ABI.
#[test]
fn common_layout_randomize_cells_are_fully_migrated() {
    if !verilator_present() {
        eprintln!("skipping: verilator not on PATH");
        return;
    }
    let dir = fresh_outdir("cells");
    let sv = dir.join("dut.sv");
    fs::write(&sv, ADDER_SV).unwrap();
    let tb = dir.join("tb.harc");
    fs::write(
        &tb,
        r#"transaction Stim
    token : uint<2> with [unique within test]
end transaction Stim

test TCells
    let dut : SplitAdder
    run
        let s : Stim
        for i in 1 .. 6
            randomize(s)
            dut.a = s.token
            dut.b = 1
            wait 1 cycle
        end for
    end run
end test TCells
"#,
    )
    .unwrap();
    let outdir = dir.join("out");
    let (ok, msg) = run_common_split(&tb, &sv, &outdir, &[]);
    assert!(ok, "randomize suite failed: {msg}");
    assert!(msg.contains("ALL TESTS PASSED"), "{msg}");

    let capsule = fs::read_to_string(outdir.join("tb__test_TCells.cpp")).unwrap();
    let interface = fs::read_to_string(outdir.join("tb__suite_api.hpp")).unwrap();
    let runtime = fs::read_to_string(outdir.join("tb__runtime.cpp")).unwrap();
    for generated in [&interface, &runtime, &capsule] {
        assert!(!generated.contains("thread_local"), "{generated}");
        assert!(!generated.contains("harc_rt_current_run"), "{generated}");
        assert!(
            !generated.contains("static harc_rt::random::HarcRng"),
            "{generated}"
        );
        assert!(
            !generated.contains("static harc_rt::random::HarcRuntimeCallSite"),
            "{generated}"
        );
    }
    assert!(
        capsule.contains("_harc_randomize_state"),
        "test-local randomize storage must be owned by the capsule:\n{capsule}"
    );
    assert!(
        !interface.contains("_solver_site_")
            && !interface.contains("_harc_runtime_random_problem_table"),
        "test-local cells and immutable descriptors must stay out of the interface:\n{interface}"
    );
    assert!(
        runtime.contains("static constexpr harc_rt::random::HarcRuntimeProblemDescriptor _harc_runtime_random_problem_table_entries[]"),
        "immutable descriptors must be defined privately in the runtime TU:\n{runtime}"
    );
    // No reader may name a migrated cell without the capsule-local path.
    for needle in [
        "harc_unique_clear(_",
        "harc_unique_remember(_",
        "harc_auto_cov_apply_point_preference(_auto_cov_plan",
    ] {
        if let Some(pos) = capsule.find(needle) {
            let line_end = capsule[pos..]
                .find('\n')
                .map(|e| pos + e)
                .unwrap_or(capsule.len());
            let line = &capsule[pos..line_end];
            assert!(
                line.contains("_harc_randomize_state."),
                "migrated cell referenced without the per-run path: {line}"
            );
        }
    }
    // No function-local statics survive in a capsule.
    assert!(
        !capsule.contains("static harc_rt::random::HarcUniqueHistory"),
        "unique history must live on the per-run runtime:\n{capsule}"
    );
    assert!(
        !capsule.contains("static harc_rt::random::HarcAutoCovState"),
        "auto-cov state must live on the per-run runtime:\n{capsule}"
    );

    fs::remove_dir_all(&dir).ok();
}

/// `--waves` under the common layout must produce a real trace. The dump
/// helpers are glue members reading `ctx.tfp`, so a run prologue that
/// COPIES the tracer pointer instead of aliasing it leaves that member
/// null and every dump silently no-ops — exit 0, header-only VCD.
#[test]
fn common_layout_waves_actually_dump() {
    if !verilator_present() {
        eprintln!("skipping: verilator not on PATH");
        return;
    }
    let dir = fresh_outdir("waves");
    let sv = dir.join("dut.sv");
    fs::write(
        &sv,
        "module WaveDut(input logic clk, input logic [7:0] d, output logic [7:0] q);\n         always_ff @(posedge clk) q <= d;\nendmodule\n",
    )
    .unwrap();
    let tb = dir.join("tb.harc");
    fs::write(
        &tb,
        "test TWave\n    let dut : WaveDut\n    run\n        dut.d = 5\n        wait 3 cycles\n        assert dut.q == 5\n    end run\nend test TWave\n",
    )
    .unwrap();
    let outdir = dir.join("out");
    let (ok, msg) = run_common_split(&tb, &sv, &outdir, &["--waves", "--wave-format", "vcd"]);
    assert!(ok, "waves run failed: {msg}");

    let vcd = fs::read_to_string(outdir.join("waves.vcd")).expect("waves.vcd");
    let timestamps = vcd.lines().filter(|l| l.starts_with('#')).count();
    assert!(
        timestamps > 0,
        "common-layout --waves produced a header-only VCD ({} bytes, 0 timestamps)",
        vcd.len()
    );

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
        assert_eq!(
            t,
            mtime(&p),
            "{} was recompiled on an unchanged rerun",
            p.display()
        );
    }

    fs::remove_dir_all(&dir).ok();
}

/// Emitter-level diagnostics must reach the user under the common
/// layout. `Emitter::errors` is a side-channel — emission continues past
/// a diagnostic so one bad statement does not mask the rest, and the
/// caller must drain the vector when it finishes. The legacy single-TU
/// path does that at the end of `emit_with_opts`; `emit_common_split`
/// drives three separate emitters (interface header, common impl,
/// one capsule per test) and checked none of them.
///
/// The result was worse than a bad error message: the offending
/// statement was dropped from the generated capsule and emission
/// reported SUCCESS. A `let drv : Drv = drv()` produced a capsule with
/// no `drv` at all, so the failure only appeared later — or not at all,
/// if nothing downstream referenced the name.
///
/// Both halves are asserted deliberately. A silent swallow has no
/// failing test by construction, so the check has to be that emission
/// *fails*, not merely that some text appears somewhere.
#[test]
fn common_layout_surfaces_emitter_diagnostics_instead_of_dropping_statements() {
    let dir = fresh_outdir("diag");
    let sv = dir.join("dut.sv");
    fs::write(&sv, ADDER_SV).unwrap();
    let tb_path = dir.join("tb.harc");
    fs::write(
        &tb_path,
        "\
transactor Helper
    n : uint<8>
end transactor Helper

test BadInstantiation
    let dut : SplitAdder
    let h : Helper = mk()
    run
        wait 1 cycle
    end run
end test BadInstantiation
",
    )
    .unwrap();

    let (ok, out) = run_common_split(&tb_path, &sv, &dir, &["--emit-only"]);
    assert!(
        !ok,
        "common layout must FAIL on an emitter diagnostic, not emit a capsule \
         with the offending statement quietly dropped; got:\n{out}"
    );
    // miette hard-wraps the rendered diagnostic into a box, so a phrase
    // that fits on one source line can arrive split across two with a
    // `│` gutter in between. Match against the flattened text or the
    // assertion becomes a test of the terminal width.
    let flat = out
        .replace('│', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        flat.contains("transactor `Helper` has no value form"),
        "the diagnostic itself must reach the user; got:\n{out}"
    );
    assert!(
        flat.contains("test `BadInstantiation`"),
        "the diagnostic must name which test's capsule produced it — the common \
         layout drives one emitter per test and the source line alone does not \
         say which; got:\n{out}"
    );

    // The capsule must not exist in a state that silently omits the let.
    let capsule = dir.join("tb__test_BadInstantiation.cpp");
    if capsule.exists() {
        let body = fs::read_to_string(&capsule).unwrap();
        panic!("a capsule was written despite the diagnostic:\n{body}");
    }
}

// --------------------------------------------------------------------------
// #619 M4 hardening (Phase A1): SHARED reusable-testbench lifecycle under the
// TBIR common (separate interface/common/shard) layout, REAL build + link +
// run. This is the once-per-suite payoff #619 targets: the shared body is
// DEFINED once in the common TU (external linkage) with a prototype in the
// suite header, and every shard CALLS it. The in-process ownership test
// `m4b_split_common_layout_emits_lifecycle_once_in_common` pins that shape by
// substring; here the real Verilator build proves the common def + header
// prototype + shard calls actually link and run.
//
// NOTE: `run_common_split` above hardcodes `--codegen v1`, whose common
// layout is the issue-#643 per-test-capsule emission (`tb__runtime.cpp` +
// capsules) — a DIFFERENT path that has no out-of-line `_harc_lc` lifecycle
// symbol. The out-of-line lifecycle lives on the TBIR separate/common path
// (`<stem>__common.cpp` / `<stem>__suite.hpp` / `<stem>__shardN.cpp`), so
// this suite runs `--codegen tbir` explicitly.
// --------------------------------------------------------------------------

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

/// Reusable testbench with a shared suspending (`Coro`) `setup` + a
/// non-suspending (`Plain`) `check`, bound by two impls.
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

/// `harc sim … --codegen tbir --cpp-split tests --cpp-split-layout common`.
fn run_common_split_tbir(
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
        .arg("--top")
        .arg("LcCounter")
        .arg("--codegen")
        .arg("tbir")
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

/// Files in `outdir` whose name ends with `suffix` (e.g. `__common.cpp`),
/// as (name, contents) pairs.
fn read_by_suffix(outdir: &Path, suffix: &str) -> Vec<(String, String)> {
    fs::read_dir(outdir)
        .expect("read outdir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().unwrap().to_string_lossy().ends_with(suffix))
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                fs::read_to_string(&p).expect("read generated file"),
            )
        })
        .collect()
}

fn count(hay: &str, needle: &str) -> usize {
    hay.matches(needle).count()
}

#[test]
fn common_layout_shared_lifecycle_defines_body_once_builds_and_runs() {
    if !verilator_present() {
        eprintln!("skipping: verilator not on PATH");
        return;
    }
    let dir = fresh_outdir("lc_common");
    let sv = dir.join("dut.sv");
    fs::write(&sv, LC_COUNTER_SV).unwrap();
    let tb = dir.join("lc.harc");
    fs::write(&tb, LC_SHARED_TB).unwrap();
    let outdir = dir.join("out");

    // Real build + default dispatch (first impl in source order).
    let (ok, msg) = run_common_split_tbir(&tb, &sv, &outdir, &[]);
    assert!(ok, "lifecycle common build failed: {msg}");
    assert!(msg.contains("ALL TESTS PASSED"), "{msg}");

    // Each impl dispatches from the same linked binary.
    for t in ["LcBumpThree", "LcBumpFive"] {
        let (ok, msg) = run_common_split_tbir(&tb, &sv, &outdir, &["--test", t]);
        assert!(ok, "{t} failed: {msg}");
        assert!(msg.contains("ALL TESTS PASSED"), "{t}: {msg}");
    }

    // The DEFINITION (signature + `(HarcTestContext…`) lives exactly once in
    // the common TU. Calls read `(ctx, …)`, so `(HarcTestContext` matches
    // only the definition, never a call site.
    let setup_def = "harc_rt::HarcThread _harc_lc__tb_lifecycle_LcSharedTb_Setup(HarcTestContext";
    let check_def = "void _harc_lc__tb_lifecycle_LcSharedTb_Check(HarcTestContext";

    let common = read_by_suffix(&outdir, "__runtime.cpp");
    assert_eq!(
        common.len(),
        1,
        "expected exactly one common TU, got {}",
        common.len()
    );
    let (_, common_src) = &common[0];
    assert_eq!(
        count(common_src, setup_def),
        1,
        "the suspending setup coroutine must be DEFINED exactly once in the common TU"
    );
    assert_eq!(
        count(common_src, check_def),
        1,
        "the non-suspending check must be DEFINED exactly once in the common TU"
    );

    // Zero DEFINITIONS in any shard (they only CALL the shared bodies).
    let shards = read_by_suffix(&outdir, ".cpp")
        .into_iter()
        .filter(|(n, _)| n.contains("__test_"))
        .collect::<Vec<_>>();
    assert!(!shards.is_empty(), "expected at least one shard TU");
    for (n, src) in &shards {
        assert_eq!(
            count(src, setup_def),
            0,
            "shard {n} must NOT define the shared setup coroutine"
        );
        assert_eq!(
            count(src, check_def),
            0,
            "shard {n} must NOT define the shared check function"
        );
    }

    // The suite header carries a prototype for each shared body so shards
    // can call them.
    let header = read_by_suffix(&outdir, "__suite_api.hpp");
    assert_eq!(header.len(), 1, "expected exactly one suite header");
    let (_, header_src) = &header[0];
    assert!(
        header_src.contains("_harc_lc__tb_lifecycle_LcSharedTb_Setup(HarcTestContext"),
        "suite header must declare the setup coroutine prototype:\n{header_src}"
    );
    assert!(
        header_src.contains("_harc_lc__tb_lifecycle_LcSharedTb_Check(HarcTestContext"),
        "suite header must declare the check prototype:\n{header_src}"
    );

    fs::remove_dir_all(&dir).ok();
}
