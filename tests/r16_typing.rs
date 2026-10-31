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
    // The single strict-fit diagnostic names the signed range (no
    // meaningless source-width `as sint<bits>` relabel hint).
    assert!(err.contains("does not fit") && err.contains("127"), "{err}");
    assert!(err.contains("declared `sint<8>`"), "{err}");
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
    assert!(err.contains("33-bit value"), "{err}");
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

// ------------------------------------------------ review (R16 typing-1)

#[test]
fn negative_untyped_literal_or_const_against_uint_is_a_mix() {
    for (decls, expr) in [("", "a < -1"), ("const NEG = -5", "a < NEG")] {
        let src = run_body(
            decls,
            &format!("        let a : uint<8> = 3\n        let x = {expr}\n        log(info, \"${{x}}\")"),
        );
        let err = lower_err(&src);
        assert!(
            err.contains("mixes signed and unsigned operands"),
            "{expr}: {err}"
        );
    }
    // Non-negative literals and constants still adapt.
    let ok = run_body(
        "const K = 3",
        "        let a : uint<8> = 3\n        let x = a < 1\n        let y = a < K\n        log(info, \"${x} ${y}\")",
    );
    lower_and_verify(&ok);
    // Constant evaluation and constraints follow the same rule.
    let c = run_body(
        "const U : uint<8> = 3\nconst BAD = U < -1",
        "        log(info, \"${BAD}\")",
    );
    assert!(lower_err(&c).contains("mixes signed and unsigned operands"));
    let k = run_body(
        "transaction M\n    u : uint<8>\n    keep u > -1\nend transaction M",
        "        let t : M\n        randomize(t)",
    );
    assert!(lower_err(&k).contains("mixes signed and unsigned operands"));
}

#[test]
fn mixed_sign_conditional_arms_are_rejected() {
    let src = run_body(
        "",
        "        let a : uint<8> = 3\n        let s : sint<8> = 0 - 1\n        let f : bool = true\n        let x = f ? a : s\n        log(info, \"${x}\")",
    );
    let err = lower_err(&src);
    assert!(err.contains("arms of a conditional"), "{err}");
}

#[test]
fn bare_literal_let_needs_a_type() {
    let src = run_body("", "        let n = 5\n        log(info, \"${n}\")");
    let err = lower_err(&src);
    assert!(err.contains("untyped integer literal"), "{err}");
    lower_and_verify(&run_body(
        "",
        "        let n : uint<32> = 5\n        let s = 4'd3\n        log(info, \"${n} ${s}\")",
    ));
    // A discard is no exception (owner decision).
    let err = lower_err(&run_body("", "        let _ = 1"));
    assert!(err.contains("untyped integer literal"), "{err}");
    lower_and_verify(&run_body("", "        let _ : uint<8> = 1"));
}

#[test]
fn untyped_constant_contributes_its_value_bits() {
    let src = run_body(
        "const K = 3",
        "        let a : uint<8> = 200\n        let r : uint<9> = a + K\n        log(info, \"${r}\")",
    );
    lower_and_verify(&src);
}

#[test]
fn typed_const_narrowing_applies_without_exception() {
    let src = run_body(
        "const A16 : uint<16> = 3\nconst X : uint<8> = A16 + 1",
        "        log(info, \"${X}\")",
    );
    assert!(lower_err(&src).contains("17-bit value"));
}

#[test]
fn constraint_mixes_through_vec_elements_and_foreach_variables_are_rejected() {
    let keep = run_body(
        "transaction M\n    s : sint<8>\n    v : Vec<uint<8>, 2>\n    keep v[0] > s\nend transaction M",
        "        let t : M\n        randomize(t)",
    );
    assert!(lower_err(&keep).contains("mixes signed and unsigned operands"));
    let with = run_body(
        "transaction M\n    v : Vec<uint<8>, 2>\nend transaction M",
        "        let t : M\n        let ls : sint<8> = 0\n        blocking randomize(t) with\n            t.v[0] > ls\n        end randomize",
    );
    assert!(lower_err(&with).contains("mixes signed and unsigned operands"));
}

#[test]
fn tier_crossing_untyped_let_infers_the_wide_type() {
    let src = run_body(
        "",
        "        let p : uint<40> = 5\n        let q : uint<40> = 7\n        let pq = p * q\n        let pq40 : uint<40> = pq\n        log(info, \"${pq40}\")",
    );
    assert!(lower_err(&src).contains("80-bit value"));
}

// ------------------------------------------- exact constant evaluation

/// The IR text of a test that logs constant `name`.
fn const_log_text(decls: &str, name: &str) -> String {
    let prog = lower_and_verify(&run_body(
        decls,
        &format!("        log(info, \"${{{name}}}\")"),
    ));
    format!("{prog}")
}

#[test]
fn constants_fold_exactly_at_their_arch_width() {
    // uint<64>(0) - uint<64>(15) is the 65-bit 2**65 - 15: mod 127 that is
    // 116, where a 64-bit carrier gave 114. Runtime computes the same.
    let text = const_log_text(
        "const A : uint<64> = 0\nconst B : uint<64> = 15\nconst C = (A - B) % 127",
        "C",
    );
    assert!(text.contains("116") && !text.contains("114"), "{text}");
    // An unsigned difference narrower than 64 bits wraps at its result
    // width, as at runtime: uint<8>(1) - uint<8>(255) is the 9-bit 258.
    let text = const_log_text(
        "const S : uint<8> = 1\nconst T : uint<8> = 255\nconst D = S - T",
        "D",
    );
    assert!(text.contains("258"), "{text}");
}

#[test]
fn constants_keep_intermediates_wider_than_128_bits() {
    // (2**64 - 1)**3 is a 192-bit intermediate; mod 1000003 it is 218658.
    let text = const_log_text(
        "const U : uint<64> = 0xFFFFFFFFFFFFFFFF\nconst C = (U * U * U) % 1000003",
        "C",
    );
    assert!(text.contains("218658"), "{text}");
    // Signed: (-2**63)**3 % 1000003 is -396961.
    let text = const_log_text(
        "const M : sint<64> = -9223372036854775807 - 1\nconst C = (M * M * M) % 1000003",
        "C",
    );
    // (The IR prints a signed literal as its 64-bit pattern.)
    assert!(text.contains(&((-396961i64) as u64).to_string()), "{text}");
    // A 200-bit typed constant built by a shift, and a wide product that
    // stays a whole constant value (substituted as its 128-bit pattern).
    lower_and_verify(&run_body(
        "const W : uint<200> = 0x80000000000000000000000000000000000000000000000000\nconst U : uint<64> = 0xFFFFFFFFFFFFFFFF\nconst SQ = U * U",
        "        let w : uint<200> = W\n        let sq : uint<128> = SQ\n        let top : uint<1> = w[199:199]\n        let hi : uint<64> = sq[127:64]\n        log(info, \"${top} ${hi}\")",
    ));
}

#[test]
fn constants_reject_only_values_that_do_not_fit_their_declared_type() {
    // An exact value outside the declared type is an error at every width.
    // Values wider than the 64-bit carrier are checked exactly; a literal
    // within it keeps the declaration policy literals always had.
    for (decl, what) in [
        ("const X : uint<8> = 256", "does not fit `uint<8>`"),
        (
            "const X : uint<100> = 0x10000000000000000000000000",
            "does not fit `uint<100>`",
        ),
        (
            "const X : sint<70> = 0x200000000000000000",
            "does not fit `sint<70>`",
        ),
        (
            "const U : uint<64> = 0xFFFFFFFFFFFFFFFF\nconst X : uint<100> = 0 - U * U",
            "does not fit `uint<100>`",
        ),
    ] {
        let err = lower_err(&run_body(decl, "        log(info, \"${X}\")"));
        assert!(err.contains(what), "{decl}: {err}");
    }
    // Evaluation errors are still errors.
    let err = lower_err(&run_body("const X = 1 / 0", "        log(info, \"${X}\")"));
    assert!(err.contains("division by zero"), "{err}");
    // A typed initializer whose ARCH width exceeds the declaration is the
    // D2 narrowing error; an explicit cast states the intent.
    let err = lower_err(&run_body(
        "const A : uint<64> = 1\nconst X : uint<64> = A + 1",
        "        log(info, \"${X}\")",
    ));
    assert!(err.contains("65-bit value"), "{err}");
    lower_and_verify(&run_body(
        "const A : uint<64> = 1\nconst X : uint<64> = (A + 1) as uint<64>\nconst Y : uint<100> = 0xFFFFFFFFFFFFFFFFFFFF",
        "        log(info, \"${X} ${Y}\")",
    ));
}

// ------------------------------------------------------------ strict fit

#[test]
fn untyped_literals_must_fit_their_declared_type_in_const_and_runtime_alike() {
    // The same value is rejected as a `const` and as a runtime store; no
    // 64-bit pattern is reinterpreted anywhere.
    for (ty, value) in [
        ("uint<64>", "~0"),
        ("uint<65>", "~0"),
        ("uint<32>", "~0"),
        ("uint<8>", "-1"),
        ("uint<8>", "256"),
        ("sint<8>", "0xFF"),
        ("sint<63>", "0xFFFFFFFFFFFFFFFF"),
    ] {
        let err = lower_err(&run_body(
            &format!("const C : {ty} = {value}"),
            "        log(info, \"${C}\")",
        ));
        assert!(err.contains("does not fit"), "const {ty} = {value}: {err}");
        let err = lower_err(&run_body(
            "",
            &format!("        let x : {ty} = {value}\n        log(info, \"${{x}}\")"),
        ));
        assert!(err.contains("does not fit"), "let {ty} = {value}: {err}");
    }
    // The explicit forms are accepted, and const and runtime agree on them.
    lower_and_verify(&run_body(
        "const C65 : uint<65> = (~0) as uint<65>\nconst ONES : uint<64> = 0xFFFF_FFFF_FFFF_FFFF\nconst M : sint<63> = -1",
        "        let u65 : uint<65> = (~0) as uint<65>\n        let ones : uint<64> = 0xFFFF_FFFF_FFFF_FFFF\n        let m : sint<63> = -1\n        assert u65 == C65 else fail(\"c65\")\n        assert ones == ONES else fail(\"ones\")\n        assert m == M else fail(\"m\")",
    ));
}

#[test]
fn literal_negativity_is_judged_from_its_exact_value() {
    // `~0` is -1, so against a uint it is the D1 mix, as `-1` is.
    for (decls, body) in [
        (
            "",
            "        let a : uint<8> = 1\n        let b = a < ~0\n        log(info, \"${b}\")",
        ),
        (
            "const K = ~0",
            "        let a : uint<8> = 1\n        let b = a < K\n        log(info, \"${b}\")",
        ),
        (
            "const K = ~(0 + 0)\nconst U : uint<8> = 3\nconst Z = U < K",
            "        log(info, \"${Z}\")",
        ),
        (
            "transaction P\n    u : uint<8>\n    keep u > ~0\nend transaction P",
            "        let p : P\n        randomize(p)",
        ),
    ] {
        let err = lower_err(&run_body(decls, body));
        assert!(
            err.contains("mixes signed and unsigned"),
            "{decls} / {body}: {err}"
        );
    }
}

#[test]
fn an_untyped_literal_adapts_to_its_peer_only_when_it_fits() {
    // A literal >= 2**63 against a sint<64> cannot be represented by it.
    for (decls, body) in [
        ("", "        let s : sint<64> = 1\n        let b = s < 0x8000000000000000\n        log(info, \"${b}\")"),
        ("", "        let s : sint<64> = 1\n        let b = s == 0xFFFFFFFFFFFFFFFF\n        log(info, \"${b}\")"),
        ("const BIG = 0x8000000000000000", "        let s : sint<64> = 1\n        let b = s < BIG\n        log(info, \"${b}\")"),
        ("", "        let a : uint<8> = 1\n        let b = a == 300\n        log(info, \"${b}\")"),
        ("", "        let a : uint<8> = 1\n        let b = a | 300\n        log(info, \"${b}\")"),
        ("const S : sint<64> = 1\nconst Z = S < 0x8000000000000000", "        log(info, \"${Z}\")"),
        (
            "transaction P\n    s : sint<64>\n    keep s < 0x8000000000000000\nend transaction P",
            "        let p : P\n        randomize(p)",
        ),
        // Membership members and range bounds are compared with the field.
        (
            "transaction P\n    u : uint<8>\n    keep u in {1, 300}\nend transaction P",
            "        let p : P\n        randomize(p)",
        ),
        (
            "transaction P\n    u : uint<8>\n    keep u in [0..300]\nend transaction P",
            "        let p : P\n        randomize(p)",
        ),
    ] {
        let err = lower_err(&run_body(decls, body));
        assert!(err.contains("does not fit its operand's type"), "{decls} / {body}: {err}");
    }
    // `+ - *` widen per ARCH: a literal contributes its own value bits and
    // need only match the peer's signedness, at any width.
    let prog = lower_and_verify(&run_body(
        "",
        "        let a : uint<8> = 1\n        let b : uint<10> = a + 300\n        let s : sint<64> = 1\n        let c : sint<65> = s + 0x8000000000000000\n        let u : uint<64> = 2\n        let d : uint<129> = u * 0x10000000000000000\n        let e = s < 0x7FFFFFFFFFFFFFFF\n        log(info, \"${b} ${c} ${d} ${e}\")",
    ));
    let text = format!("{prog}");
    assert!(
        text.contains("c : sint<65>") && text.contains("d : uint<129>"),
        "{text}"
    );
}

#[test]
fn literal_only_shifts_are_exact_in_const_and_runtime() {
    // 3 << 63 is 3 * 2**63: it does not fit uint<64> (const or runtime),
    // and (0 - 3) << 62 stays negative and does not fit sint<64>.
    for (ty, value) in [("uint<64>", "3 << 63"), ("sint<64>", "(0 - 3) << 62")] {
        let err = lower_err(&run_body(
            &format!("const C : {ty} = {value}"),
            "        log(info, \"${C}\")",
        ));
        assert!(err.contains("does not fit"), "const {ty} = {value}: {err}");
        let err = lower_err(&run_body(
            "",
            &format!("        let x : {ty} = {value}\n        log(info, \"${{x}}\")"),
        ));
        assert!(err.contains("does not fit"), "let {ty} = {value}: {err}");
    }
    // Wide enough, they fold to the same exact value in both contexts: the
    // runtime shift is folded at lowering, never computed in a 64-bit
    // carrier or sign-extended from one.
    let prog = lower_and_verify(&run_body(
        "const SH : uint<128> = 3 << 63\nconst SH2 : uint<128> = 0xFFFFFFFFFFFFFFFF << 1\nconst NS : sint<128> = (0 - 3) << 62",
        "        let sh : uint<128> = 3 << 63\n        let sh2 : uint<128> = 0xFFFFFFFFFFFFFFFF << 1\n        let ns : sint<128> = (0 - 3) << 62\n        assert sh == SH else fail(\"sh\")\n        assert sh2 == SH2 else fail(\"sh2\")\n        assert ns == NS else fail(\"ns\")",
    ));
    let text = format!("{prog}");
    assert!(!text.contains("<<"), "literal-only shifts fold: {text}");
}

#[test]
fn wide_untyped_literals_into_signed_slots_are_judged_by_strict_fit() {
    // Fitting values are accepted, const and runtime alike.
    lower_and_verify(&run_body(
        "const C = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF\nconst K : sint<129> = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
        "        let a : sint<200> = 0x10000000000000000\n        let b : sint<129> = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF\n        let c : sint<129> = -0x100000000000000000000000000000000\n        let d : sint<129> = C\n        assert d == K else fail(\"d\")\n        log(info, \"${a} ${b} ${c}\")",
    ));
    // A value past the signed range is the single strict-fit error.
    let err = lower_err(&run_body(
        "",
        "        let b : sint<128> = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF\n        log(info, \"${b}\")",
    ));
    assert!(err.contains("does not fit `b`"), "{err}");
}

#[test]
fn a_negative_literal_into_a_narrow_signed_owner_field_verifies() {
    for (owner, write) in [(
        "scoreboard SB\n    n : sint<4> default 0\nend scoreboard SB",
        "sb.n = -8",
    )] {
        let src = format!(
            "{owner}\ntestbench TB\n    dut : Top\n    sb : SB\nend testbench TB\nimpl P for TB\n    run\n        {write}\n    end run\nend impl P\n"
        );
        lower_and_verify(&src);
        let bad = src.replace("= -8", "= -9");
        let err = lower_err(&bad);
        assert!(err.contains("does not fit"), "{err}");
    }
}

#[test]
fn an_untyped_let_of_a_literal_only_expression_needs_a_type() {
    for (decls, init) in [
        ("", "1 + 299"),
        ("", "~0"),
        ("", "1 << 3"),
        ("const K = 5", "K"),
        ("", "true ? 11 : 22"),
    ] {
        let err = lower_err(&run_body(
            decls,
            &format!("        let x = {init}\n        log(info, \"${{x}}\")"),
        ));
        assert!(err.contains("literal-only expression"), "{init}: {err}");
    }
    // A sized literal declares its width.
    lower_and_verify(&run_body(
        "",
        "        let s = 4'd3\n        log(info, \"${s}\")",
    ));
}

#[test]
fn a_literal_tree_that_cannot_be_evaluated_is_an_error_wherever_it_is_judged() {
    for (decls, body, what) in [
        (
            "",
            "        let x : uint<8> = 1 << 2000",
            "shift amount 2000 is out of range",
        ),
        ("", "        let x : uint<8> = 1 / 0", "division by zero"),
        ("", "        let x : uint<16> = 300 % 0", "modulo by zero"),
        (
            "",
            "        let a : uint<8> = 1\n        let b = a < (1 << 2000)",
            "shift amount 2000",
        ),
        (
            "",
            "        let a : uint<8> = 1\n        let b = a + (1 / 0)",
            "division by zero",
        ),
        (
            "transaction P\n    u : uint<8>\n    keep u < (1 << 2000)\nend transaction P",
            "        let p : P\n        randomize(p)",
            "shift amount 2000",
        ),
        (
            "transaction P\n    u : uint<8>\n    keep u in {1, 1 / 0}\nend transaction P",
            "        let p : P\n        randomize(p)",
            "division by zero",
        ),
    ] {
        let err = lower_err(&run_body(decls, &format!("{body}\n        wait 1 cycle")));
        assert!(err.contains(what), "{body}: {err}");
    }
    // Owner-field writes and queue pushes too.
    for write in ["sb.n = 1 / 0", "sb.q.push(1 << 2000)"] {
        let src = format!(
            "scoreboard SB\n    n : sint<4> default 0\n    q : queue<uint<8>>\nend scoreboard SB\ntestbench TB\n    dut : Top\n    sb : SB\nend testbench TB\nimpl P for TB\n    run\n        {write}\n    end run\nend impl P\n"
        );
        let err = lower_err(&src);
        assert!(
            err.contains("division by zero") || err.contains("shift amount 2000"),
            "{write}: {err}"
        );
    }
}

#[test]
fn an_untyped_literal_shifted_by_a_non_constant_amount_needs_a_cast() {
    for body in [
        "        let ten : uint<64> = 10\n        let y : uint<8> = 5 << ten",
        "        let u : uint<64> = 0\n        let y : uint<16> = 300 >> u",
    ] {
        let err = lower_err(&run_body(
            "",
            &format!("{body}\n        log(info, \"${{y}}\")"),
        ));
        assert!(
            err.contains("non-constant amount") && err.contains(" as uint<16>)"),
            "{err}"
        );
    }
    let err = lower_err(&run_body(
        "transaction P\n    u : uint<8>\n    n : uint<3>\n    keep u == (1 << n)\nend transaction P",
        "        let p : P\n        randomize(p)",
    ));
    assert!(err.contains("non-constant amount"), "{err}");
    // The cast form, and a constant amount, are accepted.
    lower_and_verify(&run_body(
        "const T : uint<8> = 3",
        "        let ten : uint<64> = 10\n        let a : uint<16> = (5 as uint<16>) << ten\n        let b : uint<16> = 5 << T\n        let c : uint<16> = 5 << 3\n        log(info, \"${a} ${b} ${c}\")",
    ));
}

#[test]
fn a_negative_literal_misfit_is_described_as_signed() {
    let err = lower_err(&run_body(
        "",
        "        let x : uint<8> = -1\n        log(info, \"${x}\")",
    ));
    assert!(err.contains("(a signed 1-bit value)"), "{err}");
}

#[test]
fn record_and_transaction_field_defaults_are_judged_by_strict_fit() {
    for kind in ["struct", "transaction"] {
        for (value, what) in [
            ("300", "it is a 9-bit value"),
            ("-1", "it is a signed 1-bit value"),
        ] {
            let src = format!(
                "{kind} R\n    h : uint<8> default {value}\nend {kind} R\ntest P\n    let dut : Top\n    run\n        let r : R\n        log(info, \"${{r.h}}\")\n    end run\nend test P\n"
            );
            let err = lower_err(&src);
            assert!(
                err.contains(what) && err.contains("does not fit `uint<8>`"),
                "{kind} default {value}: {err}"
            );
        }
        let ok = format!(
            "{kind} R\n    h : uint<8> default 255\n    s : sint<4> default -8\nend {kind} R\ntest P\n    let dut : Top\n    run\n        let r : R\n        log(info, \"${{r.h}} ${{r.s}}\")\n    end run\nend test P\n"
        );
        lower_and_verify(&ok);
    }
}

#[test]
fn covergroup_bin_values_must_fit_a_typed_coverpoint() {
    use harc::codegen::{cpp_tb, tbir};
    let src = |bins: &str| {
        format!(
            "transaction Cmd\n    ticks : uint<8> default 1\nend transaction Cmd\ncovergroup G @(drv.go(cmd) post)\n    cp : cover cmd.ticks\n        bins\n{bins}\n        end bins\nend covergroup G\ntransactor D\n    dut : Top\n    when active\n        hookable go(cmd: Cmd)\n            wait 1 cycle\n        end go\n    end when\nend transactor D\ntestbench Tb\n    dut : Top\n    drv : D active\n    cov : G\nend testbench Tb\nimpl T for Tb\n    run\n        drv.dut = dut\n        let c : Cmd\n        drv.go(c)\n    end run\nend impl T\n"
        )
    };
    let emit = |s: &str| {
        let parsed = parse_source(s).expect("parses");
        let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
        let prog = lower::lower_program(&merged).expect("lowers");
        tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default())
    };
    for bins in ["            pair = [2..300]", "            neg = {-1}"] {
        let err = emit(&src(bins)).expect_err("a bin value outside uint<8> is rejected");
        assert!(
            err.0
                .contains("does not fit the coverpoint's type `uint<8>`"),
            "{bins}: {}",
            err.0
        );
    }
    emit(&src("            low = [0..3]\n            top = {255}")).expect("fitting bins emit");
}
