//! Native CLI regression for compiler generated repeat counters.

use std::path::PathBuf;
use std::process::Command;

const TB: &str = r#"
const __harc_repeat_counter_0 : uint<64> = 3

testbench RepeatCounterTb
    dut : Top
end testbench RepeatCounterTb

impl RepeatCounterTest for RepeatCounterTb
    run
        let __harc_repeat_counter_1 : uint<64> = 99
        let count : uint<64> = 0
        repeat __harc_repeat_counter_0
            count = count +% 1
        end repeat
        assert count == 3 else fail("repeat bound did not execute exactly 3 times")

        let _r : uint<64> = 2
        repeat 2
            _r = _r +% 1
            repeat _r
                count = count +% 1
            end repeat
        end repeat
        assert _r == 4 else fail("source _r was not writable")
        assert count == 10 else fail("nested repeat count mismatch")
    end run
end impl RepeatCounterTest
"#;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn native_repeat_counter_hygiene_executes_source_bounds() {
    let outdir = std::env::temp_dir().join(format!(
        "harc_native_repeat_counter_e2e_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&outdir);
    std::fs::create_dir_all(&outdir).expect("create output directory");
    let source = outdir.join("repeat_counter.harc");
    std::fs::write(&source, TB).expect("write repeat fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(repo_root().join("tests/dut/top_counter.sv"))
        .arg(&source)
        .args(["--top", "Top", "--outdir"])
        .arg(&outdir)
        .args(["--jobs", "2"])
        .output()
        .expect("run native repeat fixture");
    assert!(
        output.status.success(),
        "native repeat fixture failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ALL TESTS PASSED"),
        "native repeat fixture did not report success:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}
