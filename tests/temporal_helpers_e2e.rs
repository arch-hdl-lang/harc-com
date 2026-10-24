//! End-to-end coverage for native v1 temporal helper state.
//!
//! Each helper's state is stored in the per-run runtime-cell context.  This
//! fixture exercises all four helpers through the public CLI and runs the
//! same source with v1 and the default TB-IR backend, so an unqualified
//! write-back cannot hide behind emit-only coverage.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn harc_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_harc"))
}

fn fresh_outdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "harc_temporal_helpers_e2e_{}_{}",
        tag,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temporary fixture directory");
    dir
}

fn verilator_present() -> bool {
    let present = Command::new("verilator")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    assert!(
        present || std::env::var_os("HARC_REQUIRE_VERILATOR").is_none(),
        "HARC_REQUIRE_VERILATOR is set but verilator is unavailable"
    );
    present
}

const DUT: &str = r#"module Top(input logic clk, input logic rst, input logic en); endmodule
"#;

const TEMPORAL_TB: &str = r#"testbench Tb
    dut : Top
end testbench Tb

impl TemporalHelpers for Tb
    run
        dut.rst = 1
        dut.en = 0
        assert property stable(dut.en)
        assert property past(dut.en) == 0
        assert property !rose(dut.en)
        assert property !fell(dut.en)
        wait 2 cycles
    end run
end impl TemporalHelpers
"#;

fn run_backend(codegen: Option<&str>) {
    let outdir = fresh_outdir(codegen.unwrap_or("default"));
    let source = outdir.join("temporal.harc");
    let dut = outdir.join("Top.sv");
    fs::write(&source, TEMPORAL_TB).expect("write HARC fixture");
    fs::write(&dut, DUT).expect("write SV fixture");

    let mut command = Command::new(harc_bin());
    command
        .env("HARC_NO_LEARN", "1")
        .arg("sim")
        .arg("--sv")
        .arg(&dut)
        .arg(&source)
        .arg("--top")
        .arg("Top")
        .arg("--jobs")
        .arg("2")
        .arg("--outdir")
        .arg(&outdir);
    if let Some(codegen) = codegen {
        command.args(["--codegen", codegen]);
    }
    let output = command.output().expect("spawn harc sim");
    assert!(
        output.status.success(),
        "temporal helper simulation failed for {}:\n{}\n{}",
        codegen.unwrap_or("default"),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = fs::remove_dir_all(&outdir);
}

#[test]
fn temporal_helpers_compile_and_run_with_v1_and_default_codegen() {
    if !verilator_present() {
        eprintln!("skipping temporal helper e2e: verilator not found on PATH");
        return;
    }

    run_backend(Some("v1"));
    run_backend(None);
}
