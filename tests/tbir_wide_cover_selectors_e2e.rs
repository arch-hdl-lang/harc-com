//! TBIR-only compile/run gate for wide covergroup selector expressions.
//!
//! v1 emits ambiguous C++ conversions for these selectors, so this case
//! intentionally does not belong in the v1/TBIR equivalence registry.

use std::path::PathBuf;
use std::process::Command;

fn verilator_present() -> bool {
    let present = Command::new("verilator")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    assert!(
        present || std::env::var_os("HARC_REQUIRE_VERILATOR").is_none(),
        "HARC_REQUIRE_VERILATOR is set but `verilator` is not on PATH"
    );
    present
}

#[test]
fn tbir_wide_cover_selectors_compile_and_run() {
    if !verilator_present() {
        eprintln!("skipping tbir_wide_cover_selectors_e2e: verilator not found on PATH");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/fixtures/packed_vec_lane_test.harc");
    let base = std::fs::read_to_string(&fixture).expect("read packed-Vec fixture");
    let source_text = base
        .replace(
            "cover dut.lane_id_out[dut.lane_valid_out[0]]",
            "cover dut.lane_id_out[(dut.lane_valid_out[0] as uint<200>)]",
        )
        .replace(
            "    cp_unpacked0 : cover",
            "    cp_wide_slice : cover dut.lane_valid_out[(dut.lane_valid_out[0] as uint<200>) + 1:(dut.lane_valid_out[0] as uint<200>)]\n        bins\n            two = {2}\n        end bins\n    cp_oob_slice : cover dut.lane_valid_out[((dut.lane_valid_out[0] as uint<200>) << 130):dut.lane_valid_out[0]]\n        bins\n            zero = {0}\n        end bins\n    cp_unpacked0 : cover",
        )
        .replace(
            "        assert cov.cp_unpacked0.u42 > 0",
            "        assert cov.cp_wide_slice.two > 0 else fail(\"wide runtime slice coverpoint did not hit\")\n        assert cov.cp_oob_slice.zero > 0 else fail(\"out-of-range wide slice did not return zero\")\n        assert cov.cp_unpacked0.u42 > 0",
        );
    assert_ne!(source_text, base, "wide-selector probe substitutions applied");

    let outdir = std::env::temp_dir().join(format!(
        "harc_tbir_wide_cover_selectors_e2e_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&outdir);
    std::fs::create_dir_all(&outdir).expect("create temp outdir");
    let source = outdir.join("wide_cover_selectors.harc");
    std::fs::write(&source, source_text).expect("write HARC probe");

    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .arg("sim")
        .arg("--codegen")
        .arg("tbir")
        .arg("--sv")
        .arg(root.join("tests/dut/packed_vec_lane.sv"))
        .arg(&source)
        .arg("--top")
        .arg("PackedVecLane")
        .arg("--outdir")
        .arg(&outdir)
        .output()
        .expect("spawn harc sim");
    assert!(
        output.status.success(),
        "TBIR wide-cover-selector simulation failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let _ = std::fs::remove_dir_all(&outdir);
}
