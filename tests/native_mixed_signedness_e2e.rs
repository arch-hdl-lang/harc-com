//! Native regression for mixed-signedness solver constraints.
//!
//! These scenarios used to pin the native solver's promotion rule for a
//! `keep` mixing `uint` and `sint` operands. Owner decision D1 (R16 hunt 1)
//! makes every such mix a compile-time error, so each scenario now asserts
//! the rejection; the untyped-constant case still solves (and is UNSAT).

use std::{fs, path::PathBuf, process::Command};

const CONSTS: &str = r#"
const MIXED_U8 : uint<8> = 2
const POSITIVE_UNTYPED = 2
const NEGATIVE_UNTYPED = -1
"#;

const MIXED64: &str = r#"
transaction Mixed64
    u : uint<64>
    s : sint<64>
    keep u == 0xfffffffffffffffe
    keep s == 2
    keep u / s == 0x7fffffffffffffff
    keep u > s
    keep u % 3 == 2
end transaction Mixed64
"#;

const NARROW_MIXED: &str = r#"
transaction NarrowMixed
    u : uint<8>
    s : sint<64>
    keep u == MIXED_U8
    keep u == POSITIVE_UNTYPED
    keep s == -6
    keep s + u == -4
    keep s < u
end transaction NarrowMixed
"#;

const NESTED_MIXED: &str = r#"
transaction NestedMixed
    u : uint<8>
    s : sint<64>
    keep u == MIXED_U8
    keep u == POSITIVE_UNTYPED
    keep s == -6
    keep s + (u + 0) == -4
    keep s < (1 + 2)
end transaction NestedMixed
"#;

const SHIFT_MIXED: &str = r#"
transaction ShiftMixed
    u : uint<8>
    sh : uint<64>
    s : sint<64>
    keep u == 2
    keep sh == 64
    keep s == -6
    keep s < (u >> sh)
end transaction ShiftMixed
"#;

const NEGATIVE_CONSTANT: &str = r#"
transaction NegativeConstant
    x : uint<8>
    keep x == 0
    keep x < NEGATIVE_UNTYPED
end transaction NegativeConstant
"#;

/// The unsatisfiable intent of `NEGATIVE_CONSTANT` written without a sign
/// mix: no `uint` is below zero.
const BELOW_ZERO: &str = r#"
transaction BelowZero
    x : uint<8>
    keep x == 0
    keep x < 0
end transaction BelowZero
"#;

/// `CONSTS` + one transaction + a test that randomizes it.
fn program(decl: &str, txn: &str) -> String {
    format!(
        "{CONSTS}{decl}test One\n    let dut : Top\n    run\n        let t : {txn}\n        randomize(t)\n    end run\nend test One\n"
    )
}

fn sim(tag: &str, src: &str) -> (bool, String) {
    let outdir = std::env::temp_dir().join(format!(
        "harc_native_mixed_signedness_{tag}_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&outdir);
    fs::create_dir_all(&outdir).expect("create output directory");
    let source = outdir.join(format!("{tag}.harc"));
    fs::write(&source, src).expect("write fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/dut/top_counter.sv"))
        .arg(&source)
        .args(["--top", "Top", "--jobs", "2", "--outdir"])
        .arg(&outdir)
        .env("HARC_NO_LEARN", "1")
        .output()
        .expect("run harc sim");
    (
        output.status.success(),
        format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

#[test]
fn native_mixed_signedness_constraints_are_rejected() {
    // `keep u / s`, `keep s + u`, `keep s + (u + 0)` and `keep s < (u >> sh)`
    // each mix a declared `uint` with a declared `sint`.
    for (tag, decl, txn) in [
        ("mixed64", MIXED64, "Mixed64"),
        ("narrow", NARROW_MIXED, "NarrowMixed"),
        ("nested", NESTED_MIXED, "NestedMixed"),
        ("shift", SHIFT_MIXED, "ShiftMixed"),
    ] {
        let (ok, log) = sim(tag, &program(decl, txn));
        assert!(
            !ok,
            "{txn}: mixed-signedness keep unexpectedly compiled:\n{log}"
        );
        assert!(
            log.contains("mixes signed and unsigned operands"),
            "{txn}: expected the mixed-signedness diagnostic:\n{log}"
        );
    }
}

#[test]
fn native_negative_untyped_constant_is_not_false_sat() {
    // A NEGATIVE untyped constant is a signed value (owner decision, R16
    // review), so `x < NEGATIVE_UNTYPED` on a `uint` is the mixed-signedness
    // error rather than a constraint the solver could falsely satisfy.
    let (ok, log) = sim(
        "negative_const",
        &program(NEGATIVE_CONSTANT, "NegativeConstant"),
    );
    assert!(!ok, "negative uint constraint unexpectedly compiled: {log}");
    assert!(
        log.contains("mixes signed and unsigned operands"),
        "expected the mixed-signedness diagnostic: {log}"
    );
    // The same unsatisfiable intent without the mix still reaches the
    // solver, which must not report SAT.
    let (ok, log) = sim("below_zero", &program(BELOW_ZERO, "BelowZero"));
    assert!(!ok, "`uint < 0` constraint unexpectedly solved: {log}");
    assert!(
        log.contains("UNSAT"),
        "expected solver UNSAT diagnostic: {log}"
    );
}

/// The same four scenarios written with operands of one signedness (the
/// explicit-conversion form a user writes after the D1 rejection), so the
/// native solver's comparison and arithmetic rendering and the runtime
/// checks on the solved values stay covered.
const SAME_SIGN: &str = r#"
const POSITIVE_UNTYPED = 2
transaction Unsigned64
    u : uint<64>
    s : uint<64>
    keep u == 0xfffffffffffffffe
    keep s == 2
    keep u / s == 0x7fffffffffffffff
    keep u > s
    keep u % 3 == 2
end transaction Unsigned64
transaction NarrowSigned
    u : sint<64>
    s : sint<64>
    keep u == 2
    keep u == POSITIVE_UNTYPED
    keep s == -6
    keep s + u == -4
    keep s < u
end transaction NarrowSigned
transaction NestedSigned
    u : sint<64>
    s : sint<64>
    keep u == 2
    keep s == -6
    keep s + (u + 0) == -4
    keep s < (1 + 2)
end transaction NestedSigned
transaction ShiftSigned
    u : sint<64>
    sh : uint<64>
    s : sint<64>
    keep u == 2
    keep sh == 64
    keep s == -6
    keep s < (u >> sh)
end transaction ShiftSigned
test SameSign
    let dut : Top
    run
        let m : Unsigned64
        randomize(m)
        assert m.u == 0xfffffffffffffffe else fail("unsigned 64-bit value")
        let n : NarrowSigned
        randomize(n)
        assert n.s == -6 else fail("signed narrow value")
        let x : NestedSigned
        randomize(x)
        assert x.s == -6 else fail("signed nested value")
        let q : ShiftSigned
        randomize(q)
        assert q.s == -6 else fail("signed shift value")
    end run
end test SameSign
"#;

#[test]
fn native_same_sign_solver_controls() {
    let (ok, log) = sim("same_sign", SAME_SIGN);
    assert!(ok, "same-sign solver controls failed:\n{log}");
    assert!(log.contains("ALL TESTS PASSED"), "{log}");
}
