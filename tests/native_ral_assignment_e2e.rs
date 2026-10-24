//! Native end-to-end regression for single-evaluation RAL frontdoor writes.

use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run_backend(name: &str, v1: bool) {
    let root = repo_root();
    let outdir = std::env::temp_dir().join(format!(
        "harc_r2_b11_ral_rhs_{name}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&outdir);
    std::fs::create_dir_all(&outdir).expect("create output directory");

    let fixture = root.join("tests/fixtures/r2_b11_ral_rhs_once.harc");
    let ref_src = root.join("tests/dut/r2_b11_ral_rhs_once_ref.cpp");
    let mut command = Command::new(env!("CARGO_BIN_EXE_harc"));
    command
        .args(["sim", "--sv"])
        .arg(root.join("tests/dut/AxiLiteRegs.sv"))
        .args(["--top", "AxiLiteRegs", "--ref-src"])
        .arg(ref_src)
        .args(["--jobs", "2", "--outdir"])
        .arg(&outdir)
        .arg("--codegen");
    if v1 {
        command.arg("v1");
    } else {
        command.arg("tbir");
    }
    let output = command
        .arg(fixture)
        .env("HARC_NO_LEARN", "1")
        .output()
        .expect("run native RAL fixture");
    assert!(
        output.status.success(),
        "{name} backend failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ALL TESTS PASSED"),
        "{name} backend did not report success:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn native_ral_rhs_is_evaluated_once_v1() {
    run_backend("v1", true);
}

#[test]
fn native_ral_rhs_is_evaluated_once_default_control() {
    run_backend("tbir", false);
}
