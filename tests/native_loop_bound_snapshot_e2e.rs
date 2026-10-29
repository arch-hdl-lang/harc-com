//! Native regression: `for i in lo .. hi` upper bounds and `repeat N` counts
//! are evaluated once, on loop entry. A body that reassigns the bound local
//! must not change the iteration count, and a bare local bound must behave
//! exactly like the equivalent expression `(bound + 0)`.

use std::path::PathBuf;
use std::process::Command;

const TB: &str = r#"
domain SysDomain
    freq_mhz: 100
end domain SysDomain

test Bounds
    let dut : Top
    clock clk = SysDomain
    run
        let bound : uint<8> = 3
        let seen : uint<8> = 0
        for i in 0 .. bound
            seen = seen + 1
            bound = 0
        end for
        assert seen == 4 else fail("for bound was reevaluated: ${seen}")

        bound = 3
        seen = 0
        repeat bound
            seen = seen + 1
            bound = 0
        end repeat
        assert seen == 3 else fail("repeat count was reevaluated: ${seen}")

        bound = 3
        seen = 0
        for i in 0 .. (bound + 0)
            seen = seen + 1
            bound = 0
        end for
        assert seen == 4 else fail("for (bound + 0) was reevaluated: ${seen}")

        bound = 3
        seen = 0
        repeat (bound + 0)
            seen = seen + 1
            bound = 0
        end repeat
        assert seen == 3 else fail("repeat (bound + 0) was reevaluated: ${seen}")

        bound = 1
        seen = 0
        for i in 0 .. bound
            seen = seen + 1
            bound = 9
        end for
        assert seen == 2 else fail("growing for bound was reevaluated: ${seen}")

        bound = 1
        seen = 0
        repeat bound
            seen = seen + 1
            bound = 9
        end repeat
        assert seen == 1 else fail("growing repeat count was reevaluated: ${seen}")
        log(info, "LOOP_BOUNDS_OK")
    end run
end test Bounds
"#;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn native_loop_bounds_are_snapshotted_on_entry() {
    let outdir = std::env::temp_dir().join(format!(
        "harc_native_loop_bound_snapshot_e2e_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&outdir);
    std::fs::create_dir_all(&outdir).expect("create output directory");
    let source = outdir.join("loop_bounds.harc");
    std::fs::write(&source, TB).expect("write loop-bound fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(repo_root().join("tests/dut/top_counter.sv"))
        .arg(&source)
        .args(["--top", "Top", "--outdir"])
        .arg(&outdir)
        .args(["--jobs", "2"])
        .output()
        .expect("run native loop-bound fixture");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("ALL TESTS PASSED"),
        "native loop-bound fixture failed:\nstdout:\n{}\nstderr:\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("LOOP_BOUNDS_OK"),
        "missing marker:\n{stdout}"
    );
}
