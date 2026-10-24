//! Native regression for mixed-width and same-width solver arithmetic.

use std::{fs, path::PathBuf, process::Command};

const TEST: &str = r#"
const MIXED_U8 : uint<8> = 2
const POSITIVE_UNTYPED = 2
const NEGATIVE_UNTYPED = -1
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
    keep u == POSITIVE_UNTYPED
    keep s == -6
    keep s + u == -4
    keep s < u
end transaction NarrowMixed
transaction NestedMixed
    u : uint<8>
    s : sint<64>
    keep u == MIXED_U8
    keep u == POSITIVE_UNTYPED
    keep s == -6
    keep s + (u + 0) == -4
    keep s < (1 + 2)
end transaction NestedMixed
transaction ShiftMixed
    u : uint<8>
    sh : uint<64>
    s : sint<64>
    keep u == 2
    keep sh == 64
    keep s == -6
    keep s < (u >> sh)
end transaction ShiftMixed
transaction NegativeConstant
    x : uint<8>
    keep x == 0
    keep x < NEGATIVE_UNTYPED
end transaction NegativeConstant
test MixedSignedness
    let dut : Top
    run
        let m : Mixed64
        randomize(m)
        assert m.u == 0xfffffffffffffffe else fail("mixed uint64 value")
        let n : NarrowMixed
        randomize(n)
        assert n.s == -6 else fail("narrow signed promotion")
        let x : NestedMixed
        randomize(x)
        assert x.s == -6 else fail("nested signed promotion")
        let q : ShiftMixed
        randomize(q)
        assert q.s == -6 else fail("shift result width promotion")
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

#[test]
fn native_negative_untyped_constant_is_not_false_sat() {
    let outdir =
        std::env::temp_dir().join(format!("harc_native_negative_const_{}", std::process::id()));
    let _ = fs::remove_dir_all(&outdir);
    fs::create_dir_all(&outdir).expect("create output directory");
    let source = outdir.join("negative_const.harc");
    fs::write(
        &source,
        TEST.replace(
            "let m : Mixed64",
            "let n : NegativeConstant\n        randomize(n)\n        let m : Mixed64",
        ),
    )
    .expect("write negative constant fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/dut/top_counter.sv"))
        .arg(&source)
        .args(["--top", "Top", "--jobs", "2", "--outdir"])
        .arg(&outdir)
        .env("HARC_NO_LEARN", "1")
        .output()
        .expect("run negative constant fixture");
    let log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "negative uint constraint unexpectedly solved: {log}"
    );
    assert!(
        log.contains("UNSAT"),
        "expected solver UNSAT diagnostic: {log}"
    );
}
