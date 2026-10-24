//! Native regression for zero-budget coroutine wait-until results.

use std::path::PathBuf;
use std::process::Command;

const DUT: &str = r#"
module Top(input logic clk, output logic ready);
    assign ready = 1'b0;
endmodule
"#;

fn run_case(root: &PathBuf, outdir: &PathBuf, predicate: &str) -> std::process::Output {
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
        wait until {predicate}
            timeout 0 cycles fail("zero-budget wait timed out")
        log(info, "CONTINUED")
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

#[test]
fn zero_budget_false_wait_reports_timeout_and_true_wait_passes() {
    let root =
        std::env::temp_dir().join(format!("harc_wait_until_zero_e2e_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create output directory");
    std::fs::write(root.join("zero_wait.sv"), DUT).expect("write SystemVerilog DUT");

    let false_out = root.join("false");
    std::fs::create_dir_all(&false_out).expect("create false output directory");
    let false_run = run_case(&root, &false_out, "dut.ready == 1");
    let false_log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&false_run.stdout),
        String::from_utf8_lossy(&false_run.stderr)
    );
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

    let true_out = root.join("true");
    std::fs::create_dir_all(&true_out).expect("create true output directory");
    let true_run = run_case(&root, &true_out, "dut.ready == 0");
    let true_log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&true_run.stdout),
        String::from_utf8_lossy(&true_run.stderr)
    );
    assert!(
        true_run.status.success(),
        "true zero-budget wait failed:\n{true_log}"
    );
    assert!(
        true_log.contains("CONTINUED"),
        "true wait did not continue:\n{true_log}"
    );
    assert!(
        true_log.contains("ALL TESTS PASSED"),
        "true wait did not pass:\n{true_log}"
    );
}
