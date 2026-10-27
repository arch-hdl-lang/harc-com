//! Native regression for signed folded covergroup bounds.

use std::process::Command;

const DUT: &str = r#"module Top(input logic clk); endmodule"#;

const TEST: &str = r#"
const NEG8 : sint<8> = -1
const MIN64 : sint<64> = -9223372036854775808
const UMAX64 : uint<64> = 0xFFFFFFFFFFFFFFFF

scoreboard Sink
    calls : uint<32> default 0
    hookable accept(value: sint<8>, lo: sint<8>, hi: sint<8>, uvalue: uint<64>)
        assert value >= lo else fail("signed lower control")
        assert value <= hi else fail("signed upper control")
        calls = calls + 1
    end accept
end scoreboard Sink

covergroup Cov @(sink.accept(value, lo, hi, uvalue) post)
    signed_cp : cover value
        bins
            cross_zero = [NEG8..1]
            from_min = [MIN64..1]
            negative_only = [NEG8..NEG8]
            positive_only = [1..1]
            runtime_equivalent = [lo..hi]
        end bins
    unsigned_cp : cover uvalue
        bins
            high_bit = [UMAX64..UMAX64]
        end bins
end covergroup Cov

testbench Tb
    dut : Top
    sink : Sink
    cov : Cov
end testbench Tb

impl Test for Tb
    run
        sink.accept(-1, -1, 1, UMAX64)
        sink.accept(0, -1, 1, 0)
        sink.accept(1, -1, 1, 1)
        wait 1 cycle
    end run
    check
        assert sink.calls == 3 else fail("delivery")
        assert cov.signed_cp.cross_zero == 3 else fail("cross zero")
        assert cov.signed_cp.from_min == 3 else fail("minimum signed bound")
        assert cov.signed_cp.negative_only == 1 else fail("negative only")
        assert cov.signed_cp.positive_only == 1 else fail("positive only")
        assert cov.signed_cp.runtime_equivalent == 3 else fail("runtime equivalent")
        assert cov.unsigned_cp.high_bit == 1 else fail("unsigned high bit")
    end check
end impl Test
"#;

#[test]
fn static_signed_cover_bins_native_regression() {
    let verilator_present = Command::new("verilator")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    assert!(
        verilator_present || std::env::var_os("HARC_REQUIRE_VERILATOR").is_none(),
        "HARC_REQUIRE_VERILATOR is set but `verilator` is not on PATH"
    );
    if !verilator_present {
        eprintln!("skipping static cover bins regression: verilator not found");
        return;
    }
    let outdir =
        std::env::temp_dir().join(format!("harc_static_cover_bins_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&outdir);
    std::fs::create_dir_all(&outdir).expect("create output directory");
    std::fs::write(outdir.join("Top.sv"), DUT).expect("write DUT");
    std::fs::write(outdir.join("static_cover_bins.harc"), TEST).expect("write HARC test");
    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--codegen", "tbir", "--sv"])
        .arg(outdir.join("Top.sv"))
        .arg(outdir.join("static_cover_bins.harc"))
        .args(["--top", "Top", "--outdir"])
        .arg(&outdir)
        .args(["--jobs", "2"])
        .env("HARC_NO_LEARN", "1")
        .env("ARCH_NO_LEARN", "1")
        .env("HARC_REQUIRE_VERILATOR", "1")
        .output()
        .expect("spawn harc sim");
    assert!(
        output.status.success(),
        "static cover bins simulation failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let cpp =
        std::fs::read_to_string(outdir.join("static_cover_bins.cpp")).expect("read generated C++");
    assert!(
        cpp.contains("_v >= -1 && _v <= 1"),
        "narrow signed bounds were not emitted as signed operands: {cpp}"
    );
    assert!(
        cpp.contains("(-9223372036854775807LL - 1)"),
        "minimum signed bound lost its safe signed spelling: {cpp}"
    );
    assert!(
        !cpp.contains("_v >= 18446744073709551615 && _v <= 1)"),
        "negative bound became unsigned: {cpp}"
    );
    let _ = std::fs::remove_dir_all(&outdir);
}
