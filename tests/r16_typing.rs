//! R16 hunt-1 typing decisions (owner decisions D1/D2, aggregates-1,
//! components-2): mixed signedness, ARCH-style result widths, aggregate
//! slot narrowing, and literal writes into signed component fields.

use harc::codegen::merge;
use harc::ir::{self, lower, verify};
use harc::parser::parse_source;

fn lower_src(src: &str) -> Result<ir::TbProgram, lower::LowerError> {
    let parsed = parse_source(src).expect("source parses");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    lower::lower_program(&merged)
}

fn lower_and_verify(src: &str) -> ir::TbProgram {
    let prog = lower_src(src).unwrap_or_else(|e| panic!("lowering failed: {e}"));
    if let Err(errs) = verify::verify_program(&prog) {
        panic!("verification failed: {errs:?}");
    }
    prog
}

fn lower_err(src: &str) -> String {
    match lower_src(src) {
        Ok(_) => panic!("expected a lowering error"),
        Err(e) => e.to_string(),
    }
}

fn run_body(decls: &str, body: &str) -> String {
    format!("{decls}\ntest T\n    let dut : Top\n    run\n{body}\n    end run\nend test T\n")
}

// ---------------------------------------------------------------- D1

#[test]
fn d1_rejects_mixed_signedness_in_procedural_binary_ops() {
    for op in ["<", "==", "+", "&"] {
        let src = run_body(
            "",
            &format!(
                "        let s : sint<8> = 0 - 1\n        let u : uint<8> = 1\n        let x = s {op} u\n        log(info, \"${{x}}\")"
            ),
        );
        let err = lower_err(&src);
        assert!(
            err.contains("mixes signed and unsigned operands"),
            "{op}: {err}"
        );
    }
}

#[test]
fn d1_untyped_literals_and_constants_adapt_to_their_peer() {
    let src = run_body(
        "const N = 3\nconst M = -2",
        "        let s : sint<8> = 0 - 1\n        let u : uint<8> = 1\n        let a = s < 1\n        let b = u < N\n        let c = s < M\n        let d = (s as uint<8>) < u\n        log(info, \"${a} ${b} ${c} ${d}\")",
    );
    lower_and_verify(&src);
}

#[test]
fn d1_rejects_mixed_signedness_in_constant_expressions() {
    let src = run_body(
        "const CA : sint<8> = -1\nconst CB : uint<8> = 1\nconst BAD = CA < CB",
        "        log(info, \"${BAD}\")",
    );
    let err = lower_err(&src);
    assert!(err.contains("mixes signed and unsigned operands"), "{err}");
}

#[test]
fn d1_rejects_mixed_signedness_in_a_keep_clause() {
    let src = run_body(
        "transaction M\n    s : sint<4>\n    u : uint<4>\n    keep s < u\nend transaction M",
        "        let t : M\n        randomize(t)",
    );
    let err = lower_err(&src);
    assert!(err.contains("in a randomize constraint"), "{err}");
    assert!(err.contains("mixes signed and unsigned operands"), "{err}");
}

#[test]
fn d1_rejects_mixed_signedness_against_an_outer_local_in_a_with_block() {
    let src = run_body(
        "transaction S\n    s : sint<16>\nend transaction S",
        "        let t : S\n        let lc : uint<16> = 5\n        blocking randomize(t) with\n            t.s < lc\n        end randomize",
    );
    let err = lower_err(&src);
    assert!(err.contains("mixes signed and unsigned operands"), "{err}");
}

// ---------------------------------------------------------------- D2

#[test]
fn d2_plain_add_into_same_width_slot_narrows() {
    let src = run_body(
        "",
        "        let a : uint<8> = 255\n        let b : uint<8> = a + 1\n        log(info, \"${b}\")",
    );
    let err = lower_err(&src);
    assert!(err.contains("widens to 9 bits"), "{err}");
}

#[test]
fn d2_multiply_widens_to_the_sum_of_operand_widths() {
    let src = run_body(
        "",
        "        let a : uint<8> = 200\n        let b : uint<15> = a * a\n        log(info, \"${b}\")",
    );
    let err = lower_err(&src);
    assert!(err.contains("widens to 16 bits"), "{err}");
}

#[test]
fn d2_wrapping_truncating_and_wide_enough_forms_are_accepted() {
    let src = run_body(
        "",
        "        let a : uint<8> = 255\n        let b : uint<8> = a +% 1\n        let c : uint<8> = (a + 1).trunc<8>()\n        let d : uint<9> = a + 1\n        let e : uint<8> = a << 1\n        let f : uint<8> = a & 0xF0\n        log(info, \"${b} ${c} ${d} ${e} ${f}\")",
    );
    lower_and_verify(&src);
}

#[test]
fn d2_applies_to_component_field_read_modify_write() {
    let src = run_body(
        "struct R\n    h : uint<8>\nend struct R",
        "        let r : R\n        r.h = 255\n        r.h = r.h + 1\n        log(info, \"${r.h}\")",
    );
    let err = lower_err(&src);
    assert!(err.contains("widens to 9 bits"), "{err}");
}

// ---------------------------------------------------------- aggregates-1

#[test]
fn aggregates1_rejects_narrowing_into_a_record_field() {
    let src = run_body(
        "struct R\n    h : uint<8>\nend struct R",
        "        let a : uint<16> = 300\n        let r : R\n        r.h = a",
    );
    let err = lower_err(&src);
    assert!(
        err.contains("record field `R.h`") && err.contains("narrows"),
        "{err}"
    );
}

#[test]
fn aggregates1_rejects_signedness_relabel_into_a_transaction_field() {
    let src = run_body(
        "transaction T\n    h : uint<8>\nend transaction T",
        "        let n : sint<8> = 0 - 1\n        let t : T\n        t.h = n",
    );
    let err = lower_err(&src);
    assert!(err.contains("Signedness must match"), "{err}");
}

#[test]
fn aggregates1_rejects_narrowing_into_vec_elements() {
    let rec = run_body(
        "struct R\n    v : Vec<uint<8>, 2>\nend struct R",
        "        let a : uint<16> = 300\n        let r : R\n        r.v[0] = a",
    );
    let err = lower_err(&rec);
    assert!(
        err.contains("element of `Vec` field") && err.contains("narrows"),
        "{err}"
    );
    let local = run_body(
        "",
        "        let a : uint<16> = 300\n        let v : Vec<uint<8>, 2>\n        v[0] = a",
    );
    let err = lower_err(&local);
    assert!(
        err.contains("element of fixed-vector local `v`") && err.contains("narrows"),
        "{err}"
    );
}

// ---------------------------------------------------------- components-2

#[test]
fn components2_negative_literal_into_wide_signed_field_verifies() {
    let src = "agent A\n    r : sint<200> default 0\n    hookable set()\n        r = -1\n    end set\nend agent A\ntest T\n    let dut : Top\n    let x : A\n    run\n        x.set()\n    end run\nend test T\n";
    lower_and_verify(src);
}

#[test]
fn components2_positive_literal_into_signed_field_is_accepted() {
    for width in [8, 64, 65, 200] {
        let src = format!(
            "agent A\n    r : sint<{width}> default 0\n    hookable set()\n        r = 5\n    end set\nend agent A\ntest T\n    let dut : Top\n    let x : A\n    run\n        x.set()\n    end run\nend test T\n"
        );
        lower_and_verify(&src);
    }
}

#[test]
fn components2_out_of_range_positive_literal_names_the_signed_range() {
    let src = "agent A\n    r : sint<8> default 0\n    hookable set()\n        r = 200\n    end set\nend agent A\ntest T\n    let dut : Top\n    let x : A\n    run\n        x.set()\n    end run\nend test T\n";
    let err = lower_err(src);
    assert!(err.contains("does not fit") && err.contains("127"), "{err}");
    assert!(!err.contains("as sint<"), "{err}");
}

// ------------------------------------------- D2 inference / const / return

#[test]
fn d2_untyped_let_infers_the_widened_type() {
    let src = run_body(
        "",
        "        let a : uint<8> = 255\n        let c = a + 1\n        let d : uint<8> = c\n        log(info, \"${d}\")",
    );
    let err = lower_err(&src);
    assert!(
        err.contains("9-bit value") && err.contains("narrows"),
        "{err}"
    );
    let ok = run_body(
        "",
        "        let a : uint<8> = 255\n        let c = a + 1\n        let d : uint<9> = c\n        let z : uint<16> = (a + 1).zext<16>()\n        log(info, \"${d} ${z}\")",
    );
    lower_and_verify(&ok);
}

#[test]
fn d2_applies_to_typed_const_initializers() {
    let src = run_body(
        "const N : uint<32> = 16\nconst L : uint<32> = N - 1",
        "        log(info, \"${L}\")",
    );
    let err = lower_err(&src);
    assert!(err.contains("widens to 33 bits"), "{err}");
    let ok = run_body(
        "const N : uint<32> = 16\nconst L : uint<32> = N -% 1",
        "        log(info, \"${L}\")",
    );
    lower_and_verify(&ok);
}

#[test]
fn d2_applies_to_helper_return_values() {
    let src = "function sum4(a: uint<4>, b: uint<4>) -> uint<4>\n    return a + b\nend function sum4\ntest T\n    let dut : Top\n    run\n        let s = sum4(3, 4)\n        log(info, \"${s}\")\n    end run\nend test T\n";
    let err = lower_err(src);
    assert!(
        err.contains("the return value") && err.contains("widens to 5 bits"),
        "{err}"
    );
}
