//! End-to-end regression for the TLM/handshake wait-timeout diagnostic
//! (harc-com#416).
//!
//! Before #416, the generated BFM's bounded handshake-wait loops
//! (`while (!sig && _b > 0) { ... }`) fell through **silently** when the
//! bound expired, so a stalled DUT surfaced only as a confusing downstream
//! wrong-value assertion. #416 makes each expiry emit
//! `sim_log_line("FAIL", "... timed out after N cycles (sig stuck low)")`
//! and bump the error count.
//!
//! Snapshot tests in `tests/tbir.rs` lock the *emitted text*; this file
//! proves the diagnostic actually **fires** under a real Verilator run by
//! binding `tests/fixtures/tlm_stall_timeout_test.harc` to
//! `tests/dut/TlmStallMemory.sv` — a `read` target that never asserts
//! `req_ready`/`rsp_valid`. Both BFM emitters are covered: the test runs
//! once per `--codegen` backend (`v1` and the default `tbir`), since the
//! diagnostic is injected at per-backend emission sites that share one
//! helper.
//!
//! Gated on Verilator being installed; skips (passes) otherwise, matching
//! `tests/trace_merge_e2e.rs`.

use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn harc_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_harc"))
}

/// True if a `verilator` binary is on PATH and runnable. The test needs a
/// plain `--binary` build (no `--trace-vcd`), so any v5 is sufficient.
fn verilator_present() -> bool {
    let present = Command::new("verilator")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    // See the same guard in `tbir_wide_scoreboard_e2e.rs`. A skipped
    // end-to-end test reports `ok`, and an `ok` in 0.00s for a test that
    // builds a Verilator model reads exactly like a real pass in a CI
    // log — which is how harc#662 stayed hidden. CI installs Verilator
    // for the `cargo test` job and sets this variable, so the silent
    // skip cannot come back unnoticed.
    assert!(
        present || std::env::var_os("HARC_REQUIRE_VERILATOR").is_none(),
        "HARC_REQUIRE_VERILATOR is set but `verilator` is not on PATH: this \
         end-to-end test would have skipped itself and reported success"
    );
    present
}

fn fresh_outdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "harc_tlm_wait_timeout_e2e_{}_{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp outdir");
    dir
}

/// Run `harc sim` for the stall fixture under one codegen backend and
/// return `(success, combined_output)`.
fn run_stall_sim(codegen: &str) -> (bool, String) {
    let root = workspace_root();
    let sv = root.join("tests/dut/TlmStallMemory.sv");
    let fixture = root.join("tests/fixtures/tlm_stall_timeout_test.harc");
    let outdir = fresh_outdir(codegen);

    let out = Command::new(harc_bin())
        .arg("sim")
        .arg("--codegen")
        .arg(codegen)
        .arg("--sv")
        .arg(&sv)
        .arg(&fixture)
        .arg("--top")
        .arg("TlmStallMemory")
        .arg("--outdir")
        .arg(&outdir)
        .output()
        .expect("spawn harc sim");

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let combined = format!("--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}");
    (out.status.success(), combined)
}

fn assert_timeout_diagnostic_fires(codegen: &str) {
    if !verilator_present() {
        eprintln!("skipping tlm_wait_timeout_e2e[{codegen}]: verilator not found on PATH");
        return;
    }

    let (success, output) = run_stall_sim(codegen);

    // The stall DUT can never accept, so the run MUST fail (non-zero exit).
    // A green run would mean the timeout silently fell through — the exact
    // regression #416 closed.
    assert!(
        !success,
        "[{codegen}] stall fixture unexpectedly PASSED — the wait-timeout \
         diagnostic did not fail the run:\n{output}"
    );

    // Both the request (`req_ready`) and response (`rsp_valid`) waits expire,
    // each emitting a FAIL with the new "timed out after N cycles" wording.
    assert!(
        output.contains("TLM mem.read request timed out after 16 cycles"),
        "[{codegen}] missing req_ready timeout diagnostic:\n{output}"
    );
    assert!(
        output.contains("TLM mem.read response timed out after 16 cycles"),
        "[{codegen}] missing rsp_valid timeout diagnostic:\n{output}"
    );
    assert!(
        output.contains("mem_read_req_ready stuck low"),
        "[{codegen}] timeout message should name the stuck signal:\n{output}"
    );
}

/// v1 (legacy direct-AST `cpp_tb`) emitter.
#[test]
fn tlm_wait_timeout_diagnostic_fires_v1() {
    assert_timeout_diagnostic_fires("v1");
}

/// Default TB-IR emitter.
#[test]
fn tlm_wait_timeout_diagnostic_fires_tbir() {
    assert_timeout_diagnostic_fires("tbir");
}
