//! Native regression for nested prefix negation rendering.

use std::{fs, path::PathBuf, process::Command};

const TEST: &str = r#"
testbench UnaryNegationTb
    dut : Top
end testbench UnaryNegationTb

impl UnaryNegationTest for UnaryNegationTb
    run
        let x : sint<32> = 7
        let y : sint<32> = - -x
        assert y == 7 else fail("double negation")
        assert x == 7 else fail("double negation mutated operand")

        let a : sint<32> = 9
        let b : sint<32> = -(-a)
        assert b == 9 else fail("parenthesized negation")
        assert a == 9 else fail("parenthesized negation mutated operand")

        let c : sint<32> = 11
        let d : sint<32> = - - -c
        assert d == -11 else fail("triple negation")
        assert c == 11 else fail("triple negation mutated operand")

        let numeric : sint<32> = - - -5
        assert numeric == -5 else fail("numeric triple negation")
        wait 1 cycle
    end run
end impl UnaryNegationTest
"#;

#[test]
fn native_nested_unary_negation_matches_across_backends() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let base = std::env::temp_dir().join(format!(
        "harc_native_unary_negation_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).expect("create output directory");
    let source = base.join("unary_negation.harc");
    fs::write(&source, TEST).expect("write unary negation fixture");

    for backend in ["v1", "tbir"] {
        let outdir = base.join(backend);
        let output = Command::new(env!("CARGO_BIN_EXE_harc"))
            .args(["sim", "--sv"])
            .arg(root.join("tests/dut/top_counter.sv"))
            .arg(&source)
            .args(["--top", "Top", "--codegen", backend, "--jobs", "2", "--outdir"])
            .arg(&outdir)
            .env("HARC_NO_LEARN", "1")
            .output()
            .expect("run unary negation fixture");
        assert!(
            output.status.success(),
            "{backend} unary negation failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("ALL TESTS PASSED"),
            "{backend} unary negation did not report passing tests"
        );
    }
}
