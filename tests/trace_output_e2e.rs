//! End-to-end regression for `harc sim --record-trace` output handling.
//!
//! A requested semantic trace must be created when its parent directories are
//! missing, and an unusable directory destination must fail clearly instead
//! of producing a green simulation with no trace artifact.

use std::path::{Path, PathBuf};
use std::process::Command;

const SMOKE: &str = r#"test Smoke
    let dut : Top
    run
        log(info, "hello")
    end run
end test Smoke
"#;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn harc_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_harc"))
}

fn verilator_present() -> bool {
    let present = Command::new("verilator")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    assert!(
        present || std::env::var_os("HARC_REQUIRE_VERILATOR").is_none(),
        "HARC_REQUIRE_VERILATOR is set but `verilator` is not on PATH"
    );
    present
}

fn fresh_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "harc_trace_output_e2e_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create trace e2e root");
    root
}

fn run_sim(outdir: &Path, trace: &Path, fixture: &Path) -> std::process::Output {
    Command::new(harc_bin())
        .arg("sim")
        .arg("--sv")
        .arg(workspace_root().join("tests/dut/top_counter.sv"))
        .arg(fixture)
        .arg("--top")
        .arg("Top")
        .arg("--outdir")
        .arg(outdir)
        .arg("--record-trace")
        .arg(trace)
        .output()
        .expect("spawn harc sim")
}

fn combined_output(output: &std::process::Output) -> String {
    format!(
        "--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn record_trace_prepares_nested_parent_and_rejects_directory() {
    if !verilator_present() {
        eprintln!("SKIP trace_output_e2e: verilator not found on PATH");
        return;
    }

    let root = fresh_root();
    let fixture = root.join("smoke.harc");
    std::fs::write(&fixture, SMOKE).expect("write smoke fixture");

    let nested_trace = root.join("fresh/nested/trace.jsonl");
    assert!(!nested_trace.parent().unwrap().exists());
    let fresh = run_sim(&root.join("fresh-build"), &nested_trace, &fixture);
    assert!(
        fresh.status.success(),
        "fresh nested trace run failed:\n{}",
        combined_output(&fresh)
    );
    let trace = std::fs::read_to_string(&nested_trace).expect("read nested semantic trace");
    assert!(
        trace.lines().any(|line| {
            line.contains(r#""type":"log""#) && line.contains(r#""message":"hello""#)
        }),
        "nested semantic trace did not contain hello:\n{trace}"
    );

    let directory_trace = root.join("directory-destination");
    std::fs::create_dir(&directory_trace).expect("create directory trace destination");
    let invalid = run_sim(&root.join("invalid-build"), &directory_trace, &fixture);
    let invalid_output = combined_output(&invalid);
    assert!(
        !invalid.status.success(),
        "directory trace destination unexpectedly succeeded:\n{invalid_output}"
    );
    assert!(
        invalid_output.contains("semantic trace output path is a directory")
            && invalid_output.contains("directory-")
            && invalid_output.contains("destination"),
        "directory trace failure lacked a path-specific diagnostic:\n{invalid_output}"
    );
}
