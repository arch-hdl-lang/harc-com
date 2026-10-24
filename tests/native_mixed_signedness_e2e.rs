//! Native regression for mixed-width and same-width solver arithmetic.

use std::{fs, path::PathBuf, process::Command};

const TEST: &str = r#"
const MIXED_U8 : uint<8> = 2
transaction Mixed64
    u : uint<64>
    s : sint<64>
    keep u == 0xfffffffffffffffe
    keep s == 2
    keep u / s == 0x7fffffffffffffff
    keep u > s
    keep u % 3 == 2
end transaction Mixed64
transaction NarrowMixed
    u : uint<8>
    s : sint<64>
    keep u == MIXED_U8
    keep s == -6
    keep s + u == -4
    keep s < u
end transaction NarrowMixed
test MixedSignedness
    let dut : Top
    run
        let m : Mixed64
        randomize(m)
        assert m.u == 0xfffffffffffffffe else fail("mixed uint64 value")
        let n : NarrowMixed
        randomize(n)
        assert n.s == -6 else fail("narrow signed promotion")
    end run
end test MixedSignedness
"#;

#[test]
fn native_mixed_signedness_solver_controls() {
    let outdir = std::env::temp_dir().join(format!(
        "harc_native_mixed_signedness_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&outdir);
    fs::create_dir_all(&outdir).expect("create output directory");
    let source = outdir.join("mixed_signedness.harc");
    fs::write(&source, TEST).expect("write mixed signedness fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/dut/top_counter.sv"))
        .arg(&source)
        .args(["--top", "Top", "--jobs", "2", "--outdir"])
        .arg(&outdir)
        .env("HARC_NO_LEARN", "1")
        .output()
        .expect("run mixed signedness fixture");
    assert!(
        output.status.success(),
        "mixed signedness failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("ALL TESTS PASSED"));
}
