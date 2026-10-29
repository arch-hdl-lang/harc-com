//! Native regression: coverpoints sample `.sext<N>()` and a signed
//! `.resize<N>()` as signed VALUES, like procedural code. Runs
//! `cov_expr_targets_test`, whose `bins neg_one = {-1}` on
//! `dut.count_out[3:0].sext<8>()`, on a signed `.resize<16>()` (widening)
//! and on a signed `.resize<4>()` (narrowing) must all be hit once the
//! counter's low nibble reaches 0xF. The sampler used to mask sub-64
//! sign extensions to the N-bit pattern (255) and to zero-extend a signed
//! resize, so those bins stayed empty.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn native_coverpoints_sample_signed_width_methods_as_values() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let outdir =
        std::env::temp_dir().join(format!("harc_native_signed_cover_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&outdir);
    std::fs::create_dir_all(&outdir).expect("create output directory");
    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(root.join("tests/dut/top_counter.sv"))
        .arg(root.join("tests/fixtures/cov_expr_targets_test.harc"))
        .args(["--top", "Top", "--jobs", "2", "--outdir"])
        .arg(&outdir)
        .output()
        .expect("run coverage fixture");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("ALL TESTS PASSED"),
        "signed coverpoint fixture failed:\nstdout:\n{}\nstderr:\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains("bin hole"),
        "a signed bin stayed empty:\n{stdout}"
    );
}
