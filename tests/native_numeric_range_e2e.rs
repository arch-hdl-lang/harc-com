//! Native regressions for inclusive numeric range terminal values.

use std::path::PathBuf;
use std::process::Command;

const TB: &str = r#"
testbench NumericRangeTb
    dut : Top
end testbench NumericRangeTb

impl NumericRangeTest for NumericRangeTb
    run
        let umax : uint<64> = 0xffffffffffffffff
        let smax : sint<64> = 0x7fffffffffffffff
        let count = 0

        for i in umax .. umax
            count = count + 1
        end for
        assert count == 1 else fail("unsigned MAX singleton repeated")

        for i in umax - 1 .. umax
            count = count + 1
        end for
        assert count == 3 else fail("unsigned MAX range did not terminate")

        for i in smax .. smax
            count = count + 1
        end for
        assert count == 4 else fail("signed MAX singleton repeated")

        for i in smax - 1 .. smax
            count = count + 1
        end for
        assert count == 6 else fail("signed MAX range did not terminate")

        for i in 1 .. 3
            if i == 2
                continue
            end if
            count = count + 1
        end for
        assert count == 8 else fail("terminal range continue mismatch")

        for i in 1 .. 3
            if i == 2
                break
            end if
            count = count + 1
        end for
        assert count == 9 else fail("range break mismatch")

        for outer in 1 .. 2
            for inner in umax .. umax
                count = count + 1
            end for
        end for
        assert count == 11 else fail("nested terminal ranges mismatch")
    end run
end impl NumericRangeTest
"#;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn native_numeric_range_terminal_values_execute_once() {
    let outdir = std::env::temp_dir().join(format!(
        "harc_native_numeric_range_e2e_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&outdir);
    std::fs::create_dir_all(&outdir).expect("create output directory");
    let source = outdir.join("numeric_range.harc");
    std::fs::write(&source, TB).expect("write range fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(repo_root().join("tests/dut/top_counter.sv"))
        .arg(&source)
        .args(["--top", "Top", "--outdir"])
        .arg(&outdir)
        .args(["--jobs", "2"])
        .output()
        .expect("run native numeric range fixture");
    assert!(
        output.status.success(),
        "native numeric range fixture failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ALL TESTS PASSED"),
        "native numeric range fixture did not report success:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}
