//! Native regression for zero-budget wait-until timeouts.
//!
//! Every `wait until` takes at least one cycle (spec §7.9), so a constant
//! `timeout 0 cycles` is a compile error, while a budget that is zero only
//! at run time is clamped to a one-cycle wait: a false predicate then times
//! out with the usual diagnostics, a true one continues.

use std::path::PathBuf;
use std::process::Command;

const DUT: &str = r#"
module Top(input logic clk, output logic ready);
    assign ready = 1'b0;
endmodule
"#;

fn run_case(
    root: &PathBuf,
    outdir: &PathBuf,
    predicate: &str,
    budget: &str,
) -> std::process::Output {
    let source = outdir.join("zero_wait.harc");
    let body = format!(
        r#"domain SysDomain
  freq_mhz: 100
end domain SysDomain

testbench ZeroWaitTb
    dut : Top
end testbench ZeroWaitTb

impl ZeroWaitTest for ZeroWaitTb
    clock clk = SysDomain

    run
        let zero : uint<8> = 0
        wait 1 cycle
        log(info, "BEFORE=${{cycle_count}}")
        wait until {predicate}
            timeout {budget} cycles fail("zero-budget wait timed out")
        log(info, "CONTINUED=${{cycle_count}}")
    end run
end impl ZeroWaitTest
"#
    );
    std::fs::write(&source, body).expect("write HARC fixture");
    Command::new(env!("CARGO_BIN_EXE_harc"))
        .env("HARC_NO_LEARN", "1")
        .args(["sim", "--sv"])
        .arg(root.join("zero_wait.sv"))
        .arg(&source)
        .args(["--top", "Top", "--outdir"])
        .arg(outdir)
        .args(["--jobs", "2"])
        .output()
        .expect("run zero-budget wait fixture")
}

fn log_of(output: &std::process::Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn zero_budget_false_wait_reports_timeout_and_true_wait_passes() {
    let root =
        std::env::temp_dir().join(format!("harc_wait_until_zero_e2e_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create output directory");
    std::fs::write(root.join("zero_wait.sv"), DUT).expect("write SystemVerilog DUT");

    // A constant zero budget can never be honoured: compile error.
    let const_out = root.join("const");
    std::fs::create_dir_all(&const_out).expect("create const output directory");
    let const_run = run_case(&root, &const_out, "dut.ready == 0", "0");
    let const_log = log_of(&const_run);
    assert!(
        !const_run.status.success(),
        "constant zero-budget wait compiled:\n{const_log}"
    );
    assert!(
        const_log.contains("timeout 0 cycles`: the timeout must be at least 1 cycle"),
        "missing constant zero-budget diagnostic:\n{const_log}"
    );

    // A runtime zero budget is a one-cycle wait: a false predicate times
    // out one cycle later with the usual diagnostics.
    let false_out = root.join("false");
    std::fs::create_dir_all(&false_out).expect("create false output directory");
    let false_run = run_case(&root, &false_out, "dut.ready == 1", "zero");
    let false_log = log_of(&false_run);
    assert!(
        !false_run.status.success(),
        "false zero-budget wait passed:\n{false_log}"
    );
    assert!(
        false_log.contains("zero-budget wait timed out"),
        "missing timeout diagnostic:\n{false_log}"
    );
    assert!(
        false_log.contains("not yet true: dut.ready == 1"),
        "missing predicate diagnostic:\n{false_log}"
    );
    assert!(
        false_log.contains("BEFORE=1") && false_log.contains("CONTINUED=2"),
        "runtime zero budget did not wait exactly one cycle:\n{false_log}"
    );

    // ... and a true predicate continues one cycle later.
    let true_out = root.join("true");
    std::fs::create_dir_all(&true_out).expect("create true output directory");
    let true_run = run_case(&root, &true_out, "dut.ready == 0", "zero");
    let true_log = log_of(&true_run);
    assert!(
        true_run.status.success(),
        "true zero-budget wait failed:\n{true_log}"
    );
    assert!(
        true_log.contains("BEFORE=1") && true_log.contains("CONTINUED=2"),
        "true wait did not continue one cycle later:\n{true_log}"
    );
    assert!(
        true_log.contains("ALL TESTS PASSED"),
        "true wait did not pass:\n{true_log}"
    );
}
