//! TB-IR pipeline tests: AST â†’ IR lowering snapshots, verifier checks,
//! `LowerError::Unsupported` stubs, and tbir C++ emission smoke tests.
//! The end-to-end v1-vs-tbir trace-equivalence gate runs out-of-band
//! (`harc sim --codegen {v1,tbir}` + `harc trace-diff`); these tests
//! lock the in-process shapes.

use harc::codegen::{cpp_tb, merge, tbir};
use harc::ir::passes::{lower_coroutine, placement};
use harc::ir::{self, lower, verify};
use harc::parser::parse_source;
use std::path::Path;

fn fixture(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn lower_src(src: &str) -> Result<ir::TbProgram, lower::LowerError> {
    let parsed = parse_source(src).expect("fixture parses");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    lower::lower_program(&merged)
}

/// Merged `SourceFile` for one source string (the input `tbir::emit`
/// needs for the constraint-IR / randomize seam â€” empty otherwise).
fn merged_src(src: &str) -> harc::ast::SourceFile {
    let parsed = parse_source(src).expect("fixture parses");
    merge::merge_for_sim(vec![parsed], None).expect("merge")
}

/// Multi-file variant for fixtures that split helpers across files
/// (mirrors how run_fixtures.sh loads them).
fn lower_fixtures(names: &[&str]) -> Result<ir::TbProgram, lower::LowerError> {
    let parsed: Vec<_> = names
        .iter()
        .map(|n| parse_source(&fixture(n)).unwrap_or_else(|e| panic!("{n} parses: {e:?}")))
        .collect();
    let merged = merge::merge_for_sim(parsed, None).expect("merge");
    lower::lower_program(&merged)
}

/// Lower one fixture that `use`s a stdlib bus (`use BusAxiLite`),
/// providing the bus decl by parsing `stdlib/<Bus>.arch` alongside it â€”
/// mirroring the CLI's `resolve_use_imports` (which `lower_src` /
/// `lower_fixtures` do not perform). Only the parsed bus items survive
/// `merge_for_sim`, like the CLI path.
fn lower_with_stdlib_bus(
    fixture_name: &str,
    bus_file: &str,
) -> Result<ir::TbProgram, lower::LowerError> {
    let fix = parse_source(&fixture(fixture_name)).expect("fixture parses");
    let bus_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("stdlib")
        .join(bus_file);
    let bus_src = std::fs::read_to_string(&bus_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", bus_path.display()));
    let bus = parse_source(&bus_src).expect("stdlib bus parses");
    let merged = merge::merge_for_sim(vec![fix, bus], None).expect("merge");
    lower::lower_program(&merged)
}

/// `merge_for_sim` of one source string plus `stdlib/<Bus>.arch` â€” the
/// source-string analogue of `lower_with_stdlib_bus`, for probes built
/// by editing a bus-using fixture.
fn merged_with_stdlib_bus(src: &str, bus_file: &str) -> harc::ast::SourceFile {
    let fix = parse_source(src).expect("source parses");
    let bus_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("stdlib")
        .join(bus_file);
    let bus_src = std::fs::read_to_string(&bus_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", bus_path.display()));
    let bus = parse_source(&bus_src).expect("stdlib bus parses");
    merge::merge_for_sim(vec![fix, bus], None).expect("merge")
}

/// Lower + verify + emit one registry fixture through the tbir backend
/// with default options (the `--sv` Verilator path the equivalence
/// harness exercises).
fn emit_fixture_cpp(name: &str) -> String {
    let merged = merged_src(&fixture(name));
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits")
}

/// Lower + verify + emit one inline source string through the tbir
/// backend (the in-process analogue of `emit_fixture_cpp`, for focused
/// codegen-shape assertions that do not warrant a registry fixture).
fn emit_cpp_src(src: &str) -> String {
    let merged = merged_src(src);
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits")
}

fn emit_cpp_src_result(src: &str) -> Result<String, String> {
    let merged = merged_src(src);
    let prog = lower::lower_program(&merged).map_err(|e| e.to_string())?;
    verify::verify_program(&prog).map_err(|e| format!("{e:?}"))?;
    tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).map_err(|e| e.to_string())
}

/// The negative-test contract: every out-of-subset fixture must produce
/// `LowerError::Unsupported` whose rendered message names the offending
/// construct and points the user at `--codegen v1`.
fn assert_unsupported(err: &lower::LowerError) -> String {
    assert!(
        matches!(err, lower::LowerError::Unsupported { .. }),
        "must be LowerError::Unsupported: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("--codegen v1"),
        "unsupported error must suggest --codegen v1: {msg}"
    );
    msg
}

/// A construct no backend implements. The diagnostic must NOT suggest
/// `--codegen v1` â€” that suggestion is only honest when v1 actually
/// implements the construct, and sending a user to a backend that
/// rejects (or silently mis-lowers) it is worse than saying nothing.
fn assert_not_implemented(err: &lower::LowerError, v1: lower::V1Status) -> String {
    match err {
        lower::LowerError::NotImplemented { v1: got, .. } => {
            assert_eq!(*got, v1, "wrong V1Status on: {err:?}")
        }
        other => panic!("must be LowerError::NotImplemented: {other:?}"),
    }
    let msg = err.to_string();
    assert!(
        !msg.contains("re-run with `--codegen v1`"),
        "a not-implemented error must not send the user to v1: {msg}"
    );
    msg
}

/// #521 diagnostics contract: an illegal constant evaluation (division
/// by zero, width violation, cyclic reference, ...) is a program error
/// under every backend, so it surfaces as `LowerError::Invalid` with a
/// precise message â€” and must NOT point the user at `--codegen v1`.
fn assert_invalid(err: &lower::LowerError) -> String {
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "must be LowerError::Invalid: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        !msg.contains("--codegen v1"),
        "invalid-program error must not suggest --codegen v1: {msg}"
    );
    msg
}

/// A comparable projection of a covergroup bin value. `CovBinValue` no
/// longer derives `PartialEq` (a runtime range bound carries an `Expr`,
/// which is not comparable), so constant-bin tests compare this folded
/// form: `Eq(x)` â†’ `("eq", Some(x), None)`, a constant `Range` â†’ its two
/// optional bounds, and any runtime bound â†’ `u64::MAX` sentinel so a
/// stray non-constant fold is visible rather than silently dropped.
fn bin_repr(v: &ir::CovBinValue) -> (&'static str, Option<u64>, Option<u64>) {
    fn bound(b: &ir::CovBinBound) -> Option<u64> {
        match b {
            ir::CovBinBound::Const(x) => Some(*x),
            ir::CovBinBound::Runtime(_) => Some(u64::MAX),
        }
    }
    match v {
        ir::CovBinValue::Eq(x) => ("eq", bound(x), None),
        ir::CovBinValue::Range { lo, hi } => (
            "range",
            lo.as_ref().and_then(bound),
            hi.as_ref().and_then(bound),
        ),
    }
}

/// Project a point's bins to `(name, [(kind, lo, hi)])` for equality
/// assertions against constant-bin expectations.
fn bins_repr(
    p: &ir::CoverPointSchema,
) -> Vec<(&str, Vec<(&'static str, Option<u64>, Option<u64>)>)> {
    p.bins
        .iter()
        .map(|b| (b.name.as_str(), b.values.iter().map(bin_repr).collect()))
        .collect()
}

/// harc#473: `+% / -% / *%` mask the result to `max(W(lhs), W(rhs))` bits.
/// A typed `a : uint<8>` operand plus a literal masks at 8 b, so the emitted
/// C++ carries the `& 0xFF` residue rather than the un-wrapped `a + 1`.
/// (A typed `let : uint<8>` assignment does NOT itself mask â€” the residue is
/// proof the wrap was applied.)
#[test]
fn wrapping_ops_mask_to_operand_width() {
    let cpp = emit_cpp_src(
        r#"test T
    let dut : Top
    run
        let a : uint<8> = 255
        let s : uint<8> = a +% 1
        let p : uint<8> = a *% 3
        assert s == 0 else fail("x")
        assert p == 253 else fail("x")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("((a + 1)) & 0xFFULL"),
        "expected 8-bit add-wrap mask `(a + 1) & 0xFF`; got:\n{cpp}"
    );
    assert!(
        cpp.contains("((a * 3)) & 0xFFULL"),
        "expected 8-bit mul-wrap mask `(a * 3) & 0xFF`; got:\n{cpp}"
    );
}

/// harc#473: the wrap width is `max(W(lhs), W(rhs))` â€” a literal is
/// self-sized (minimum width) and does not widen the result, so `a +% 300`
/// with `a : uint<8>` masks at 9 b (300 needs 9 bits), not 8.
#[test]
fn wrapping_op_width_is_max_of_operands() {
    let cpp = emit_cpp_src(
        r#"test T
    let dut : Top
    run
        let a : uint<8> = 255
        let w : uint<9> = a +% 300
        assert w == 43 else fail("x")
    end run
end test T"#,
    );
    // 9-bit mask is 0x1FF; an 8-bit mask (0xFF) would be wrong here.
    assert!(
        cpp.contains("& 0x1FFULL"),
        "expected a 9-bit wrap mask (max(8,9)) `& 0x1FF`; got:\n{cpp}"
    );
}

/// harc#473: a wrapping op whose operand widths cannot be determined is a
/// hard lowering error â€” never a silent no-wrap (an un-masked scoreboard
/// value the DUT can never emit is worse than a loud failure). `dut.count_out`
/// read into an untyped local is width-erased.
#[test]
fn wrapping_op_unknown_operand_width_is_rejected() {
    let err = lower_src(
        r#"test T
    let dut : Top
    run
        let x = dut.count_out
        let y : uint<8> = x +% x
        assert y == 0 else fail("x")
    end run
end test T"#,
    )
    .expect_err("unknown-width wrapping operand must fail lowering");
    let msg = err.to_string();
    assert!(
        msg.contains("statically known bit-width"),
        "expected a known-width lowering error for `x +% x`; got: {msg}"
    );
}

/// #494 P2a: file-scope `const` initializers that are constant
/// EXPRESSIONS (not just plain integer literals) fold to a `u64` at
/// lowering and substitute as that literal at use sites â€” the same value
/// v1's C++ constexpr would compute. Earlier consts are visible to later
/// ones (`SEEN_BOTH` references `SEEN_FIRST`/`SEEN_SECOND`).
#[test]
fn const_expression_initializers_fold() {
    let cpp = emit_cpp_src(
        r#"const SEEN_FIRST  : uint<64> = 1 << 0
const SEEN_SECOND : uint<64> = 1 << 1
const SEEN_BOTH   : uint<64> = SEEN_FIRST | SEEN_SECOND
const WIDTH_MASK  : uint<32> = (1 << 8) - 1

test T
    let dut : Top
    run
        assert SEEN_FIRST == 1 else fail("x")
        assert SEEN_SECOND == 2 else fail("x")
        assert SEEN_BOTH == 3 else fail("x")
        assert WIDTH_MASK == 255 else fail("x")
    end run
end test T"#,
    );
    // Each const use substitutes as its folded literal, so the assert
    // condition reduces to a literal-vs-literal comparison.
    for lit in [
        "((uint64_t)(1)) == 1",
        "((uint64_t)(2)) == 2",
        "((uint64_t)(3)) == 3",
        "((uint64_t)(255)) == 255",
    ] {
        assert!(
            cpp.contains(lit),
            "expected folded const comparison `{lit}`; got:\n{cpp}"
        );
    }
}

/// #494 P2a / #521: a `const` initializer referencing an undefined name
/// is rejected with a precise `Invalid` diagnostic naming both the
/// const and the unknown reference â€” never silently accepted.
#[test]
fn const_unknown_reference_is_rejected() {
    let err = lower_src(
        r#"const BAD : uint<32> = NOT_A_CONST + 1

test T
    let dut : Top
    run
        assert BAD == 0 else fail("x")
    end run
end test T"#,
    )
    .expect_err("non-constant const initializer must fail lowering");
    let msg = assert_invalid(&err);
    assert!(
        msg.contains("const BAD") && msg.contains("NOT_A_CONST"),
        "rejection must name the offending const and the unknown name; got: {msg}"
    );
}

/// #521 / #550: the wrapping `+% -% *%` operators DO fold in a `const`
/// initializer, at `max(W(lhs), W(rhs))` per spec Â§2.4, whenever both
/// operand widths are statically known â€” a literal is self-sized, an
/// `as uint<W>` cast carries W. v1 has always emitted the mask into the
/// `constexpr` initializer; TB-IR used to reject the form outright, so
/// the same source compiled under one backend and not the other.
#[test]
fn const_wrap_operator_folds_at_the_operand_width() {
    let prog = lower_src(
        r#"const K : uint<8> = 255 +% 1
const M : uint<8> = (200 as uint<8>) *% 3

test T
    let dut : Top
    run
        assert K == 0 else fail("k")
        assert M == 88 else fail("m")
    end run
end test T"#,
    )
    .expect("literal-width const wraps fold");
    let text = format!("{prog}");
    assert!(
        text.contains("(0 == 0)") && text.contains("(88 == 88)"),
        "const wraps must fold to their masked value; got:\n{text}"
    );
}

/// An operand whose width is not statically known still cannot be folded
/// â€” the `const` table carries values, not declared types, so a
/// reference to another `const` has no width. v1 rejects the same shape,
/// so the two backends accept and reject the same set.
#[test]
fn const_wrap_operator_needs_known_operand_widths() {
    let err = lower_src(
        r#"const W : uint<8> = 255
const BAD : uint<8> = W +% 1

test T
    let dut : Top
    run
        assert BAD == 0 else fail("x")
    end run
end test T"#,
    )
    .expect_err("unknown-width const wrap must fail lowering");
    let msg = assert_invalid(&err);
    assert!(
        msg.contains("statically known bit-width"),
        "must name the unknown-width operand; got: {msg}"
    );
}

/// Adversarial-review finding (#524 follow-up): const and enum-variant
/// substitution emits widthless `UInt(None)`/`SInt(None)` literals, and
/// assigning one into an explicitly width-typed local must verify â€” this
/// exact shape (`let x : uint<8> = K`) hit an internal verifier error
/// ("assigns UInt(None) into local declared UInt(Some(8))") because the
/// wildcard exemption only covered `IrType::Unknown`.
#[test]
fn const_and_enum_assign_to_typed_local() {
    let cpp = emit_cpp_src(
        r#"const K : uint<32> = 5
const NEG : sint<8> = -1
enum Color { RED, GREEN, BLUE }

test T
    let dut : Top
    run
        let x : uint<8> = K
        let c : uint<2> = GREEN
        let s : sint<8> = NEG
        x = K
        assert x == 5 else fail("x")
        assert c == 1 else fail("c")
        assert s == NEG else fail("s")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("((uint64_t)(5))"),
        "const substitution into typed locals must emit; got:\n{cpp}"
    );
}

/// Adversarial-review finding (#524 follow-up): a literal mask bounds a
/// shift operand's width, so `(wide & 0xFF) >> 4` is a 64-bit-safe shift
/// even when `wide` is `uint<128>` â€” the guard must not reject it (it
/// did, by taking the max over `&` operands). The unmasked wide operand
/// must still reject, in BOTH directions â€” `<<` previously had no guard
/// and silently emitted a wrong 64-bit shift.
#[test]
fn wide_shift_guard_is_mask_aware_and_symmetric() {
    let masked = r#"const M : uint<8> = 0xF0

test T
    let dut : Top
    run
        let wide : uint<128> = 240
        let x : uint<8> = (wide & 0xFF) >> 4
        assert x == 15 else fail("x=${x}")
    end run
end test T"#;
    let cpp = emit_cpp_src(masked);
    assert!(
        cpp.contains(">> 4"),
        "masked wide operand must emit a plain 64-bit shift; got:\n{cpp}"
    );

    for (expr, dir) in [("wide >> 1", "right"), ("wide << 1", "left")] {
        let src = format!(
            r#"test T
    let dut : Top
    run
        let wide : uint<128> = 1
        let x : uint<64> = {expr}
        assert x == 2 else fail("x")
    end run
end test T"#
        );
        let merged = merged_src(&src);
        let prog = lower::lower_program(&merged).expect("lowers");
        verify::verify_program(&prog).expect("verifies");
        let err = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default())
            .expect_err("unmasked wide shift must fail emission");
        let msg = format!("{err:?}");
        assert!(
            msg.contains(&format!("{dir} shift above 64 bits")),
            "wide `{expr}` must reject with a {dir}-shift emit error; got: {msg}"
        );
    }
}

/// Adversarial-review finding (#524 follow-up): a `sint<W>` initializer's
/// bit pattern is reinterpreted as signed two's complement before the
/// range check â€” the mod-2^64 conversion C++20 defines and v1 performs.
/// `sint<63> = ~0` is -1 (v1 agrees), consistent with `sint<64>`, which
/// already accepted the same pattern.
#[test]
fn const_sint_reinterprets_high_bit_patterns() {
    let cpp = emit_cpp_src(
        r#"const ALL63 : sint<63> = 0xFFFFFFFFFFFFFFFF
const CASTNEG : sint<8> = (0 - 1) as uint<8>

test T
    let dut : Top
    run
        assert ALL63 == 0 - 1 else fail("a")
        assert CASTNEG == 0 - 1 else fail("c")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("((int64_t)(-1))"),
        "sint reinterpretation must store -1; got:\n{cpp}"
    );
    // Positive patterns still range-check: 255 has no negative reading.
    let err = lower_src(
        r#"const BAD : sint<8> = 0xFF

test T
    let dut : Top
    run
        assert BAD == 0 else fail("x")
    end run
end test T"#,
    )
    .expect_err("positive out-of-range sint must still reject");
    let msg = assert_invalid(&err);
    assert!(
        msg.contains("does not fit `sint<8>`"),
        "positive sint overflow must reject; got: {msg}"
    );
}

/// #521: a construct outside the constant-expression subset (here, a
/// call) still gets the structured `Unsupported` rejection that
/// suggests `--codegen v1`.
#[test]
fn const_out_of_subset_initializer_is_unsupported() {
    let err = lower_src(
        r#"const BAD : uint<32> = some_call()

test T
    let dut : Top
    run
        assert BAD == 0 else fail("x")
    end run
end test T"#,
    )
    .expect_err("non-constant const initializer must fail lowering");
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("const BAD"),
        "rejection must name the offending const `BAD`; got: {msg}"
    );
}

/// #521: signed constants fold with the declared signedness â€” `>>` on a
/// `sint` const is an arithmetic shift and `/` truncates toward zero,
/// matching v1's `int64_t` constexpr evaluation. The stored bit
/// pattern substitutes at use sites, so `-1 >> 1 == -1` (not
/// `0x7FFF...`), and `-7 / 2 == -3` (not a huge unsigned quotient).
#[test]
fn const_signed_semantics_fold() {
    let cpp = emit_cpp_src(
        r#"const NEG_ONE : sint<8>  = -1
const NEG_SHR : sint<8>  = NEG_ONE >> 1
const NEG_DIV : sint<32> = (0 - 7) / 2
const NEG_MOD : sint<32> = (0 - 7) % 2

test T
    let dut : Top
    run
        assert NEG_SHR == NEG_ONE else fail("x")
        assert NEG_DIV == 0 - 3 else fail("x")
        assert NEG_MOD == 0 - 1 else fail("x")
        assert (NEG_ONE >> 1) == NEG_ONE else fail("direct-shr")
        assert ((NEG_ONE + 0) >> 1) == NEG_ONE else fail("nested-shr")
        assert ((NEG_ONE as uint<8>) >> 1) == 9223372036854775807 else fail("cast-shr")
        assert (NEG_ONE.sext<64>() >> 1) == NEG_ONE else fail("sext-shr")
    end run
end test T"#,
    );
    // -1 (as a 64-bit pattern) survives the arithmetic shift.
    assert!(
        cpp.contains("(((int64_t)(-1)) == ((int64_t)(-1)))"),
        "sint consts must retain their signed value at use sites; got:\n{cpp}"
    );
    assert!(
        cpp.contains("((int64_t)(((int64_t)(-1)))) >> 1"),
        "signed const use sites must use arithmetic right shift; got:\n{cpp}"
    );
    assert!(
        cpp.contains("((uint64_t)(((uint64_t)(((int64_t)(-1)))))) >> 1"),
        "uint relabel casts must use logical right shift; got:\n{cpp}"
    );
    // The inner `(uint64_t)` narrows before the signed relabel (a
    // `HarcWide` receiver is otherwise an ambiguous conversion); it is
    // value-transparent here â€” `(int64_t)((uint64_t)(-1))` is `-1`.
    assert!(
        cpp.contains("((int64_t)(((int64_t)((uint64_t)(((int64_t)(-1))))))) >> 1"),
        "sext results must use arithmetic right shift; got:\n{cpp}"
    );
    assert!(
        cpp.contains("((int64_t)((((int64_t)(-1)) + 0))) >> 1"),
        "signed nested arithmetic must use arithmetic right shift; got:\n{cpp}"
    );
}

#[test]
fn const_sint64_min_emits_a_signed_literal() {
    let cpp = emit_cpp_src(
        r#"const MIN : sint<64> = -9223372036854775808

test T
    let dut : Top
    run
        assert MIN < 0 else fail("min")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("-9223372036854775807LL - 1"),
        "sint<64> minimum must be emitted without an unsigned literal; got:\n{cpp}"
    );
}

/// Unsigned constants must retain uint64_t rank at use sites. Otherwise
/// `sint`/`uint` mixed expressions lose C++'s usual-arithmetic conversion.
#[test]
fn const_mixed_signedness_use_preserves_usual_arithmetic_conversion() {
    let cpp = emit_cpp_src(
        r#"const NEG : sint<8> = -1
const ONE : uint<8> = 1

test T
    let dut : Top
    run
        assert !(NEG < ONE) else fail("mixed")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("((int64_t)(-1)) < ((uint64_t)(1))"),
        "mixed signed/unsigned consts must retain C++ conversion rank; got:\n{cpp}"
    );
}

/// #521: the folded value must fit the declared width â€” out-of-range
/// initializers are a precise compile-time error, not a silent
/// truncation (which would diverge from v1's 64-bit storage) and not a
/// silent wrong value (which would make the declared width a lie).
#[test]
fn const_width_violation_is_rejected() {
    for (src_ty, init, needle) in [
        ("uint<8>", "0x1FF", "does not fit `uint<8>`"),
        ("uint<32>", "~0", "does not fit `uint<32>`"),
        ("uint<32>", "0 - 1", "negative values cannot initialize"),
        ("sint<8>", "128", "does not fit `sint<8>`"),
        ("sint<8>", "0 - 129", "does not fit `sint<8>`"),
    ] {
        let err = lower_src(&format!(
            r#"const BAD : {src_ty} = {init}

test T
    let dut : Top
    run
        assert BAD == 0 else fail("x")
    end run
end test T"#
        ))
        .expect_err("width-violating const must fail lowering");
        let msg = assert_invalid(&err);
        assert!(
            msg.contains("const BAD") && msg.contains(needle),
            "width rejection for `{src_ty} = {init}` must contain `{needle}`; got: {msg}"
        );
    }
}

/// #521: integer literals evaluate in the 64-bit domain (HARC fixed-
/// width semantics), NOT as C++ 32-bit `int`. `1 << 31` is the bit-31
/// mask (0x80000000), and shift amounts 32..=63 and intermediate sums
/// above `INT_MAX` are well-defined. v1 mis-handles all three (C++20
/// sign-extends `1 << 31` to 0xFFFFFFFF80000000; the other two are
/// constexpr-UB compile errors in the emitted C++), so this corner is
/// locked here rather than in the v1-equivalence fixture.
#[test]
fn const_literals_are_64_bit() {
    let cpp = emit_cpp_src(
        r#"const BIT31 : uint<32> = 1 << 31
const BIT40 : uint<64> = 1 << 40
const BIG   : uint<64> = 2000000000 + 2000000000

test T
    let dut : Top
    run
        assert BIT31 == 0x80000000 else fail("x")
        assert BIT40 == 0x10000000000 else fail("x")
        assert BIG == 4000000000 else fail("x")
    end run
end test T"#,
    );
    for v in ["2147483648", "1099511627776", "4000000000"] {
        assert!(
            cpp.contains(v),
            "64-bit literal fold must produce {v}; got:\n{cpp}"
        );
    }
}

/// #521: boundary values on the declared width are accepted exactly.
#[test]
fn const_width_boundaries_fold() {
    let cpp = emit_cpp_src(
        r#"const U8_MAX  : uint<8>  = (1 << 8) - 1
const S8_MAX  : sint<8>  = 127
const S8_MIN  : sint<8>  = 0 - 128
const U63     : uint<63> = (1 << 63) - 1
const ALL_ONES : uint<64> = ~0

test T
    let dut : Top
    run
        assert U8_MAX == 255 else fail("x")
        assert S8_MAX == 127 else fail("x")
        assert S8_MIN + 128 == 0 else fail("x")
        assert U63 == 9223372036854775807 else fail("x")
        assert ALL_ONES == ~0 else fail("x")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("255") && cpp.contains("9223372036854775807"),
        "boundary consts must fold to their exact values; got:\n{cpp}"
    );
    // S8_MIN is a signed const â€” its use site must emit the signed
    // literal -128, not the raw 64-bit pattern.
    assert!(
        cpp.contains("((int64_t)(-128))"),
        "sint<8> minimum must substitute as -128; got:\n{cpp}"
    );
}

/// #521: illegal evaluations get precise diagnostics naming the const
/// and the failure: division/modulo by zero, out-of-range or negative
/// shift amounts, and self-referencing (cyclic) definitions.
#[test]
fn const_invalid_evaluations_are_rejected() {
    for (init, needle) in [
        ("1 / 0", "division by zero"),
        ("1 % 0", "modulo by zero"),
        ("1 / (BASE - BASE)", "division by zero"),
        ("1 << 64", "shift amount 64 is out of range"),
        ("1 >> 200", "shift amount 200 is out of range"),
        ("1 << (0 - 1)", "negative shift amount (-1)"),
        ("BAD + 1", "references itself"),
    ] {
        let err = lower_src(&format!(
            r#"const BASE : uint<32> = 4
const BAD : uint<32> = {init}

test T
    let dut : Top
    run
        assert BAD == 0 else fail("x")
    end run
end test T"#
        ))
        .expect_err("invalid const evaluation must fail lowering");
        let msg = assert_invalid(&err);
        assert!(
            msg.contains("const BAD") && msg.contains(needle),
            "diagnostic for `{init}` must contain `{needle}`; got: {msg}"
        );
    }
}

/// #521: `as uint<W>` / `as sint<W>` relabel casts fold â€” the value is
/// unchanged (matching the runtime relabel lowering and v1's 64-bit C
/// cast) and the signedness follows the cast target, so a negative
/// pattern can be moved into an unsigned const explicitly.
#[test]
fn const_relabel_cast_folds() {
    let cpp = emit_cpp_src(
        r#"const ALL : uint<64> = (0 - 1) as uint<64>
const BACK : sint<8> = (255 as sint<64>) >> 8

test T
    let dut : Top
    run
        assert ALL == ~0 else fail("x")
        assert BACK == 0 else fail("x")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("18446744073709551615"),
        "relabel-cast const must keep the bit pattern; got:\n{cpp}"
    );
    // BACK = (255 as sint<64>) >> 8 â€” the arithmetic shift of a
    // positive value is 0, and it must substitute as the signed zero
    // literal, proving the cast+shift actually folded.
    assert!(
        cpp.contains("((int64_t)(0))"),
        "BACK must fold to signed 0; got:\n{cpp}"
    );
}

/// #524 adversarial-review finding 6: signed values keep their
/// signedness at use sites. (a) A `sint` record field resolves through
/// the record schema, so `s.bias >> 1` emits an arithmetic shift; (b)
/// an explicitly `sint`-typed local declares as `int64_t` (v1's
/// `c_type_for`), so `/`, `%`, and ordered comparisons are signed at
/// the C++ level; (c) an untyped `let` of a signed expression infers
/// the RHS signedness (v1's `auto` â†’ int64_t).
#[test]
fn signed_use_sites_keep_signedness() {
    let cpp = emit_cpp_src(
        r#"const NEG : sint<8> = -1

struct Sample
    bias : sint<8>
end struct Sample

test T
    let dut : Top
    run
        let s : Sample
        s.bias = 0 - 8
        assert (s.bias >> 1) == 0 - 4 else fail("f")
        let t : sint<8> = 0 - 8
        assert (t / 2) == 0 - 4 else fail("t")
        let d = NEG
        assert (d >> 1) == NEG else fail("d")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("((int64_t)(s.bias)) >> 1"),
        "sint record field must get an arithmetic shift; got:\n{cpp}"
    );
    assert!(
        cpp.contains("int64_t t = 0") && cpp.contains("int64_t d = 0"),
        "sint-typed and signed-inferred locals must declare int64_t; got:\n{cpp}"
    );
    assert!(
        cpp.contains("((int64_t)(d)) >> 1"),
        "signed-inferred local must get an arithmetic shift; got:\n{cpp}"
    );
}

/// harc#532: signed values routed through a pure `function` helper must
/// keep their signedness at the C++ level. The helper's return slot,
/// params, and internal locals declare `int64_t` for `sint` (matching
/// v1's `c_type_for` / `cpp_sint_for_width`), so `/`, `%`, and ordered
/// comparisons compute signed â€” not just `>>` (which the shift emitter's
/// forced `(int64_t)` cast already saved). A `bool` return stays
/// `uint64_t` (a 0/1 value â€” no signedness divergence); only a `sint`
/// param flips. Before the fix every helper local was `uint64_t`.
#[test]
fn signed_helper_params_locals_return_declare_int64() {
    let cpp = emit_cpp_src(
        r#"function div2(x: sint<8>) -> sint<8>
    return x / 2
end function div2

function halve_twice(x: sint<16>) -> sint<16>
    let h : sint<16> = x / 2
    return h / 2
end function halve_twice

function isneg(x: sint<8>) -> bool
    return x < 0
end function isneg

test T
    let dut : Top
    run
        let a : sint<8> = 0 - 8
        assert div2(a) == 0 - 4 else fail("d")
        assert halve_twice(0 - 16) == 0 - 4 else fail("h")
        assert isneg(a) else fail("n")
    end run
end test T"#,
    );
    // `sint` param + `sint` return â†’ int64_t.
    assert!(
        cpp.contains("static int64_t harc_helper_div2(int64_t x)"),
        "sint helper param/return must declare int64_t; got:\n{cpp}"
    );
    // A helper whose signedness lives in an internal local (not the
    // param) is int64_t in signature and body.
    assert!(
        cpp.contains("static int64_t harc_helper_halve_twice(int64_t x)"),
        "sint helper with internal sint local must declare int64_t; got:\n{cpp}"
    );
    // A `bool` return keeps uint64_t; only the sint param flips.
    assert!(
        cpp.contains("static uint64_t harc_helper_isneg(int64_t x)"),
        "bool-returning helper keeps uint64_t return, int64_t sint param; got:\n{cpp}"
    );
}

/// #530 residual (#524 adversarial-review finding 6): signed HOST-STATE
/// members â€” `_tb` scalar fields, component/scoreboard scalar fields â€”
/// resolve their declared signedness through the owning schema, so
/// `>>` on them emits the arithmetic form v1's raw member access gets.
/// (Transactor state is covered end-to-end by
/// `signed_state_field_test.harc`; it needs a bus-bound DUT.)
#[test]
fn signed_host_state_keeps_signedness() {
    let cpp = emit_cpp_src(
        r#"scoreboard MSb
    delta : sint<8> default 0

    hookable half() -> sint<8>
        return delta >> 1
    end half
end scoreboard MSb

testbench HTb
    dut : Top
    bias : sint<8> default 0
end testbench HTb

impl HTest for HTb
    let env : MSb

    run
        bias = 0 - 8
        assert (bias >> 1) == 0 - 4 else fail("t")
        assert (env.delta >> 1) == 0 - 4 else fail("p")
    end run
end impl HTest"#,
    );
    assert!(
        cpp.contains("((int64_t)(_tb.bias)) >> 1"),
        "sint _tb field must get an arithmetic shift; got:\n{cpp}"
    );
    assert!(
        cpp.contains("((int64_t)(self.delta)) >> 1"),
        "sint component field read in a method must get an arithmetic shift; got:\n{cpp}"
    );
    assert!(
        cpp.contains("((int64_t)(env.delta)) >> 1"),
        "sint component field read by path must get an arithmetic shift; got:\n{cpp}"
    );
}

#[test]
fn signed_relabel_cast_preserves_narrow_source_value() {
    let cpp = emit_cpp_src(
        r#"test T
    let dut : Top
    run
        let byte : uint<8> = 255
        let signed : sint<64> = byte as sint<64>
        assert signed == 255 else fail("relabel")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("= ((int64_t)((uint64_t)(byte)));"),
        "as sint<64> must relabel without sign-extending a narrow source; got:\n{cpp}"
    );
}

#[test]
fn signed_wide_right_shift_is_rejected_in_tbir() {
    let err = emit_cpp_src_result(
        r#"test T
        let dut : Top
    run
        let wide : sint<128> = 0
        assert (wide >> 1) == 0 else fail("wide-shr")
        assert ((wide + 1) >> 1) == 0 else fail("wide-nested-shr")
        assert ((1 + wide) >> 1) == 0 else fail("wide-rhs-shr")
    end run
end test T"#,
    )
    .expect_err("TB-IR must not silently truncate signed shifts above 64 bits");
    assert!(
        err.contains("right shift above 64 bits"),
        "wide signed shift must have a targeted diagnostic; got: {err}"
    );
}

/// harc#473: `-%` masks like `+%`/`*%`. A typed `z : uint<8>` minus a
/// literal masks the two's-complement residue at 8 b, so `0 -% 1` emits
/// `(z - 1) & 0xFF` (== 255 at runtime), not the un-wrapped `z - 1`.
/// (`+%`/`*%` are covered above; this locks the subtraction path, which is
/// otherwise only exercised at runtime by the fixture.)
#[test]
fn wrapping_sub_op_masks_to_operand_width() {
    let cpp = emit_cpp_src(
        r#"test T
    let dut : Top
    run
        let z : uint<8> = 0
        let d : uint<8> = z -% 1
        assert d == 255 else fail("x")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("((z - 1)) & 0xFFULL"),
        "expected 8-bit sub-wrap mask `(z - 1) & 0xFF`; got:\n{cpp}"
    );
}

/// harc#473: a wrapping op nested inside another (`(a +% b) *% c`) masks at
/// *each* level's own operand width, not just the outermost. The inner
/// `a +% b` lowers to its own `& 0xFF` and the outer `*% c` wraps that
/// masked value again â€” so two independent 8-bit masks appear. This locks
/// the recursive `Binary` arm of `infer_wrap_operand_width`, which reports a
/// nested wrap's result width as `max(W(lhs), W(rhs))`; a single-level
/// fixture cannot catch a regression in that composition.
#[test]
fn wrapping_ops_nest_masks_at_each_level() {
    let cpp = emit_cpp_src(
        r#"test T
    let dut : Top
    run
        let a : uint<8> = 200
        let b : uint<8> = 100
        let c : uint<8> = 3
        let r : uint<8> = (a +% b) *% c
        assert r == 132 else fail("x")
    end run
end test T"#,
    );
    // Inner add-wrap masks at 8 b ...
    assert!(
        cpp.contains("((a + b)) & 0xFFULL"),
        "expected inner add-wrap mask `(a + b) & 0xFF`; got:\n{cpp}"
    );
    // ... and the outer mul-wrap masks the already-masked value again, so at
    // least two 8-bit masks are present in the emitted assignment.
    let masks = cpp.matches("& 0xFFULL").count();
    assert!(
        masks >= 2,
        "expected both nested wraps to mask at 8 b (>=2 `& 0xFF`); found {masks} in:\n{cpp}"
    );
}

/// Locks the dump-ir text for the tracer-bullet fixture: testbench /
/// test schemas, block structure, port hoisting, loop shapes,
/// interpolated format args.
#[test]
fn top_counter_dump_ir_snapshot() {
    let prog = lower_src(&fixture("top_counter_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("top_counter_dump_ir", format!("{prog}"));
}

/// Locks the dump-ir text for the arbiter fixture: request-mask
/// writes with single-cycle waits, grant asserts carrying inline port
/// reads, a two-point covergroup, and check-phase CovBin reads.
#[test]
fn bus_arbiter_dump_ir_snapshot() {
    let prog = lower_src(&fixture("bus_arbiter_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("bus_arbiter_dump_ir", format!("{prog}"));
}

/// Locks the dump-ir text for the ROM fixture: an impure helper
/// (`read_addr`) CFG-inlined at every call site, a full-address-space
/// covergroup, and the check-phase bin reads.
#[test]
fn rom_lut_dump_ir_snapshot() {
    let prog = lower_src(&fixture("rom_lut_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("rom_lut_dump_ir", format!("{prog}"));
}

/// Locks the dump-ir text for the probe/force fixture: read-only
/// `(probe)` PortRefs in assert conditions/format args, a `(force)`
/// `DutWrite`, and a `ProbeRelease`. Guards the `PortAccess` flow added
/// by the probe/force slice (was always `Port`).
#[test]
fn probe_force_dump_ir_snapshot() {
    let prog = lower_src(&fixture("probe_force_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("probe_force_dump_ir", format!("{prog}"));
}

/// A testbench-OWNED probed DUT (probes declared inside the `testbench`
/// block, not the `impl`) must still flow probes through the impl-for
/// desugar. Regression for issue #204 on the tbir path.
#[test]
fn testbench_owned_probes_lower() {
    let prog = lower_src(&fixture("testbench_probe_dut_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let txt = format!("{prog}");
    assert!(txt.contains("dut.inject_rs1 (force)"), "{txt}");
    assert!(txt.contains("dut.alu_a (probe)"), "{txt}");
    assert!(
        txt.contains("ProbeRelease(dut.inject_rs1 (force))"),
        "{txt}"
    );
}

/// Writing a read-only `probe` is a hard error (not a `--codegen v1`
/// fallback): only `probe force` opts into the SV procedural-force path.
#[test]
fn write_to_readonly_probe_is_rejected() {
    let src = r#"testbench T
end testbench T

impl Tst for T
    let dut : CpuPipe
        probe alu_a : uint<32> at alu0.a
    end let dut
    run
        dut.alu_a = 5
    end run
end impl Tst"#;
    let err = lower_src(src).expect_err("read-only probe write must be rejected");
    let msg = err.to_string();
    assert!(matches!(err, lower::LowerError::Invalid(_)), "{err:?}");
    assert!(msg.contains("read-only probe"), "{msg}");
    assert!(msg.contains("probe force"), "{msg}");
}

/// `release` of a read-only probe is a hard error â€” only a force probe
/// can be released.
#[test]
fn release_of_readonly_probe_is_rejected() {
    let src = r#"testbench T
end testbench T

impl Tst for T
    let dut : CpuPipe
        probe alu_a : uint<32> at alu0.a
    end let dut
    run
        release dut.alu_a
    end run
end impl Tst"#;
    let err = lower_src(src).expect_err("release of read-only probe must be rejected");
    let msg = err.to_string();
    assert!(matches!(err, lower::LowerError::Invalid(_)), "{err:?}");
    assert!(msg.contains("read-only probe"), "{msg}");
}

/// harc-com#493: a bus type named by `use` that never resolved (here,
/// unconditionally â€” `lower_src` skips `resolve_use_imports`, so every
/// `use` is "unresolved" from lowering's point of view) must not fall
/// through to the generic "let with a bind" rejection when a test binds
/// against it. The diagnostic should name the missing type and point at
/// the failed `use`, not tell the user to remove the bind.
#[test]
fn unresolved_use_bind_gets_targeted_diagnostic() {
    let src = r#"use MissingBus

testbench T
end testbench T

impl Tst for T
    let dut : SomeDut
    let axil : MissingBus = bind dut
    run
    end run
end impl Tst"#;
    let err = lower_src(src).expect_err("bind against an unresolved `use` type must be rejected");
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "must be LowerError::Invalid, not Unsupported (the type isn't out-of-subset, \
         it's missing): {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("MissingBus"),
        "must name the missing type: {msg}"
    );
    assert!(
        msg.contains("use MissingBus"),
        "must point at the failed `use`: {msg}"
    );
    assert!(
        !msg.contains("with probes or a bind"),
        "must not fall back to the generic misleading message: {msg}"
    );
}

/// Sibling of the above: a `use` that never resolves but is also never
/// bound against must keep parsing exactly as before (non-goal in
/// harc-com#493) â€” existing fixtures with a dangling `use arc.stdlib.X`
/// line must not gain a new error.
#[test]
fn unused_unresolved_use_does_not_error() {
    let src = r#"use MissingBus

testbench T
end testbench T

impl Tst for T
    let dut : SomeDut
    run
        let x : uint<8> = 1
        assert x == 1 else fail("x")
    end run
end impl Tst"#;
    lower_src(src).expect("an unused, never-resolved `use` must not error");
}

// â”€â”€ Emitted-C++ snapshots â€” the emission surface for the original
//    five fixtures of the equivalence matrix
//    (tests/tbir_equiv_fixtures.txt). Full files,
//    so any future emitter refactor diffs visibly here instead of
//    silently shifting shapes the marker tests don't cover. â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

#[test]
fn top_counter_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "top_counter_emitted_cpp",
        emit_fixture_cpp("top_counter_test.harc")
    );
}

#[test]
fn sync_fifo_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "sync_fifo_emitted_cpp",
        emit_fixture_cpp("sync_fifo_test.harc")
    );
}

#[test]
fn bus_arbiter_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "bus_arbiter_emitted_cpp",
        emit_fixture_cpp("bus_arbiter_test.harc")
    );
}

#[test]
fn wait_until_counter_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "wait_until_counter_emitted_cpp",
        emit_fixture_cpp("wait_until_counter_test.harc")
    );
}

#[test]
fn rom_lut_emitted_cpp_snapshot() {
    insta::assert_snapshot!("rom_lut_emitted_cpp", emit_fixture_cpp("rom_lut_test.harc"));
}

/// Locks the dump-ir text for the file-log fixture: `logf` statements
/// carrying `LogLevel::File` (path + severity) alongside console
/// info/warn logs with interpolated port reads.
#[test]
fn log_paths_dump_ir_snapshot() {
    let prog = lower_src(&fixture("log_paths_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("log_paths_dump_ir", format!("{prog}"));
}

/// Locks the dump-ir text for the fatal-path fixture: the
/// `LogLevel::Fatal` statement (errors++ AND `_fatal = true` in both
/// emitters) followed by a wait and a post-fatal statement that the
/// drive loop must never reach.
#[test]
fn fatal_path_dump_ir_snapshot() {
    let prog = lower_src(&fixture("fatal_path_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("fatal_path_dump_ir", format!("{prog}"));
}

/// The unbound event-driven transactor (`req : in event<RegOp>` +
/// `on req(t)` driving raw DUT signals, `emit drv.req(t)` from the test
/// scope) now lowers: the transactor routes to the composite-component
/// table, its `in event` field becomes a subscriber-callback vector, the
/// `on` handler body lowers as a synchronous component subscriber (waits
/// â†’ sync tick loops), and the DUT handle field pokes the test DUT.
#[test]
fn event_driven_transactor_fixture_lowers() {
    let prog = lower_src(&fixture("axilite_seqdrv_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    // The transactor lowered to a component with a DUT handle, a scalar
    // state field, and an `event` input pipe with one `on` handler.
    let comp = prog
        .components
        .iter()
        .find(|c| c.name == "SeqXactor")
        .expect("SeqXactor component");
    assert!(comp
        .fields
        .iter()
        .any(|f| matches!(f.kind, ir::ComponentFieldKind::Dut { .. })));
    assert!(comp
        .fields
        .iter()
        .any(|f| matches!(f.kind, ir::ComponentFieldKind::Event { .. })));
    assert_eq!(comp.on_handlers.len(), 1);
}

/// The *bound-to* event-driven transactor (`transactor X bound to
/// BusAxiLite` + `req : in event` + `on req` driving the bound bus's
/// handshake channels) now lowers: it routes to the composite-component
/// table with a `bound_bus`, its `on req` handler body resolves
/// `bus.<ch>.send/recv` against the bound binding (CFG-inlined valid/
/// ready spin loops), and the test-scope `let xact : X active = bind
/// axil` fills the placeholder bus prefix with the real binding name.
#[test]
fn bound_event_driven_transactor_lowers() {
    let prog =
        lower_with_stdlib_bus("transactor_active_test.harc", "BusAxiLite.arch").expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let comp = prog
        .components
        .iter()
        .find(|c| c.name == "AxilXactor")
        .expect("AxilXactor component");
    // Bound to a bus, with an `in event` input pipe + one `on` handler,
    // a scalar state field, and NO private DUT handle (drives the bus).
    assert_eq!(comp.bound_bus.as_deref(), Some("BusAxiLite"));
    assert!(comp
        .fields
        .iter()
        .any(|f| matches!(f.kind, ir::ComponentFieldKind::Event { .. })));
    assert!(comp
        .fields
        .iter()
        .any(|f| matches!(f.kind, ir::ComponentFieldKind::Scalar { .. })));
    assert!(!comp
        .fields
        .iter()
        .any(|f| matches!(f.kind, ir::ComponentFieldKind::Dut { .. })));
    assert_eq!(comp.on_handlers.len(), 1);
}

/// The bound-to MONITOR surface: a transactor's always-on
/// `on bus.<ch>.handshake(arg)` observer handlers (the passive half of
/// the bound-to-agent cluster) lower into monitor cycle-trigger handlers,
/// and a `passive` bound instance is accepted. Each monitor handler:
///   * carries `monitor_channel = Some(<ch>)`, rising edge;
///   * has a body capturing the channel payload into `arg` (+ per-field
///     aliases) then feeding the sub-scoreboard.
#[test]
fn bound_monitor_handshake_handlers_lower_passive() {
    let prog = lower_with_stdlib_bus("transactor_passive_only_test.harc", "BusAxiLite.arch")
        .expect("passive-only bound monitor lowers");
    verify::verify_program(&prog).expect("verifies");
    let comp = prog
        .components
        .iter()
        .find(|c| c.name == "AxilXactor")
        .expect("AxilXactor component");
    assert_eq!(comp.bound_bus.as_deref(), Some("BusAxiLite"));
    // Two `on bus.<ch>.handshake` observers â†’ two monitor cycle handlers
    // (channels `w` and `r`), both rising-edge.
    let monitors: Vec<&ir::CycleTriggerHandlerSchema> = comp
        .cycle_handlers
        .iter()
        .filter(|ch| ch.monitor_channel.is_some())
        .collect();
    assert_eq!(monitors.len(), 2, "expected w + r handshake monitors");
    assert!(monitors.iter().all(|ch| ch.edge == ir::CycleEdge::Rising));
    let channels: std::collections::HashSet<&str> = monitors
        .iter()
        .map(|ch| ch.monitor_channel.as_deref().unwrap())
        .collect();
    assert!(channels.contains("w") && channels.contains("r"));
    // The synthesized trigger must read the BOUND binding's wires, not
    // the placeholder (`axil_w_valid`, not `bus_w_valid`).
    let w_mon = monitors
        .iter()
        .find(|ch| ch.monitor_channel.as_deref() == Some("w"))
        .unwrap();
    let trig = format!("{prog}");
    assert!(
        trig.contains("dut.axil.w.valid") && trig.contains("dut.axil.w.ready"),
        "monitor trigger should read the bound binding wires, got dump:\n{trig}"
    );
    let _ = w_mon;
}

/// A `passive` bound instance of a PURE-DRIVER transactor (no monitor
/// half) is inert and must be rejected precisely (the driver lives under
/// `when active`; a passive instance would observe nothing).
#[test]
fn passive_bound_instance_without_monitor_rejected() {
    let prog = lower_with_stdlib_bus("transactor_active_test.harc", "BusAxiLite.arch");
    // transactor_active_test binds `active`, so it lowers â€” sanity that
    // the fixture itself is fine; the rejection is exercised by the unit
    // source below (active fixture mutated to passive).
    assert!(prog.is_ok());
    let src = r#"bus B
    handshake_channel ch: send kind: valid_ready
        data: uint<32>;
    end handshake_channel ch
end bus B

transactor Drv bound to B
    when active
        req : in event<uint<32>>
        on req(t)
            bus.ch.send(t)
        end on
    end when
end transactor Drv

test T
    let dut : SomeDut
    let b : B = bind dut
    let drv : Drv passive = bind b
    run
    end run
end test T"#;
    let err = lower_src(src).expect_err("passive pure-driver must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("passive") && msg.contains("no monitor half"),
        "rejection should name the inert passive-no-monitor case: {msg}"
    );
}

/// Randomize/constraint fixture (`randomize(t) with` + Z3 constraints,
/// loaded together with its helper file exactly as run_fixtures.sh
/// does) now lowers through the constraint-IR seam: every `randomize`
/// site becomes a `Terminator::Randomize` carrying a `ConstraintRef`.
/// (Was a negative test before this slice; see git history.)
#[test]
fn randomize_fixture_lowers() {
    let prog = lower_fixtures(&["axilite_constraint_test.harc", "axilite_regs_test.harc"])
        .expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    assert!(
        !prog.constraint_sites.is_empty(),
        "the `randomize(p) with` site lowered into a constraint site"
    );
    let randomize_blocks = prog
        .functions
        .iter()
        .flat_map(|f| &f.blocks)
        .filter(|b| matches!(b.terminator, ir::Terminator::Randomize { .. }))
        .count();
    assert!(randomize_blocks >= 1, "a Randomize terminator is present");
}

/// `tseq` (transaction-sequence) lowers: the generator becomes a
/// `FunctionKind::Tseq` function whose body carries `SeqPush` (`yield`)
/// and a `Randomize` terminator, and the test body iterates the
/// materialized `RecordSeq` with a `SeqLen`/`SeqIndex` counted loop.
#[test]
fn tseq_basic_fixture_lowers() {
    let prog = lower_src(&fixture("tseq_basic_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");

    // Exactly one Tseq function, with a RecordSeq accumulator and a
    // SeqPush statement in its body.
    let tseq_fn = prog
        .functions
        .iter()
        .find(|f| matches!(f.kind, ir::FunctionKind::Tseq { .. }))
        .expect("a FunctionKind::Tseq function is present");
    assert!(
        matches!(
            tseq_fn.ret.map(|r| &tseq_fn.local(r).ty),
            Some(ir::IrType::RecordSeq(_))
        ),
        "the tseq `ret` accumulator is RecordSeq-typed"
    );
    let has_seq_push = tseq_fn
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .any(|s| matches!(s, ir::Stmt::SeqPush { .. }));
    assert!(has_seq_push, "`yield t` lowered to a SeqPush");
    let has_randomize = tseq_fn
        .blocks
        .iter()
        .any(|b| matches!(b.terminator, ir::Terminator::Randomize { .. }));
    assert!(has_randomize, "`randomize(t)` inside the tseq lowered");

    // The run body has a RecordSeq local (the `let txns = Gen(5)` result),
    // a Tseq call edge, and a SeqLen/SeqIndex iteration.
    let run = prog.function(prog.tests[0].run);
    assert!(
        run.locals
            .iter()
            .any(|l| matches!(l.ty, ir::IrType::RecordSeq(_))),
        "the materialized sequence local is RecordSeq-typed"
    );
    let has_tseq_call = run.blocks.iter().flat_map(|b| &b.stmts).any(|s| {
        matches!(
            s,
            ir::Stmt::Assign(_, ir::Expr::Call(ir::CallTarget::Tseq(_), _))
        )
    });
    assert!(
        has_tseq_call,
        "`let txns = Gen(5)` is a CallTarget::Tseq edge"
    );
    let has_seq_index = run
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .any(|s| matches!(s, ir::Stmt::Assign(_, ir::Expr::SeqIndex { .. })));
    assert!(
        has_seq_index,
        "`for t in txns` binds t to seq[i] via SeqIndex"
    );
}

/// `wait_until_quiesce` composes an `agent`, binds it as a TESTBENCH
/// FIELD (`prod : Producer`), and drives it with `emit prod.in_ev(t)`
/// where the event payload is `event<TinyTxn>` â€” a *transaction*
/// payload. With record-payload events lowered (this slice), the
/// fixture lowers fully: the agent's `in_ev` field carries a
/// `Record` payload and its `on in_ev(t)` handler takes a
/// record-typed argument.
#[test]
fn wait_until_quiesce_fixture_lowers_record_event() {
    let prog = lower_src(&fixture("wait_until_quiesce_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let agent = prog
        .components
        .iter()
        .find(|c| c.name == "Producer")
        .expect("Producer agent");
    // The `in_ev : event<TinyTxn>` field is a record-payload event.
    let ev = agent
        .fields
        .iter()
        .find(|f| f.name == "in_ev")
        .expect("in_ev field");
    let rid = match &ev.kind {
        ir::ComponentFieldKind::Event {
            payload: ir::EventPayload::Record(r),
        } => *r,
        other => panic!("in_ev should be a record-payload event, got {other:?}"),
    };
    assert_eq!(prog.records[rid.index()].name, "TinyTxn");
    // Its `on in_ev(t)` handler takes the same record by value.
    let oh = agent.on_handlers.first().expect("on-handler");
    assert_eq!(oh.arg_payload, ir::EventPayload::Record(rid));
    let body = prog.function(oh.function);
    assert_eq!(
        body.params.first().map(|p| &p.ty),
        Some(&ir::IrType::Record(rid)),
        "handler arg is the record type"
    );
}

/// Locks the dump-ir text for the heartbeat fixture: an `agent` with a
/// record-payload `event<TinyTxn>` field, an `on in_ev(t)` handler
/// taking the record by value, `emit prod.in_ev(t)` carrying a record
/// local, and the `idle_in` heartbeat predicate poll.
#[test]
fn heartbeat_idle_dump_ir_snapshot() {
    let prog = lower_src(&fixture("heartbeat_idle_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("heartbeat_idle_dump_ir", format!("{prog}"));
}

/// `watchdog_quiesce_test` stacks a `watchdog` directive on top of the
/// record-payload event. The watchdog lowers to a zero-arg
/// `comp_watchdog_*` body (the user heartbeat log; the period/max_idle
/// idle check is emitted in the per-instance `_checkers` closure) plus a
/// `watchdog period 500 max_idle 1000` schema line. Locks the dump-ir.
#[test]
fn watchdog_quiesce_dump_ir_snapshot() {
    let prog = lower_src(&fixture("watchdog_quiesce_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let agent = prog
        .components
        .iter()
        .find(|c| c.name == "Producer")
        .expect("Producer agent");
    let w = agent.watchdog.as_ref().expect("watchdog schema");
    // The body references `cycle_count` (a framework value) and a self
    // field (`seen`), both lowered in the component-self context.
    let body = prog.function(w.function);
    assert!(body.params.is_empty(), "watchdog body takes only `self`");
    insta::assert_snapshot!("watchdog_quiesce_dump_ir", format!("{prog}"));
}

/// Locks the emitted tbir C++ for the watchdog fixture: the zero-arg
/// `<Comp>_watchdog<fid>` body lambda, and the per-instance `_checkers`
/// closure that gates on the period static, runs the body, then the
/// `max_idle` idle check + FAIL diagnostic.
#[test]
fn watchdog_quiesce_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "watchdog_quiesce_emitted_cpp",
        emit_fixture_cpp("watchdog_quiesce_test.harc")
    );
}

/// `env_quiesced_phase_test` exercises three TB-IR constructs together:
///   1. a DATA-ONLY `scoreboard` (`DrainSb`) bound as an env SUB-component
///      (`ComponentFieldKind::ScoreboardSub`) â€” accessed by the nested
///      run-scope path `top.sb.expected` (not `_tb.sb`);
///   2. `<env>.quiesced(N)` â€” expands to an AND of `idle(N)` over every
///      leaf sub-component (`top.prod.idle(8) && top.sb.idle(8)`);
///   3. a named `phase drain` whose body is INLINED at the `drain()` call
///      site in the run body.
/// Locks the dump-ir for all three.
#[test]
fn env_quiesced_phase_dump_ir_snapshot() {
    let prog = lower_src(&fixture("env_quiesced_phase_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    // The env holds the data scoreboard as a `ScoreboardSub` field.
    let env = prog
        .components
        .iter()
        .find(|c| c.name == "HeartbeatEnv")
        .expect("HeartbeatEnv env");
    assert!(
        env.fields
            .iter()
            .any(|f| matches!(f.kind, ir::ComponentFieldKind::ScoreboardSub { .. })),
        "env should hold the data scoreboard as a ScoreboardSub field"
    );
    insta::assert_snapshot!("env_quiesced_phase_dump_ir", format!("{prog}"));
}

/// Locks the emitted tbir C++ for the env-quiesce-phase fixture: the
/// nested `top.sb.expected.push/pop/empty` access, the `quiesced(8)`
/// idle conjunction inside the `wait_until_timeout` predicate, and the
/// inlined `drain` phase body.
#[test]
fn env_quiesced_phase_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "env_quiesced_phase_emitted_cpp",
        emit_fixture_cpp("env_quiesced_phase_test.harc")
    );
}

/// `on <N> cycles` periodic handler in an `agent`: lowers to a zero-arg
/// `comp_periodic_*` body and an `on 10 cycles = fn0` schema line.
#[test]
fn agent_periodic_dump_ir_snapshot() {
    let prog = lower_src(&fixture("agent_periodic_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let agent = prog
        .components
        .iter()
        .find(|c| c.name == "Ticker")
        .expect("Ticker agent");
    assert_eq!(agent.periodic_handlers.len(), 1, "one periodic handler");
    let ph = &agent.periodic_handlers[0];
    let body = prog.function(ph.function);
    assert!(body.params.is_empty(), "periodic body takes only `self`");
    insta::assert_snapshot!("agent_periodic_dump_ir", format!("{prog}"));
}

/// Untimed `any of` lowers to a `WaitUntil` terminator in `AnyOf`
/// mode with every sub-predicate kept inline (the emitter `||`-joins
/// them, matching v1's disjunction).
#[test]
fn wait_until_any_of_lowers_to_any_of_mode() {
    let src = r#"
test WaitUntilAnyTest
    let dut : Top
    run
        wait until any of dut.ready == 1, dut.count_out == 2
    end run
end test WaitUntilAnyTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let f = prog.function(prog.tests[0].run);
    let ir::Terminator::WaitUntil { preds, mode, .. } = &f.blocks[0].terminator else {
        panic!("expected WaitUntil terminator:\n{f}");
    };
    assert_eq!(*mode, ir::WaitMode::AnyOf);
    assert_eq!(preds.len(), 2);
    assert_eq!(preds[0].src_text, "dut.ready == 1");
    assert_eq!(preds[1].src_text, "dut.count_out == 2");
}

/// Timed `any of` carries v1's any-of timeout diagnostics: the default
/// header is "wait until any of timed out after %lld cycles" and the
/// breakdown is ONE unguarded "  none of: <src1>, <src2>" line (a
/// timed-out any-of means no predicate ever fired â€” v1 lists them all
/// without re-checking), not the per-predicate "not yet true:" lines.
#[test]
fn wait_until_any_of_timeout_diag_block() {
    let src = r#"
test WaitAnyTimeoutTest
    let dut : Top
    run
        wait until any of dut.ready == 1, dut.count_out == 2 timeout 50 cycles
    end run
end test WaitAnyTimeoutTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let f = prog.function(prog.tests[0].run);
    let ir::Terminator::WaitUntilTimeout {
        mode,
        on_timeout,
        on_fire,
        ..
    } = &f.blocks[0].terminator
    else {
        panic!("expected WaitUntilTimeout terminator:\n{f}");
    };
    assert_eq!(*mode, ir::WaitMode::AnyOf);
    let diag = f.block(*on_timeout);
    assert_eq!(diag.stmts.len(), 2, "header + one none-of line:\n{f}");
    let ir::Stmt::FailDiag { guard: None, args } = &diag.stmts[0] else {
        panic!("first diag stmt is the unguarded header:\n{f}");
    };
    assert_eq!(args.fmt, "wait until any of timed out after %lld cycles");
    let ir::Stmt::FailDiag { guard: None, args } = &diag.stmts[1] else {
        panic!("second diag stmt is the unguarded none-of line:\n{f}");
    };
    assert_eq!(args.fmt, "  none of: dut.ready == 1, dut.count_out == 2");
    assert!(args.args.is_empty());
    assert!(
        matches!(diag.terminator, ir::Terminator::Jump(b) if b == *on_fire),
        "on_timeout rejoins on_fire:\n{f}"
    );
}

/// Untimed single-predicate `wait until` becomes a `WaitUntil`
/// terminator with the port read kept inline (re-sampled each cycle)
/// and the source text captured for diagnostics.
#[test]
fn wait_until_single_lowers_to_wait_until_terminator() {
    let src = r#"
test WaitSingleTest
    let dut : Top
    run
        wait until dut.ready == 1
        dut.en = 0
    end run
end test WaitSingleTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let f = prog.function(prog.tests[0].run);
    let ir::Terminator::WaitUntil { preds, mode, succ } = &f.blocks[0].terminator else {
        panic!("expected WaitUntil terminator:\n{f}");
    };
    assert_eq!(*mode, ir::WaitMode::Single);
    assert_eq!(preds.len(), 1);
    assert_eq!(preds[0].src_text, "dut.ready == 1");
    assert!(
        matches!(&preds[0].expr, ir::Expr::Binary(ir::BinOp::Eq, l, _)
            if matches!(&**l, ir::Expr::Port(_))),
        "port stays inline in the wait predicate:\n{f}"
    );
    // Successor carries the post-wait statements.
    assert!(
        f.block(*succ)
            .stmts
            .iter()
            .any(|s| matches!(s, ir::Stmt::DutWrite(..))),
        "wait successor continues the body:\n{f}"
    );
}

/// `wait until ... timeout N cycles fail("...")` becomes a
/// `WaitUntilTimeout` whose `on_timeout` block carries the v1
/// diagnostic shape: unconditional FAIL header (the user's message),
/// one guarded "not yet true:" line per sub-predicate, then a rejoin
/// to the success path.
#[test]
fn wait_until_timeout_lowers_diag_block() {
    let src = r#"
test WaitTimeoutTest
    let dut : Top
    run
        wait until all of dut.count_out >= 12, dut.en == 1 timeout 100 cycles fail("quiesce conditions not met")
    end run
end test WaitTimeoutTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let f = prog.function(prog.tests[0].run);
    let ir::Terminator::WaitUntilTimeout {
        preds,
        mode,
        cycles,
        on_fire,
        on_timeout,
    } = &f.blocks[0].terminator
    else {
        panic!("expected WaitUntilTimeout terminator:\n{f}");
    };
    assert_eq!(*mode, ir::WaitMode::AllOf);
    assert_eq!(preds.len(), 2);
    assert_eq!(preds[0].src_text, "dut.count_out >= 12");
    assert_eq!(preds[1].src_text, "dut.en == 1");
    // Budget is evaluated once into a local before the wait.
    assert!(
        matches!(cycles, ir::Expr::Local(_)),
        "budget stashed in a local:\n{f}"
    );

    let diag = f.block(*on_timeout);
    assert_eq!(diag.stmts.len(), 3, "header + 2 breakdown lines:\n{f}");
    let ir::Stmt::FailDiag { guard: None, args } = &diag.stmts[0] else {
        panic!("first diag stmt is the unguarded header:\n{f}");
    };
    assert_eq!(args.fmt, "quiesce conditions not met");
    let ir::Stmt::FailDiag {
        guard: Some(_),
        args,
    } = &diag.stmts[1]
    else {
        panic!("second diag stmt is a guarded breakdown line:\n{f}");
    };
    assert_eq!(args.fmt, "  not yet true: dut.count_out >= 12");
    let ir::Stmt::FailDiag {
        guard: Some(_),
        args,
    } = &diag.stmts[2]
    else {
        panic!("third diag stmt is a guarded breakdown line:\n{f}");
    };
    assert_eq!(args.fmt, "  not yet true: dut.en == 1");
    // Timeout arm rejoins the success path.
    assert!(
        matches!(diag.terminator, ir::Terminator::Jump(b) if b == *on_fire),
        "on_timeout rejoins on_fire:\n{f}"
    );
}

/// Default (message-less) timeout header mirrors v1's
/// "<label> timed out after %lld cycles" text, with the budget local
/// as the format argument.
#[test]
fn wait_until_timeout_default_header() {
    let src = r#"
test WaitDefaultHeaderTest
    let dut : Top
    run
        wait until dut.ready == 1 timeout 50 cycles
    end run
end test WaitDefaultHeaderTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let f = prog.function(prog.tests[0].run);
    let ir::Terminator::WaitUntilTimeout { on_timeout, .. } = &f.blocks[0].terminator else {
        panic!("expected WaitUntilTimeout terminator:\n{f}");
    };
    let ir::Stmt::FailDiag { guard: None, args } = &f.block(*on_timeout).stmts[0] else {
        panic!("first diag stmt is the header:\n{f}");
    };
    assert_eq!(args.fmt, "wait until timed out after %lld cycles");
    assert_eq!(args.args.len(), 1);
    assert!(matches!(args.args[0].expr, ir::Expr::Local(_)));
}

/// Locks the dump-ir text for the wait-until fixture (terminator
/// shapes, PredSrc source text, timeout diagnostic blocks).
#[test]
fn wait_until_counter_dump_ir_snapshot() {
    let prog = lower_src(&fixture("wait_until_counter_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("wait_until_counter_dump_ir", format!("{prog}"));
}

/// Locks the dump-ir text for the any-of fixture (`AnyOf` wait modes,
/// untimed + timed) and its emitted C++ (the `||`-joined awaiter
/// predicates).
#[test]
fn wait_any_of_dump_ir_snapshot() {
    let prog = lower_src(&fixture("wait_any_of_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("wait_any_of_dump_ir", format!("{prog}"));
}

#[test]
fn wait_any_of_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "wait_any_of_emitted_cpp",
        emit_fixture_cpp("wait_any_of_test.harc")
    );
}

/// Locks the dump-ir text for the deliberately-failing any-of timeout
/// fixture: both timeout diagnostic blocks (default header and user
/// `fail("â€¦")` header) carry the single unguarded "none of:" line.
#[test]
fn wait_any_of_timeout_dump_ir_snapshot() {
    let prog = lower_src(&fixture("wait_any_of_timeout_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("wait_any_of_timeout_dump_ir", format!("{prog}"));
}

// â”€â”€ lower_coroutine pass: CFG â†’ tagged-FSM metadata. Snapshots lock
//    the `harc dump-ir --pass lower-coroutine` suffix (the metadata
//    section the pass appends after the regular IR dump, which the
//    *_dump_ir snapshots above already lock). â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// top_counter: three wait-1-cycle loops + a trailing reset wait.
/// Locks resume-point state numbering and the collapsed loop /
/// loop-exit transitions with their branch-condition summaries.
#[test]
fn top_counter_lower_coroutine_snapshot() {
    let prog = lower_src(&fixture("top_counter_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let meta = lower_coroutine::run(&prog).expect("tags");
    insta::assert_snapshot!(
        "top_counter_lower_coroutine",
        format!("{}", meta.display(&prog))
    );
}

/// wait_until_counter: chained `WaitUntilTimeout`s. Locks the paired
/// fire/timeout edges and the timeout-handler states falling through
/// to the success path.
#[test]
fn wait_until_counter_lower_coroutine_snapshot() {
    let prog = lower_src(&fixture("wait_until_counter_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let meta = lower_coroutine::run(&prog).expect("tags");
    insta::assert_snapshot!(
        "wait_until_counter_lower_coroutine",
        format!("{}", meta.display(&prog))
    );
}

/// The pass is a side-table: running it must not perturb the IR (the
/// `dump-ir` text is byte-identical before and after), and its own
/// rendering is byte-stable across runs.
#[test]
fn lower_coroutine_leaves_ir_untouched_and_is_deterministic() {
    let prog = lower_src(&fixture("top_counter_test.harc")).expect("lowers");
    let before = format!("{prog}");
    let meta_a = lower_coroutine::run(&prog).expect("tags");
    let meta_b = lower_coroutine::run(&prog).expect("tags");
    assert_eq!(format!("{prog}"), before, "pass must not mutate the IR");
    assert_eq!(
        format!("{}", meta_a.display(&prog)),
        format!("{}", meta_b.display(&prog)),
        "metadata rendering must be byte-stable across runs"
    );
}

/// tbir emission of the wait-until fixture carries the v1 runtime
/// calls: untimed/timed awaiters and the timeout diagnostic text.
#[test]
fn tbir_emit_wait_until_runtime_calls() {
    let merged = merged_src(&fixture("wait_until_counter_test.harc"));
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits");
    for marker in [
        "bool _wu_satisfied = co_await harc_rt::wait_until_timeout(_slot, \
         [&]{ return (harc_rt::harc_read(dut->count_out) == 8); }, (uint32_t)_wu_budget);",
        "[&]{ return ((harc_rt::harc_read(dut->count_out) >= 12)) && \
         ((harc_rt::harc_read(dut->en) == 1)); }",
        "sim_log_line(\"FAIL\", \"count never reached 8\");",
        "sim_log_line(\"FAIL\", \"  not yet true: dut.count_out >= 12\");",
    ] {
        assert!(cpp.contains(marker), "missing wait-until marker `{marker}`");
    }
}

/// `randomize(t)` now lowers to a `Terminator::Randomize` carrying a
/// `ConstraintRef` into the program's constraint-site table, and the
/// tbir backend splices in v1's Z3-solve snippet (the constraint-IR
/// seam â€” `docs/tbir-mvp.md` Â§"randomize"). A bare `randomize(t)` of a
/// keep-free transaction routes through the unconstrained-PRNG shell.
#[test]
fn randomize_lowers_to_terminator() {
    let src = r#"
transaction Req
    addr : uint<32>
    keep addr % 4 == 0
end transaction Req

test RandTest
    let dut : Top
    run
        let t : Req
        randomize(t)
    end run
end test RandTest
"#;
    let merged = merged_src(src);
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    // One constraint site, carrying the merged keep set and a problem id.
    assert_eq!(prog.constraint_sites.len(), 1, "one randomize site");
    let site = &prog.constraint_sites[0];
    assert_eq!(site.record, "Req");
    assert_eq!(site.constraints.len(), 1, "transaction keep merged in");
    assert!(site.problem_id.is_some(), "Z3-ready problem id");
    // The run function ends a block with a Randomize terminator.
    let run = prog.function(prog.tests[0].run);
    assert!(
        run.blocks
            .iter()
            .any(|b| matches!(b.terminator, ir::Terminator::Randomize { .. })),
        "a Randomize terminator is present"
    );
    // tbir emits the v1 Z3-solve block (constraint-IR seam reused).
    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits");
    assert!(cpp.contains("z3::solver"), "Z3 solve block emitted");
    assert!(
        cpp.contains("#include \"harc_z3_rt.h\""),
        "z3 runtime header included"
    );
    assert!(
        cpp.contains("trace.randomize("),
        "randomize trace event emitted"
    );
}

/// Retargeting a reused v1 randomize snippet must rename only the
/// randomized object, not record-field members with the same spelling.
#[test]
fn randomize_snippet_retarget_preserves_record_field_names() {
    let src = r#"
transaction Req
    errors : uint<8>
    keep errors in [1..3]
end transaction Req

test RandNameCollisionTest
    let dut : Top
    run
        let errors : Req
        randomize(errors)
        assert errors.errors >= 1
    end run
end test RandNameCollisionTest
"#;
    let merged = merged_src(src);
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits");
    assert!(
        cpp.contains("_u_errors.errors"),
        "object local is sanitized while record field name is preserved:\n{cpp}"
    );
    assert!(
        !cpp.contains("_u_errors._u_errors"),
        "retargeting must not rewrite member names:\n{cpp}"
    );
}

#[test]
fn randomize_constraints_resolve_file_scope_integer_consts() {
    let src = r#"
const MAX_KIND : uint<4> = 6
const MAX_ERR : uint<3> = 2

transaction Choice
    cls  : uint<4>
    err  : uint<3>
end transaction Choice

test RandConstConstraintTest
    let dut : Top
    run
        let c : Choice
        randomize(c) with
            c.cls <= MAX_KIND
            c.err <= MAX_ERR
        end randomize
        assert c.cls <= MAX_KIND
        assert c.err <= MAX_ERR
    end run
end test RandConstConstraintTest
"#;
    let merged = merged_src(src);
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits");
    assert!(
        cpp.contains("z3::ule(_z_cls, _ctx.bv_val((uint64_t)6"),
        "randomize constraint should lower MAX_KIND to a literal:\n{cpp}"
    );
    assert!(
        cpp.contains("z3::ule(_z_err, _ctx.bv_val((uint64_t)2"),
        "randomize constraint should lower MAX_ERR to a literal:\n{cpp}"
    );
}

// â”€â”€ Transaction value-records (non-randomize usage) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Locks the dump-ir text for the transaction fixture: the records
/// table (defaults, `!` fields, inert keep/attr text), `RecordInit`
/// inside the loop body, `RecordFieldWrite`, and field reads in
/// asserts / branch conditions / loop bounds / format args.
#[test]
fn transaction_basic_dump_ir_snapshot() {
    let prog = lower_src(&fixture("transaction_basic_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("transaction_basic_dump_ir", format!("{prog}"));
}

/// Locks the emitted tbir C++ for the transaction fixture: the
/// value-record struct (member-initializer defaults, operator==/!=),
/// the record-typed hoisted local, and the let-site re-init.
#[test]
fn transaction_basic_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "transaction_basic_emitted_cpp",
        emit_fixture_cpp("transaction_basic_test.harc")
    );
}

/// Scoreboard data-only subset: the schema's scalar/queue fields, the
/// queue push (statement) / pop (let-RHS, then assign), scalar
/// read/write ops, and the size()/empty() value-queries in
/// assert/log positions, across run and check.
#[test]
fn scoreboard_basic_dump_ir_snapshot() {
    let prog = lower_src(&fixture("scoreboard_basic_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("scoreboard_basic_dump_ir", format!("{prog}"));
}

/// Locks the emitted tbir C++ for the scoreboard fixture: the
/// scoreboard struct (scalar defaults + `harc_rt::HarcQueue<T>`
/// members), the `_tb`-held instance, and the push/pop/size/empty/
/// scalar accessors.
#[test]
fn scoreboard_basic_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "scoreboard_basic_emitted_cpp",
        emit_fixture_cpp("scoreboard_basic_test.harc")
    );
}

/// Env-composition subset: a `let env : AnalysisEnv` composing an
/// analysis-source transactor (`out event` + `emit`) and two
/// method-bearing scoreboards, wired by `connect`. Locks the dump-ir:
/// the component schemas (sub-component fields, method signatures, the
/// env's resolved connect edges), the self-relative method bodies
/// (`ComponentEmit` / `ComponentFieldWrite`), and the test-body
/// `ComponentCall` / `ComponentField` path access.
#[test]
fn analysis_env_connect_dump_ir_snapshot() {
    let prog = lower_src(&fixture("analysis_sink_connect_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("analysis_env_connect_dump_ir", format!("{prog}"));
}

/// Locks the emitted tbir C++ for the env-composition fixture: the
/// component structs (event-callback vectors, by-value sub-components),
/// the `<Comp>_<method>` lambdas, and the env local + connect push_backs.
#[test]
fn analysis_env_connect_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "analysis_env_connect_emitted_cpp",
        emit_fixture_cpp("analysis_sink_connect_test.harc")
    );
}

/// Analysis `connect` is a typed subscription boundary. A plain `function`
/// is callable but is not hookable, so it must not silently become a
/// subscription sink; similarly, signed and unsigned scalar callback shapes
/// must agree before C++ emission.
#[test]
fn analysis_connect_rejects_non_hookable_and_payload_mismatched_sinks() {
    let non_hookable = r#"
transactor Source
    observed : out event<uint<8>>

    hookable publish(v: uint<8>)
        emit observed(v)
    end publish
end transactor Source

scoreboard Sink
    function accept(v: uint<8>)
    end accept
end scoreboard Sink

env E
    source : Source passive
    sink : Sink

    connect
        source.observed -> sink.accept
    end connect
end env E

test T
    let dut : Top
    let e : E
    run
        log(info, "test")
    end run
end test T
"#;
    // v1 emits `for (auto& _s : sink.accept)` over a `function` method,
    // which is not a struct member â€” g++: "'struct Sink' has no member
    // named 'accept'". So v1 is no escape hatch. It is not `Invalid`
    // either: an env nothing instantiates emits no wiring, and that
    // program runs fine under v1.
    let err = lower_src(non_hookable).expect_err("must reject non-hookable sink");
    let msg = assert_not_implemented(&err, lower::V1Status::EmitsUncompilable);
    assert!(
        msg.contains("not `hookable`"),
        "unexpected diagnostic: {msg}"
    );

    // This second case is a SIGNEDNESS-only mismatch, and v1 RUNS it:
    // the generic bridge converts `uint64_t` into the sink's `int64_t`
    // parameter and the program behaves as written (built and run:
    // count=2 sum=8). So it keeps the suggestion. It sat under
    // `assert_invalid` for one commit â€” the counterexample to that
    // verdict was inside the suite asserting it.
    let mismatched_payload = non_hookable.replace(
        "function accept(v: uint<8>)\n    end accept",
        "hookable accept(v: sint<8>)\n    end accept",
    );
    let err = lower_src(&mismatched_payload).expect_err("must reject incompatible payloads");
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("payload mismatch"),
        "unexpected diagnostic: {msg}"
    );
    cpp_tb::emit(&merged_src(&mismatched_payload)).expect("v1 emits the sign mismatch");
}

/// An omitted hookable-parameter annotation leaves the front-end type as
/// `Unknown`; it must remain compatible with an event payload because v0
/// source permits unannotated parameters and the callback value is still
/// supplied by the typed source event.
#[test]
fn analysis_connect_accepts_unannotated_hookable_payload() {
    let src = r#"
transactor Source
    observed : out event<uint<8>>
end transactor Source

scoreboard Sink
    hookable accept(v)
    end accept
end scoreboard Sink

testbench ConnectTb
    dut : Top
    source : Source passive
    sink : Sink

    connect
        source.observed -> sink.accept
    end connect
end testbench ConnectTb

impl ConnectTest for ConnectTb
    run
        log(info, "test")
    end run
end impl ConnectTest
"#;
    let prog = lower_src(src).expect("unannotated sink payload lowers");
    verify::verify_program(&prog).expect("unannotated sink payload verifies");
    assert_eq!(prog.testbenches[0].connects.len(), 1);
}

/// A reusable testbench may own analysis wiring directly. The endpoint paths
/// are rooted at its component fields, may descend through nested components,
/// and preserve declaration-order fanout in emitted subscription setup.
#[test]
fn testbench_owned_analysis_connects_lower_and_emit() {
    let src = r#"
transactor Source
    observed : out event<uint<8>>

    hookable publish(v: uint<8>)
        emit observed(v)
    end publish
end transactor Source

scoreboard Sink
    count : uint<32> default 0

    hookable accept(v: uint<8>)
        count = count + 1
    end accept
end scoreboard Sink

env Holder
    inner : Sink
end env Holder

testbench ConnectTb
    dut : Top
    source : Source passive
    direct : Sink
    nested : Holder

    connect
        source.observed -> direct.accept
        source.observed -> nested.inner.accept
    end connect
end testbench ConnectTb

impl TestbenchConnectTest for ConnectTb
    run
        source.publish(7)
        assert direct.count == 1
        assert nested.inner.count == 1
    end run
end impl TestbenchConnectTest
"#;
    let merged = merged_src(src);
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let tb = &prog.testbenches[0];
    assert_eq!(tb.connects.len(), 2);
    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits");
    assert!(cpp.contains("source.observed.push_back"), "{cpp}");
    assert!(cpp.contains("Sink_accept(direct, _t)"), "{cpp}");
    assert!(cpp.contains("Sink_accept(nested.inner, _t)"), "{cpp}");
}

/// Analysis-source transactors use the composite-component IR route, but
/// retain transactor binding semantics: a direct testbench field needs an
/// explicit mode, and both active and passive instances are valid.
#[test]
fn analysis_source_component_direct_mode_contract() {
    let src = r#"
transactor Relay
    observed : out event<uint<8>>
    count : uint<32> default 0

    hookable publish(v: uint<8>)
        count = count + 1
        emit observed(v)
    end publish

    when active
        active_only : uint<32> default 0
    end when
end transactor Relay

scoreboard Sink
    count : uint<32> default 0

    hookable accept(v: uint<8>)
        count = count + v
    end accept
end scoreboard Sink

testbench RelayTb
    dut : Top
    active_relay : Relay active
    passive_relay : Relay passive
    sink : Sink

    connect
        active_relay.observed -> sink.accept
        passive_relay.observed -> sink.accept
    end connect
end testbench RelayTb

impl RelayTest for RelayTb
    run
        active_relay.publish(2)
        passive_relay.publish(3)
        assert active_relay.count == 1
        assert passive_relay.count == 1
        assert sink.count == 5
    end run
end impl RelayTest
"#;
    let prog = lower_src(src).expect("active and passive analysis-source fields lower");
    verify::verify_program(&prog).expect("mode-correct program verifies");
    assert_eq!(prog.testbenches[0].component_fields.len(), 3);
    assert_eq!(
        prog.testbenches[0].component_fields[0].mode,
        Some(ir::ComponentInstanceMode::Active)
    );
    assert_eq!(
        prog.testbenches[0].component_fields[1].mode,
        Some(ir::ComponentInstanceMode::Passive)
    );
    assert_eq!(prog.testbenches[0].component_fields[2].mode, None);
    assert!(format!("{prog}").contains("component active_relay=c0 active"));
    assert!(format!("{prog}").contains("component passive_relay=c0 passive"));

    let mut malformed = prog.clone();
    malformed.testbenches[0].component_fields[0].mode = None;
    assert!(
        verify::verify_program(&malformed).is_err(),
        "the verifier must preserve direct transactor mode metadata"
    );

    let mode_less = src.replace("active_relay : Relay active", "active_relay : Relay");
    let mode_less_err = lower_src(&mode_less).expect_err("mode-less relay is invalid");
    let msg = assert_invalid(&mode_less_err);
    assert!(msg.contains("active_relay") && msg.contains("mode"), "{msg}");

    let structural = src.replace("active_relay : Relay active", "active_relay : Sink active");
    let structural_err = lower_src(&structural).expect_err("mode on scoreboard is invalid");
    let msg = assert_invalid(&structural_err);
    assert!(msg.contains("scoreboard") || msg.contains("Sink"), "{msg}");

    let passive_structural =
        src.replace("active_relay : Relay active", "active_relay : Sink passive");
    let structural_err =
        lower_src(&passive_structural).expect_err("passive mode on scoreboard is invalid");
    let msg = assert_invalid(&structural_err);
    assert!(msg.contains("Sink") && msg.contains("mode"), "{msg}");
}

/// A mixed-mode analysis source shares one schema, but `when active` members
/// are callable and registered only for the active binding.
#[test]
fn analysis_source_active_surface_is_mode_gated() {
    let src = r#"
transactor Relay
    observed : out event<uint<8>>
    active_ticks : uint<32> default 0

    hookable publish(v: uint<8>)
        emit observed(v)
    end publish

    when active
        hookable bump()
            active_ticks = active_ticks + 1
        end bump

        on 1 cycles
            active_ticks = active_ticks + 1
        end on
    end when
end transactor Relay

testbench RelayTb
    dut : Top
    active_relay : Relay active
    passive_relay : Relay passive
end testbench RelayTb

impl RelayTest for RelayTb
    run
        active_relay.bump()
        active_relay.publish(1)
        passive_relay.publish(2)
    end run
end impl RelayTest
"#;
    let prog = lower_src(src).expect("active surface lowers for mixed-mode source");
    verify::verify_program(&prog).expect("mixed-mode source verifies");
    let relay = prog
        .components
        .iter()
        .find(|component| component.name == "Relay")
        .expect("relay schema");
    assert_eq!(relay.methods.len(), 2, "one always-on and one active method");
    assert_eq!(relay.methods[0].activation, ir::Activation::Always);
    assert_eq!(relay.methods[1].activation, ir::Activation::ActiveOnly);
    assert_eq!(relay.periodic_handlers[0].activation, ir::Activation::ActiveOnly);

    let cpp = emit_cpp_src(src);
    assert!(cpp.contains("_per_active_relay_"), "{cpp}");
    assert!(!cpp.contains("_per_passive_relay_"), "{cpp}");

    let passive_call = src.replace("active_relay.bump()", "passive_relay.bump()");
    let err = lower_src(&passive_call).expect_err("passive active-only call is invalid");
    let msg = assert_invalid(&err);
    assert!(msg.contains("passive_relay") && msg.contains("active-only"), "{msg}");
}

/// A test-scope structural env root can carry inherited mode context. A nested
/// explicit passive mode wins over that context, so it cannot expose
/// active-only work. A mode on the reusable testbench field itself remains
/// invalid: structural fields are not transactor instances.
#[test]
fn analysis_source_nested_mode_inheritance_and_override() {
    let src = r#"
transactor Relay
    observed : out event<uint<8>>

    when active
        ticks : uint<32> default 0

        hookable bump()
            ticks = ticks + 1
        end bump

        on 1 cycles
            ticks = ticks + 1
        end on
    end when
end transactor Relay

env RelayEnv
    inherited : Relay
    overridden : Relay passive
end env RelayEnv

test RelayEnvTest
    let dut : Top
    let env : RelayEnv active
    run
        env.inherited.bump()
    end run
end test RelayEnvTest
"#;
    let prog = lower_src(src).expect("inherited mode lowers");
    verify::verify_program(&prog).expect("inherited mode verifies");
    assert_eq!(
        prog.testbenches[0].component_fields[0].mode,
        Some(ir::ComponentInstanceMode::Active)
    );
    let cpp = emit_cpp_src(src);
    assert!(cpp.contains("_per_env_inherited_"), "{cpp}");
    assert!(!cpp.contains("_per_env_overridden_"), "{cpp}");

    let passive_call = src.replace("env.inherited.bump()", "env.overridden.bump()");
    let err = lower_src(&passive_call).expect_err("override is passive");
    let msg = assert_invalid(&err);
    assert!(msg.contains("env.overridden") && msg.contains("active-only"), "{msg}");

    let passive_field = src.replace("env.inherited.bump()", "assert env.overridden.ticks == 0");
    let err = lower_src(&passive_field).expect_err("active-only field is unavailable on passive");
    let msg = assert_invalid(&err);
    assert!(msg.contains("env.overridden") && msg.contains("active-only"), "{msg}");

    let structural_field = src.replace(
        "test RelayEnvTest\n    let dut : Top\n    let env : RelayEnv active",
        "testbench RelayEnvTb\n    dut : Top\n    env : RelayEnv active\nend testbench RelayEnvTb\n\nimpl RelayEnvTest for RelayEnvTb",
    )
    .replace("end test RelayEnvTest", "end impl RelayEnvTest");
    let err = lower_src(&structural_field).expect_err("mode on structural field is invalid");
    let msg = assert_invalid(&err);
    assert!(msg.contains("mode") && msg.contains("RelayEnvTb.env"), "{msg}");

    let passive_structural_field = structural_field.replace("RelayEnv active", "RelayEnv passive");
    let err = lower_src(&passive_structural_field)
        .expect_err("passive mode on structural field is invalid");
    let msg = assert_invalid(&err);
    assert!(msg.contains("RelayEnvTb.env") && msg.contains("mode"), "{msg}");

    // An otherwise-unused mode-sensitive leaf must still be rejected. This
    // proves validation walks the composed instance tree, rather than only
    // noticing the missing mode when a run/check body reaches the leaf.
    let unresolved_leaf = src.replace("let env : RelayEnv active", "let env : RelayEnv").replace(
        "        env.inherited.bump()",
        "        assert 1 == 1",
    );
    let err = lower_src(&unresolved_leaf)
        .expect_err("a nested mode-sensitive transactor leaf needs an inherited or declared mode");
    let msg = assert_invalid(&err);
    assert!(msg.contains("env.inherited") && msg.contains("effective"), "{msg}");
}

#[test]
fn analysis_source_active_connects_and_self_access_are_gated() {
    let src = r#"
transactor Source
    when active
        observed : out event<uint<8>>
        ticks : uint<32> default 0
        hookable publish(v: uint<8>)
            emit observed(v)
        end publish
    end when
end transactor Source

scoreboard Sink
    count : uint<32> default 0
    hookable accept(v: uint<8>)
        count = count + v
    end accept
end scoreboard Sink

env SourceEnv
    source : Source
    sink : Sink
    connect
        source.observed -> sink.accept
    end connect
end env SourceEnv

test SourceEnvTest
    let dut : Top
    let env : SourceEnv passive
    run
        assert env.sink.count == 0
    end run
end test SourceEnvTest
"#;
    let prog = lower_src(src).expect("passive env may contain active-only source surface");
    verify::verify_program(&prog).expect("active-only connect metadata verifies");
    let cpp = emit_cpp_src(src);
    assert!(!cpp.contains("env.source.observed.push_back"), "{cpp}");

    let always_on = src.replace(
        "    when active\n",
        "    hookable illegal()\n        ticks = ticks + 1\n    end illegal\n\n    when active\n",
    );
    let err = lower_src(&always_on).expect_err("always-on member cannot access active-only state");
    let msg = assert_invalid(&err);
    assert!(msg.contains("always-on") && msg.contains("active-only"), "{msg}");
}

/// `connect` belongs to env/agent/testbench composition. The component
/// lowering route for analysis-source transactors must reject an active-only
/// declaration instead of silently dropping it while collecting the ordinary
/// and `when active` surfaces.
#[test]
fn analysis_source_transactor_active_connect_is_rejected() {
    let src = r#"
transactor Relay
    observed : out event<uint<8>>

    when active
        connect
            ignored.observed -> ignored.accept
        end connect
    end when
end transactor Relay

testbench RelayTb
    dut : Top
    relay : Relay passive
end testbench RelayTb

impl RelayTest for RelayTb
    run
        assert 1 == 1
    end run
end impl RelayTest
"#;
    let err = lower_src(src).expect_err("active transactor connects cannot be silently dropped");
    let msg = assert_unsupported(&err);
    assert!(msg.contains("when active") && msg.contains("connect") && msg.contains("Relay"), "{msg}");
}

/// A hook trigger subscribes to its target method during compilation. That
/// subscription must not make an active-only method reachable on a passive
/// analysis-source instance.
#[test]
fn passive_analysis_source_active_hook_trigger_is_rejected() {
    let src = r#"
covergroup RelayCov @(relay.bump(v) post)
    cp : cover v
        bins
            seen = [0..255]
        end bins
end covergroup RelayCov

transactor Relay
    when active
        hookable bump(v: uint<8>)
        end bump
    end when
end transactor Relay

testbench RelayTb
    dut : Top
    relay : Relay passive
    cov : RelayCov
end testbench RelayTb

impl RelayTest for RelayTb
    run
        assert 1 == 1
    end run
end impl RelayTest
"#;
    let err = lower_src(src).expect_err("a passive binding cannot subscribe to an active-only hook");
    let msg = assert_invalid(&err);
    assert!(msg.contains("relay.bump") && msg.contains("active-only") && msg.contains("passive"), "{msg}");

    // Emission remains defensive when a later pass corrupts a previously
    // valid active binding after hook subscriptions have been recorded.
    let active_src = src.replace("relay : Relay passive", "relay : Relay active");
    let mut malformed = lower_src(&active_src).expect("active hook binding lowers");
    malformed.testbenches[0].component_fields[0].mode = Some(ir::ComponentInstanceMode::Passive);
    let err = tbir::emit(
        &malformed,
        &merged_src(&active_src),
        &cpp_tb::EmitOpts::default(),
    )
    .expect_err("emission rejects malformed passive active-only hook subscription");
    assert!(err.to_string().contains("active-only") && err.to_string().contains("relay.bump"));
}

#[test]
fn testbench_owned_state_connect_dump_ir_snapshot() {
    let prog = lower_src(&fixture("testbench_owned_state_connect_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("testbench_owned_state_connect_dump_ir", format!("{prog}"));
}

/// The verifier protects TB-IR consumers from malformed testbench-owned
/// state/connect metadata introduced by a later pass.
#[test]
fn verifier_rejects_malformed_testbench_queue_and_connect_metadata() {
    let mut queue_prog = lower_src(&fixture("testbench_owned_state_connect_test.harc"))
        .expect("lowers");
    let run = queue_prog
        .tests
        .iter()
        .find(|test| test.name == "OwnedStateConnectTest")
        .expect("fixture has the queue exercise test")
        .run
        .index();
    let push = queue_prog.functions[run]
        .blocks
        .iter_mut()
        .flat_map(|b| b.stmts.iter_mut())
        .find_map(|stmt| match stmt {
            ir::Stmt::TbQueuePush { value, .. } => Some(value),
            _ => None,
        })
        .expect("fixture has queue push");
    *push = ir::Expr::Literal {
        value: 1,
        ty: ir::IrType::SInt(Some(8)),
    };
    assert!(verify::verify_program(&queue_prog).is_err());

    let mut connect_prog = lower_src(&fixture("testbench_owned_state_connect_test.harc"))
        .expect("lowers");
    connect_prog.testbenches[0].connects[0].sink_component = ir::ComponentId(999);
    assert!(verify::verify_program(&connect_prog).is_err());

    let mut query_prog = lower_src(&fixture("testbench_owned_state_connect_test.harc"))
        .expect("lowers");
    let run = query_prog
        .tests
        .iter()
        .find(|test| test.name == "OwnedStateConnectTest")
        .expect("fixture has the queue exercise test")
        .run
        .index();
    let cond = query_prog.functions[run]
        .blocks
        .iter_mut()
        .flat_map(|b| b.stmts.iter_mut())
        .find_map(|stmt| match stmt {
            ir::Stmt::AssertCheck { cond, .. } => Some(cond),
            _ => None,
        })
        .expect("fixture has assertion");
    *cond = ir::Expr::TbQueueQuery {
        field: "pending".to_string(),
        query: ir::ScoreboardQuery::Scalar {
            scalar: "pending".to_string(),
        },
    };
    assert!(verify::verify_program(&query_prog).is_err());
}

/// A reusable testbench owns typed FIFO state just like a scoreboard or
/// component, but the queue lifetime is the `_tb` host object's lifecycle.
/// Helpers and lifecycle phases must therefore resolve the same queue cell.
#[test]
fn testbench_owned_scalar_and_record_queues_lower_and_emit() {
    let src = r#"
struct PendingItem
    value : uint<32>
end struct PendingItem

testbench QueueTb
    dut     : Top
    pending : queue<uint<32>>
    records : queue<PendingItem>

    function enqueue(v: uint<32>)
        pending.push(v)
    end enqueue

    check
        assert pending.size() == 1
            else fail("expected one pending item")
        let got : uint<32> = pending.pop()
        assert got == 7
            else fail("wrong queued value")
        assert pending.empty()
            else fail("queue must drain")
    end check
end testbench QueueTb

impl QueueOwnerTest for QueueTb
    run
        enqueue(7)
    end run
end impl QueueOwnerTest
"#;
    let merged = merged_src(src);
    let prog = lower::lower_program(&merged).expect("testbench queues lower");
    verify::verify_program(&prog).expect("testbench queues verify");
    let tb = prog.testbench(prog.tests[0].testbench);
    assert_eq!(tb.queue_fields.len(), 2, "both queue fields are retained");
    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("queues emit");
    assert!(
        cpp.contains("harc_rt::HarcQueue<uint64_t> pending;"),
        "{cpp}"
    );
    assert!(
        cpp.contains("harc_rt::HarcQueue<PendingItem> records;"),
        "{cpp}"
    );
    assert!(cpp.contains("_tb.pending.push"), "{cpp}");
    assert!(cpp.contains("_tb.pending.pop()"), "{cpp}");
}

/// A queue pop defines its destination for every successor block, just like
/// scoreboard/component queue pops. This source puts the read after a branch
/// so the verifier must propagate the definition through its dataflow `gens`.
#[test]
fn testbench_queue_pop_definition_reaches_successor_blocks() {
    let src = r#"
testbench QueueTb
    dut : Top
    pending : queue<uint<32>>
end testbench QueueTb

impl QueuePopBranchTest for QueueTb
    run
        pending.push(7)
        let got = pending.pop()
        if pending.empty()
            assert got == 7
        end if
    end run
end impl QueuePopBranchTest
"#;
    let prog = lower_src(src).expect("cross-block queue pop lowers");
    verify::verify_program(&prog).expect("queue pop definition reaches successor blocks");
}

/// Agent subset: an `agent` composing an `event<T>` self-event, an
/// `on <ev>(arg)` handler (lowered as a one-param `ComponentMethod`),
/// and the heartbeat `idle_in` predicate. Locks the dump-ir: the
/// component schema with `(agent)` kind, the `comp_on_*` handler
/// function, the test-scope path `ComponentEmit(tagger.in_ev, ...)`,
/// and the `tagger.idle_in(N)` predicate expression.
#[test]
fn agent_on_handler_dump_ir_snapshot() {
    let prog = lower_src(&fixture("agent_on_handler_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("agent_on_handler_dump_ir", format!("{prog}"));
}

/// Regression: an agent that declares its `on <ev>` handler BEFORE a
/// `hookable` method. Pass 1 reserves FunctionIds methods-first then
/// on-handlers; the body-lowering pass must emit bodies in that same
/// FunctionId order (not source order), or `prog.functions` (indexed by
/// FunctionId) ends up non-monotonic and every later `prog.function(id)`
/// lookup is corrupt. `verify_program` walks the function table by id,
/// so it fails loudly on a mis-order.
#[test]
fn agent_on_handler_before_method_lowers_in_function_id_order() {
    let src = r#"
agent Mixed
    in_ev : event<uint<8>>
    seen  : uint<32> default 0

    on in_ev(t)
        seen = seen + 1
        bump()
    end on

    hookable bump()
        seen = seen + 1
    end bump
end agent Mixed

test MixedAgentTest
    let dut   : Top
    let mixed : Mixed
    run
        emit mixed.in_ev(7)
        assert mixed.seen == 2
            else fail("expected 2, got ${mixed.seen}")
    end run
end test MixedAgentTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    // Function ids must be densely 0..N in table order.
    for (i, f) in prog.functions.iter().enumerate() {
        assert_eq!(f.id.0 as usize, i, "function table out of FunctionId order");
    }
}

/// Composite-component testbench-FIELD binding: the same agent as
/// `agent_on_handler_test`, but bound as a `testbench` field
/// (`tagger : Tagger`) under an `impl ... for` body rather than a
/// test-scope `let`. The impl-for desugaring rewrites the field accesses
/// to `_tb.tagger.*`; the component machinery strips the `_tb` prefix so
/// `emit`/`idle_in`/field reads all resolve to the bare-name component
/// instance â€” IR identical to the test-scope-let form (tbir emits every
/// component at run scope regardless of binding shape).
#[test]
fn tb_field_agent_dump_ir_snapshot() {
    let prog = lower_src(&fixture("tb_field_agent_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let dump = format!("{prog}");
    // The agent bound as a testbench field still lowers as a component,
    // and every access resolved to the BARE `tagger` instance (the `_tb`
    // prefix stripped) â€” not `_tb.tagger`, not a DUT port.
    assert!(
        dump.contains("component c0 Tagger (agent)"),
        "expected the agent to lower as a component: {dump}"
    );
    assert!(
        dump.contains("ComponentEmit(tagger.in_ev"),
        "expected `emit` to resolve to the bare component instance: {dump}"
    );
    assert!(
        dump.contains("tagger.idle_in(4)"),
        "expected `idle_in` to resolve to the bare component instance: {dump}"
    );
    assert!(
        !dump.contains("_tb.tagger"),
        "the `_tb` prefix must be stripped from component accesses: {dump}"
    );
}

/// Locks the emitted tbir C++ for the agent fixture: the component
/// struct (event-callback vector + heartbeat stamps), the
/// `<Comp>_on_h<fid>` handler lambda, the on-handler `push_back`
/// registration (with the `_last_in_cycle` bump), the path-based
/// `emit` fan-out, and the `idle_in` predicate.
#[test]
fn agent_on_handler_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "agent_on_handler_emitted_cpp",
        emit_fixture_cpp("agent_on_handler_test.harc")
    );
}

/// Sequencer slice: a `sequencer` lowers as a composite component (the
/// analysis-source shape â€” `out event<T>` port + a hookable method that
/// `emit`s the generated stream), connected inside an env to a scoreboard
/// sink. Locks the dump-ir: the component schema with `(sequencer)` kind,
/// the literal-range dispatch loop emitting on the self event, and the
/// env's resolved `connect` edge (sequencer.dispatched -> sb.sink).
#[test]
fn sequencer_connect_dump_ir_snapshot() {
    let prog = lower_src(&fixture("sequencer_connect_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("sequencer_connect_dump_ir", format!("{prog}"));
}

/// Locks the emitted tbir C++ for the sequencer fixture: the sequencer
/// component struct (event-callback vector), the dispatch lambda, and the
/// env local + connect push_back wiring the sequencer stream to the sink.
#[test]
fn sequencer_connect_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "sequencer_connect_emitted_cpp",
        emit_fixture_cpp("sequencer_connect_test.harc")
    );
}

/// Source for the mode-VARIANT tests only: the same analysis-source
/// shape as `tests/fixtures/passive_analysis_monitor_test.harc`, reduced
/// to what the mode gate cares about, with `{mode}` substituted.
///
/// The shipped fixture is bound `passive`, so the `active` and
/// mode-less variants cannot come from it and need this parameterized
/// source. The `passive` case deliberately does NOT use this helper â€” it
/// loads the fixture, so what CI pins is the shape actually shipped
/// rather than a copy here that can drift out of sync with it.
fn passive_monitor_src(mode: &str) -> String {
    format!(
        r#"
struct LifeRec
    tag   : uint<8>
    stamp : uint<32>
end struct LifeRec

transactor LifecycleMonitor
    seen    : out event<uint<8>>
    starts  : uint<32> default 0
    history : queue<LifeRec>

    hookable note_start(tag: uint<8>)
        starts = starts + 1
        let r : LifeRec
        r.tag = tag
        r.stamp = starts
        history.push(r)
        emit seen(tag)
    end note_start

    hookable note_stop(tag: uint<8>)
        emit seen(tag)
    end note_stop
end transactor LifecycleMonitor

scoreboard LifeSink
    count : uint<32> default 0
    hookable accept(v: uint<8>)
        count = count + 1
    end accept
end scoreboard LifeSink

testbench LargeTb
    dut       : Top
    lifecycle : LifecycleMonitor{MODE}
    sink      : LifeSink

    connect
        lifecycle.seen -> sink.accept
    end connect
end testbench LargeTb

impl T for LargeTb
    run
        lifecycle.note_start(1)
        assert lifecycle.starts == 1 else fail("x")
    end run
end impl T
"#,
        MODE = mode
    )
}

/// harc#538 regression: a `passive` analysis-source component bound as a
/// testbench field lowers.
///
/// This is the shape that used to abort a whole production suite before
/// C++ emission ("TB-IR lowering does not support an `active`/`passive`
/// mode on composite-component testbench field"). `passive` is now an
/// ownership annotation on this path â€” it must NOT suppress the
/// component's methods or its `connect` fanout, which is exactly what
/// would happen if a passive analysis source were treated like a passive
/// BFM transactor (whose `when active` methods structurally do not
/// exist).
#[test]
fn passive_analysis_monitor_testbench_field_lowers() {
    let src = fixture("passive_analysis_monitor_test.harc");
    let prog = lower_src(&src).expect("a passive analysis-source component field lowers");
    verify::verify_program(&prog).expect("verifies");
    let dump = format!("{prog}");

    assert!(
        dump.contains("component c0 LifecycleMonitor (transactor)"),
        "monitor should lower as a composite component: {dump}"
    );
    // The annotation must not strip the always-on methods.
    assert!(
        dump.contains("method note_start") && dump.contains("method note_stop"),
        "passive must keep the monitor's always-on methods: {dump}"
    );
    // Nor its persistent state or record queue.
    assert!(
        dump.contains("field starts : scalar = 0")
            && dump.contains("field history : queue<LifeRec>"),
        "passive must keep the monitor's counters and record queue: {dump}"
    );
    // Nor the analysis fanout declared in `connect`.
    assert!(
        dump.contains("connect lifecycle.seen->sink.accept"),
        "passive must keep the connect fanout: {dump}"
    );

    // Carry it through CODEGEN too, not just lowering. `connect` fanout
    // surviving in the IR does not prove it survives emission, and the
    // end-to-end fixture that would catch that is Verilator-gated and
    // skips in CI â€” so without this the stated CI guarantee has a hole.
    let merged = merged_src(&src);
    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits");
    assert!(
        cpp.contains("LifeSink_accept("),
        "passive monitor's connect fanout must reach the emitted C++: {cpp}"
    );
    assert!(
        cpp.contains("LifecycleMonitor_note_start(") && cpp.contains("LifecycleMonitor_note_stop("),
        "passive monitor's always-on methods must reach the emitted C++"
    );
}

/// Full IR shape for the passive analysis monitor, so a regression on
/// this path is caught even where the targeted assertions above do not
/// look. Mirrors `testbench_owned_state_connect_dump_ir_snapshot`.
#[test]
fn passive_analysis_monitor_dump_ir_snapshot() {
    let prog = lower_src(&fixture("passive_analysis_monitor_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("passive_analysis_monitor_dump_ir", format!("{prog}"));
}

/// The same field with no mode at all also lowers.
///
/// This documents that an always-on analysis monitor needs no transactor mode.
/// The passive ownership annotation is now retained as diagnostic binding
/// metadata, but it selects no behavior: both forms keep the same methods,
/// state, and fanout.
#[test]
fn modeless_analysis_monitor_testbench_field_lowers() {
    let modeless_src = passive_monitor_src("");
    let passive_src = passive_monitor_src(" passive");
    let modeless = lower_src(&modeless_src).expect("a mode-less component field lowers");
    let passive = lower_src(&passive_src).expect("the passive ownership form lowers");
    verify::verify_program(&modeless).expect("modeless verifies");
    verify::verify_program(&passive).expect("passive verifies");
    assert_eq!(modeless.testbenches[0].component_fields[0].mode, None);

    let modeless_dump = format!("{modeless}");
    let passive_dump = format!("{passive}").replace(
        "component lifecycle=c0 passive",
        "component lifecycle=c0",
    );
    assert_eq!(modeless_dump, passive_dump, "only binding metadata may differ");

    let modeless_cpp = tbir::emit(
        &modeless,
        &merged_src(&modeless_src),
        &cpp_tb::EmitOpts::default(),
    )
    .expect("modeless emits");
    let passive_cpp = tbir::emit(
        &passive,
        &merged_src(&passive_src),
        &cpp_tb::EmitOpts::default(),
    )
    .expect("passive emits");
    assert_eq!(modeless_cpp, passive_cpp, "ownership metadata is behaviorally inert");
}

/// `active` on a composite-component field stays rejected, and is
/// rejected loudly rather than silently dropped.
///
/// `active`/`passive` is a transactor concept; on an analysis-source
/// component only the passive ownership annotation is meaningful.
/// Accepting `active` here would let a suite express a mode that has no
/// effect on emission.
#[test]
fn active_mode_on_analysis_monitor_field_is_rejected() {
    let err = lower_src(&passive_monitor_src(" active"))
        .expect_err("`active` on a composite-component field is out of subset");
    let msg = assert_invalid(&err);
    assert!(
        msg.contains("`active` mode on composite-component"),
        "diagnostic should name the offending mode and construct: {msg}"
    );
    assert!(
        msg.contains("LargeTb.lifecycle"),
        "diagnostic should locate the offending field: {msg}"
    );

    // The always-on source policy must win over the overlapping reactive
    // monitor classifier when an observation handler is also present.
    let reactive = passive_monitor_src(" active").replace(
        "end transactor LifecycleMonitor",
        "    on 1 cycles\n        starts = starts + 1\n    end on\nend transactor LifecycleMonitor",
    );
    let err = lower_src(&reactive).expect_err("active periodic analysis monitor is invalid");
    let msg = assert_invalid(&err);
    assert!(msg.contains("active") && msg.contains("LargeTb.lifecycle"), "{msg}");
}

/// A relay type with an always-on analysis surface and an active-only
/// half, for the three activation-gating seams below.
fn mode_relay_src() -> &'static str {
    r#"
transactor ModeRelay
    observed  : out event<uint<8>>
    published : uint<32> default 0

    hookable publish(v: uint<8>)
        published = published + 1
        emit observed(v)
    end publish

    when active
        acalls : uint<32> default 0
        aev    : out event<uint<8>>

        hookable activate()
            acalls = acalls + 1
            emit aev(1)
        end activate
    end when
end transactor ModeRelay

scoreboard ModeSink
    count : uint<32> default 0
    hookable accept(v: uint<8>)
        count = count + 1
    end accept
end scoreboard ModeSink
"#
}

/// An active-only member reached through a SELF sub-component field is
/// gated by that field's declared mode.
///
/// The self-relative arm of `as_component_method_call` returned without
/// any activation check, so `relay.activate()` inside an env holding
/// `relay : ModeRelay passive` lowered to a `ComponentCall` and emitted
/// `ModeRelay_activate(self.relay);` â€” active-only behavior running on a
/// passive instance. The path arm and the bare-self arm both checked;
/// only this one did not.
#[test]
fn active_only_method_through_a_passive_self_sub_component_is_rejected() {
    let src = |mode: &str| {
        format!(
            r#"{}
env Wrap
    relay : ModeRelay {mode}

    hookable kick()
        relay.activate()
    end kick
end env Wrap

test WrapTest
    let dut : Top
    let wrap : Wrap
    run
        wrap.kick()
    end run
end test WrapTest
"#,
            mode_relay_src()
        )
    };

    let err = lower_src(&src("passive")).expect_err("active-only through passive is invalid");
    let msg = assert_invalid(&err);
    assert!(
        msg.contains("active-only method `activate`") && msg.contains("relay"),
        "diagnostic should name the member and the passive field: {msg}"
    );

    // The control: the same body through an `active` field lowers, so
    // the rejection above is the MODE and not the self-relative shape.
    let prog = lower_src(&src("active")).expect("active sub-component lowers");
    verify::verify_program(&prog).expect("verifies");
}

/// A self-relative path reads its mode from the field it names, not from
/// the `"self"` root the emitter re-roots at.
///
/// `component_path_head` hands back `["self", <field>]` as the base
/// segments, and the mode lookup used `base_head[0]` â€” the literal
/// `"self"`, which names no test-scope binding. Reading an active-only
/// field of an `active` sub-component from its holder's own body was
/// therefore rejected with ``transactor `self` has no effective
/// active/passive mode``.
#[test]
fn self_relative_field_access_resolves_the_mode_from_its_own_binding() {
    let src = |mode: &str| {
        format!(
            r#"{}
env Wrap
    relay : ModeRelay {mode}
    seen  : uint<32> default 0

    hookable peek()
        seen = relay.acalls
    end peek
end env Wrap

test WrapTest
    let dut : Top
    let wrap : Wrap
    run
        wrap.peek()
    end run
end test WrapTest
"#,
            mode_relay_src()
        )
    };

    let prog = lower_src(&src("active")).expect("an active sub-component's field reads");
    verify::verify_program(&prog).expect("verifies");

    // And the gate still bites on the same access through `passive` â€”
    // the fix restores the resolution, it does not remove the check.
    let err = lower_src(&src("passive")).expect_err("active-only field through passive is invalid");
    let msg = assert_invalid(&err);
    assert!(msg.contains("acalls"), "{msg}");
}

/// A mode-less head whose descendant binding carries the mode resolves
/// through the descendant, rather than being rejected at the head.
///
/// `resolve_component_mode` rejected any transactor head with no mode of
/// its own, before walking the path at all. A reactive monitor is
/// legitimately mode-less, so `mon.sub.activate()` â€” where `sub : Relay
/// active` supplies the mode one segment down â€” failed with ``transactor
/// `mon` has no effective active/passive mode``, though the same tree
/// passes binding validation. The post-walk check on the resolved TARGET
/// is what actually answers the question.
#[test]
fn a_modeless_head_resolves_its_mode_from_the_descendant_binding() {
    let src = |sub_mode: &str| {
        format!(
            r#"
transactor Relay
    obs : out event<uint<8>>

    when active
        hookable activate()
            emit obs(1)
        end activate
    end when
end transactor Relay

transactor Mon
    seen  : out event<uint<8>>
    sub   : Relay {sub_mode}
    beats : uint<32> default 0

    on 1 cycles
        beats = beats + 1
    end on
end transactor Mon

test MonTest
    let dut : Top
    let mon : Mon
    run
        mon.sub.activate()
    end run
end test MonTest
"#
        )
    };

    let prog = lower_src(&src("active")).expect("the descendant's `active` supplies the mode");
    verify::verify_program(&prog).expect("verifies");

    let err = lower_src(&src("passive")).expect_err("a passive descendant still gates");
    let msg = assert_invalid(&err);
    assert!(msg.contains("activate"), "{msg}");
}

/// A consumer whose `on` handler is active-only cannot be bound
/// `passive`: nothing would subscribe to its `in event`, so the `emit`
/// runs its fan-out over an empty vector and the transaction vanishes.
///
/// The emitted C++ for the rejected form was the whole diagnosis â€” a
/// `for (auto& _s : t.req) _s(1);` loop with no `push_back` anywhere.
/// The analysis-source mode gates accept `passive` and run ahead of the
/// event-driven one, and a consumer that also declares an `out event` is
/// an analysis source, so they claimed the type first; this gate runs
/// before them.
///
/// The `out event` axis is covered both ways because it decides which
/// gate would otherwise claim the type, and the always-on rows are the
/// control: an `on` handler in the ordinary body registers for EVERY
/// mode, so `passive` keeps working there and must not be swept up.
#[test]
fn an_active_only_consumer_rejects_every_mode_but_active() {
    let consumer = |out_event: bool, active_only: bool, mode: &str| {
        let obs = if out_event {
            "    obs : out event<uint<8>>\n"
        } else {
            ""
        };
        let handler = "on req(v)\n            n = n + 1\n        end on";
        let body = if active_only {
            format!("    when active\n        {handler}\n    end when\n")
        } else {
            format!("    {handler}\n")
        };
        format!(
            r#"
transactor T
    req : in event<uint<8>>
{obs}    n   : uint<32> default 0

{body}end transactor T

testbench Tb
    dut : Top
    t   : T {mode}
end testbench Tb

impl MTest for Tb
    run
        emit t.req(1)
    end run
end impl MTest
"#
        )
    };

    for out_event in [false, true] {
        // Active-only handler: `active` is the only mode that subscribes.
        let prog = lower_src(&consumer(out_event, true, "active"))
            .unwrap_or_else(|e| panic!("out_event={out_event}: active lowers: {e:?}"));
        verify::verify_program(&prog).expect("verifies");
        let src = consumer(out_event, true, "active");
        let cpp = tbir::emit(&prog, &merged_src(&src), &cpp_tb::EmitOpts::default())
            .expect("emits");
        assert!(
            cpp.contains("req.push_back"),
            "out_event={out_event}: the active instance subscribes"
        );

        let err = lower_src(&consumer(out_event, true, "passive"))
            .expect_err("a passive active-only consumer must be rejected");
        let msg = assert_unsupported(&err);
        assert!(
            msg.contains("when active") && msg.contains("no subscriber"),
            "out_event={out_event}: the diagnostic should say why: {msg}"
        );

        // Control: an always-on handler registers on every mode, so
        // `passive` stays legal and stays wired.
        let src = consumer(out_event, false, "passive");
        let prog = lower_src(&src)
            .unwrap_or_else(|e| panic!("out_event={out_event}: always-on passive lowers: {e:?}"));
        let cpp = tbir::emit(&prog, &merged_src(&src), &cpp_tb::EmitOpts::default())
            .expect("emits");
        assert!(
            cpp.contains("req.push_back"),
            "out_event={out_event}: an always-on handler wires a passive instance"
        );
    }
}

/// A testbench-owned `connect` edge must be valid for the endpoint modes.
///
/// Silently dropping a declared edge during emission leaves a valid-looking
/// but incomplete testbench, so reject the statically known mismatch while
/// lowering instead.
#[test]
fn a_testbench_connect_edge_rejects_a_mode_disabled_endpoint() {
    let src = |mode: &str| {
        format!(
            r#"{}
testbench ModeTb
    dut   : Top
    relay : ModeRelay {mode}
    sink  : ModeSink

    connect
        relay.aev -> sink.accept
    end connect
end testbench ModeTb

impl ModeConnectTest for ModeTb
    run
        relay.publish(1)
    end run
end impl ModeConnectTest
"#,
            mode_relay_src()
        )
    };

    let passive_src = src("passive");
    let err = lower_src(&passive_src).expect_err("mode-disabled wiring must be rejected");
    let msg = assert_invalid(&err);
    assert!(
        msg.contains("connect") && msg.contains("relay.aev") && msg.contains("mode-disabled"),
        "{msg}"
    );

    // The anchor: the same edge on an `active` instance IS wired, so the
    // assertion above is the mode gate and not a renamed field.
    let active_src = src("active");
    let mut active = lower_src(&active_src).expect("lowers");
    let active_cpp = tbir::emit(
        &active,
        &merged_src(&active_src),
        &cpp_tb::EmitOpts::default(),
    )
    .expect("emits");
    assert!(
        active_cpp.contains("aev.push_back"),
        "an active instance keeps its connect registration"
    );

    // Verification independently owns the invariant for transformed or
    // deserialized IR that did not come directly from lowering.
    active.testbenches[0]
        .component_fields
        .iter_mut()
        .find(|field| field.field == "relay")
        .expect("relay binding exists")
        .mode = Some(ir::ComponentInstanceMode::Passive);
    assert!(
        verify::verify_program(&active).is_err(),
        "the verifier must reject a mode-disabled connect endpoint"
    );
}

/// The always-on analysis-monitor rule applies at every component binding
/// seam, not just a reusable testbench field. A structural root may carry an
/// inherited mode for a mode-sensitive descendant, but an explicit `active`
/// annotation on the monitor itself is meaningless and invalid.
#[test]
fn active_mode_on_always_on_analysis_monitor_is_rejected_at_all_binding_seams() {
    let direct = r#"
transactor Monitor
    observed : out event<uint<8>>
end transactor Monitor

test MonitorTest
    let dut : Top
    let monitor : Monitor active
    run
        assert 1 == 1
    end run
end test MonitorTest
"#;
    let err = lower_src(direct).expect_err("test-scope active monitor is invalid");
    let msg = assert_invalid(&err);
    assert!(msg.contains("monitor") && msg.contains("active"), "{msg}");

    let nested = r#"
transactor Monitor
    observed : out event<uint<8>>
end transactor Monitor

env Wrapper
    monitor : Monitor active
end env Wrapper

test MonitorTest
    let dut : Top
    let wrapper : Wrapper
    run
        assert 1 == 1
    end run
end test MonitorTest
"#;
    let err = lower_src(nested).expect_err("nested active monitor is invalid");
    let msg = assert_invalid(&err);
    assert!(msg.contains("wrapper.monitor") && msg.contains("active"), "{msg}");
}

/// A method-bearing scoreboard lowers as a composite component
/// (per-instance state materialized). Since the testbench-field-binding
/// slice it binds BOTH as a test-scope `let` AND as a `testbench` FIELD
/// (`sb : Sb`). The impl-for desugaring rewrites the field access to
/// `_tb.sb.n`; the component machinery strips the `_tb` prefix so the
/// access resolves through `component_fields` by the bare name `sb`,
/// identical to a test-scope-let binding (and never mis-lowered to a DUT
/// module type).
#[test]
fn scoreboard_method_testbench_field_lowers() {
    let src = r#"
scoreboard Sb
    n : uint<32> default 0
    hookable bump()
        n = n + 1
    end bump
end scoreboard Sb

testbench Tb
    dut : Top
    sb  : Sb
end testbench Tb

impl T for Tb
    run
        assert sb.n == 0 else fail("x")
    end run
end impl T
"#;
    let prog = lower_src(src).expect("composite-component testbench field lowers");
    verify::verify_program(&prog).expect("verifies");
    // The `sb` field is registered as a component instance, and the
    // `sb.n` read resolved to a bare-name `ComponentField` access (the
    // `_tb` prefix stripped) rather than a DUT port or a tb-struct field.
    let dump = format!("{prog}");
    assert!(
        dump.contains("component c0 Sb (scoreboard)"),
        "expected the scoreboard to lower as a component: {dump}"
    );
    assert!(
        dump.contains("sb.n == 0"),
        "expected the `sb.n` read to resolve through the component path: {dump}"
    );
}

/// A `queue<Struct>` element on a plain data-only scoreboard lowers to a
/// record-element queue (`QueueElem::Record`), mirroring v1's
/// `HarcQueue<Struct>` â€” reusing the same record-queue seam as the
/// composite-component path. push/pop/size of the struct element work,
/// and a `let s : Pkt = sb.q.pop()` types the popped local as the record.
#[test]
fn scoreboard_struct_queue_lowers_to_record_element() {
    let src = r#"
struct Pkt
    a : uint<8>
end struct Pkt

scoreboard Sb
    q : queue<Pkt>
end scoreboard Sb

testbench Tb
    dut : Top
    sb  : Sb
end testbench Tb

impl T for Tb
    run
        let p : Pkt
        p.a = 7
        sb.q.push(p)
        assert sb.q.size() == 1 else fail("x")
        let got : Pkt = sb.q.pop()
        assert got.a == 7 else fail("y")
        assert sb.q.empty() else fail("z")
    end run
end impl T
"#;
    let prog = lower_src(src).expect("queue<struct> on a data-only scoreboard lowers");
    verify::verify_program(&prog).expect("verifies");
    // The scoreboard stays on the data-only `ScoreboardSchema` path (a
    // method-less board never routes to the composite-component table),
    // and its queue field carries a record element (not a scalar
    // fallback), mirroring v1's `HarcQueue<Pkt>`.
    let sb = prog
        .scoreboards
        .iter()
        .find(|s| s.name == "Sb")
        .expect("data-only scoreboard `Sb` is present");
    let q = sb.field("q").expect("queue field `q`");
    match &q.kind {
        ir::ScoreboardFieldKind::Queue {
            elem: ir::QueueElem::Record(_),
        } => {}
        other => panic!("expected a record-element queue, got {other:?}"),
    }
}

/// A record let inside a loop re-runs the defaults each iteration:
/// the lowering must place a `RecordInit` at the let site (loop
/// body), not rely on the hoisted declaration's one-time initializer.
#[test]
fn record_let_in_loop_reinitializes() {
    let src = r#"
transaction Req
    addr : uint<32> default 5
end transaction Req

test ReinitTest
    let dut : Top
    run
        for i in 0 .. 2
            let t : Req
            t.addr = t.addr + i
        end for
    end run
end test ReinitTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let run = prog.function(prog.tests[0].run);
    // The loop body block carries RecordInit then RecordFieldWrite.
    let body = run
        .blocks
        .iter()
        .find(|b| {
            b.stmts
                .iter()
                .any(|s| matches!(s, ir::Stmt::RecordInit(..)))
        })
        .expect("a block carries RecordInit");
    let init_pos = body
        .stmts
        .iter()
        .position(|s| matches!(s, ir::Stmt::RecordInit(..)))
        .unwrap();
    let write_pos = body
        .stmts
        .iter()
        .position(|s| matches!(s, ir::Stmt::RecordFieldWrite { .. }))
        .expect("field write lowered");
    assert!(init_pos < write_pos, "init precedes the write:\n{run}");
    // The body block is a loop participant (reachable from the header
    // branch), so the init re-runs per iteration by construction.
    assert!(
        matches!(prog.records[0].fields[0].default, Some(5)),
        "default carried into the schema"
    );
}

/// A `struct` declaration lowers into the same records table as a
/// transaction â€” a `let s : S` default-constructs and `s.field`
/// reads/writes reuse the record machinery (no struct-specific IR).
#[test]
fn struct_lowers_as_value_record() {
    let src = r#"
struct Pkt
    flag  : bool    default true
    count : uint<16> default 7
    spare : uint<8>
end struct Pkt

test StructTest
    let dut : Top
    run
        let p : Pkt
        p.count = p.count + 1
        assert p.flag else fail("flag")
    end run
end test StructTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    // One record schema, fields in declaration order with defaults.
    assert_eq!(prog.records.len(), 1, "struct â†’ one record schema");
    let rec = &prog.records[0];
    assert_eq!(rec.name, "Pkt");
    assert_eq!(rec.fields.len(), 3, "fields not double-counted from body");
    assert_eq!(rec.fields[0].default, Some(1), "bool true â†’ 1");
    assert_eq!(rec.fields[1].default, Some(7));
    assert_eq!(rec.fields[2].default, None, "undefaulted field re-zeroes");
    // The body carries RecordInit + a RecordFieldWrite for `p.count`.
    let run = prog.function(prog.tests[0].run);
    assert!(
        run.blocks.iter().any(|b| b
            .stmts
            .iter()
            .any(|s| matches!(s, ir::Stmt::RecordInit(..)))),
        "struct local default-constructs via RecordInit:\n{run}"
    );
}

/// A non-scalar struct field (here a `Vec`) is out of the scalar-only
/// subset: rejected at the field, never mis-lowered. (This is the
/// residual blocker for the `tlm_pairing_arch_burst_*` fixtures.)
#[test]
fn struct_vec_field_lowers_and_indexes() {
    // A `Vec<T, N>` struct field is the one aggregate the record subset
    // lowers (v1's `std::array<T, N>` member); element read/write
    // (`r.data[i]`) lower to indexed `RecordField` / `RecordFieldWrite`.
    let src = r#"
struct Resp
    data : Vec<uint<32>, 4>
    len : uint<3>
end struct Resp

test StructVecTest
    let dut : Top
    run
        let r : Resp
        r.data[0] = 17
        let v = r.data[0]
        assert v == 17 else fail("data[0]")
    end run
end test StructVecTest
"#;
    let prog = lower_src(src).expect("Vec struct field lowers");
    let dump = format!("{prog}");
    assert!(
        dump.contains("data : Vec<uint<32>, 4>"),
        "names the Vec field: {dump}"
    );
    assert!(
        dump.contains("RecordFieldWrite(%r.data[0]"),
        "indexed element write: {dump}"
    );
    assert!(dump.contains("%r.data[0]"), "indexed element read: {dump}");
}

/// A record field typed as a NON-fixed aggregate (`Vec` with a
/// widthless element, a nested record, a list) is still rejected â€”
/// only fixed `Vec<scalar, N>` lowers.
#[test]
fn struct_non_scalar_field_is_rejected() {
    let src = r#"
struct Resp
    data : Vec<uint, 4>
end struct Resp

test StructVecTest
    let dut : Top
    run
        let r : Resp
    end run
end test StructVecTest
"#;
    let err = lower_src(src).expect_err("widthless Vec element must be rejected");
    let msg = assert_unsupported(&err);
    assert!(msg.contains("struct field"), "names the field: {msg}");
    assert!(msg.contains("non-scalar"), "names the reason: {msg}");
}

/// A NESTED struct whose inner leaf is a genuinely unsupported type (a
/// `Vec` with a widthless element â€” no defined packed width) is still
/// rejected. Nested structs are now lowered, but only when every leaf is
/// itself representable. The diagnostic must name the offending leaf
/// (`Inner.bad`), not just say "non-scalar".
#[test]
fn nested_struct_with_unsupported_leaf_is_rejected() {
    let src = r#"
struct Inner
    bad : Vec<uint, 4>
end struct Inner

struct Outer
    inner : Inner
end struct Outer

test NestedBadLeafTest
    let dut : Top
    run
        let o : Outer
    end run
end test NestedBadLeafTest
"#;
    let err = lower_src(src).expect_err("unsupported nested leaf must be rejected");
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("Inner.bad"),
        "names the offending leaf path: {msg}"
    );
    assert!(msg.contains("non-scalar"), "names the reason: {msg}");
}

/// A fixed `Vec<Record, N>` struct field lowers (harc#522): the schema
/// carries the element record with the vec length, and an UNUSED
/// compatible declaration must not block an otherwise unrelated test.
#[test]
fn struct_vec_of_record_field_lowers_unused() {
    let src = r#"
struct Entry
    tag : uint<8>
    value : uint<32>
end struct Entry

struct EntryTable
    entries : Vec<Entry, 4>
end struct EntryTable

test VecOfStructUnusedTest
    let dut : Top
    run
        log(info, "Vec-of-struct declaration compiled")
    end run
end test VecOfStructUnusedTest
"#;
    let prog = lower_src(src).expect("unused Vec<Record, N> declaration lowers");
    let dump = format!("{prog}");
    assert!(
        dump.contains("entries : Vec<record(r0), 4>"),
        "schema carries the record-element Vec field: {dump}"
    );
}

/// Element-selected field access on a `Vec<Record, N>` field
/// (`tbl.entries[i].tag`) lowers on both the write and the read side,
/// carrying the mid-chain element index (harc#522).
#[test]
fn vec_of_record_element_field_access_lowers() {
    let src = r#"
struct Entry
    tag : uint<8>
    value : uint<32>
end struct Entry

struct EntryTable
    entries : Vec<Entry, 4>
end struct EntryTable

test VecOfStructAccessTest
    let dut : Top
    run
        let tbl : EntryTable
        tbl.entries[1].tag = 0x5A
        tbl.entries[1].value = 1234
        let v = tbl.entries[1].value
        assert v == 1234 else fail("value: ${v}")
        assert tbl.entries[1].tag == 0x5A else fail("tag")
        assert tbl.entries[0].tag == 0 else fail("default tag")
    end run
end test VecOfStructAccessTest
"#;
    let prog = lower_src(src).expect("entries[i].field access lowers");
    verify::verify_program(&prog).expect("verifies");
    let dump = format!("{prog}");
    assert!(
        dump.contains("RecordFieldWrite(%tbl.entries[1].tag, 90)"),
        "mid-indexed element field write: {dump}"
    );
    assert!(
        dump.contains("%tbl.entries[1].value"),
        "mid-indexed element field read: {dump}"
    );
}

/// Whole-element copies (`tbl.entries[i] = e`, `let e : Entry =
/// tbl.entries[i]`) and the whole-vector copy (`b.entries = a.entries`)
/// preserve record value semantics (harc#522).
#[test]
fn vec_of_record_element_and_whole_vector_copies_lower() {
    let src = r#"
struct Entry
    tag : uint<8>
    value : uint<32>
end struct Entry

struct EntryTable
    entries : Vec<Entry, 4>
end struct EntryTable

test VecOfStructCopyTest
    let dut : Top
    run
        let a : EntryTable
        let e : Entry
        e.tag = 7
        e.value = 42
        a.entries[2] = e
        let e2 : Entry = a.entries[2]
        assert e2.value == 42 else fail("element copy out")
        let b : EntryTable
        b.entries = a.entries
        assert b.entries[2].tag == 7 else fail("whole-vector copy")
    end run
end test VecOfStructCopyTest
"#;
    let prog = lower_src(src).expect("element and whole-vector copies lower");
    verify::verify_program(&prog).expect("verifies");
    let dump = format!("{prog}");
    assert!(
        dump.contains("RecordFieldWrite(%a.entries[2], %e)"),
        "whole-element store: {dump}"
    );
    assert!(
        dump.contains("RecordFieldWrite(%b.entries, %a.entries)"),
        "whole-vector copy: {dump}"
    );
}

/// The tbir C++ emission for a `Vec<Record, N>` field: an
/// `std::array<Entry, N>` member (value-initialized so element defaults
/// run), and direct member-chain element access (`tbl.entries[1].tag`).
#[test]
fn vec_of_record_field_cpp_shape() {
    let src = r#"
struct Entry
    tag : uint<8>
    value : uint<32>
end struct Entry

struct EntryTable
    entries : Vec<Entry, 4>
end struct EntryTable

test VecOfStructCppTest
    let dut : Top
    run
        let tbl : EntryTable
        tbl.entries[1].tag = 90
        let v = tbl.entries[1].tag
        assert v == 90 else fail("tag")
    end run
end test VecOfStructCppTest
"#;
    let cpp = emit_cpp_src(src);
    assert!(
        cpp.contains("std::array<Entry, 4> entries = {};"),
        "record-element array member: {cpp}"
    );
    assert!(
        cpp.contains("tbl.entries[1].tag = 90;"),
        "direct element field store: {cpp}"
    );
}

/// A `Vec<Record, N>` whose element record contains a genuinely
/// unsupported leaf is rejected via the ELEMENT record's own lowering,
/// and the diagnostic names the complete offending leaf path
/// (`Inner.bad`), not the vector field (harc#522 acceptance).
#[test]
fn vec_of_record_with_unsupported_leaf_names_leaf_path() {
    let src = r#"
struct Inner
    bad : Vec<uint, 4>
end struct Inner

struct Outer
    entries : Vec<Inner, 2>
end struct Outer

test VecOfBadRecordTest
    let dut : Top
    run
        let o : Outer
    end run
end test VecOfBadRecordTest
"#;
    let err = lower_src(src).expect_err("unsupported element leaf must be rejected");
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("Inner.bad"),
        "names the offending leaf path: {msg}"
    );
}

/// Traversing a `Vec<Record, N>` field WITHOUT an element index
/// (`tbl.entries.tag`) is rejected with a message that points at
/// element selection, not silently mis-lowered.
#[test]
fn vec_of_record_traversal_without_index_is_rejected() {
    let src = r#"
struct Entry
    tag : uint<8>
end struct Entry

struct EntryTable
    entries : Vec<Entry, 4>
end struct EntryTable

test VecOfStructNoIndexTest
    let dut : Top
    run
        let tbl : EntryTable
        tbl.entries.tag = 1
    end run
end test VecOfStructNoIndexTest
"#;
    let err = lower_src(src).expect_err("un-indexed Vec traversal must be rejected");
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("without an element index"),
        "names the missing index: {msg}"
    );
}

/// An element store with a non-matching RHS (`tbl.entries[i] = 5`) is
/// rejected precisely â€” never emitted as `Entry = <scalar>`.
#[test]
fn vec_of_record_element_write_with_scalar_rhs_is_rejected() {
    let src = r#"
struct Entry
    tag : uint<8>
end struct Entry

struct EntryTable
    entries : Vec<Entry, 4>
end struct EntryTable

test VecOfStructBadElemWriteTest
    let dut : Top
    run
        let tbl : EntryTable
        tbl.entries[0] = 5
    end run
end test VecOfStructBadElemWriteTest
"#;
    let err = lower_src(src).expect_err("scalar RHS on a record element must be rejected");
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("element write") && msg.contains("non-`Entry` RHS"),
        "names the shape mismatch: {msg}"
    );
}

/// A self-referential by-value struct (`Node { next : Node }`) is
/// STRUCTURALLY INVALID in every backend â€” the generated C++ struct is
/// infinitely sized, and v1 codegen stack-overflows on it. So it must be
/// rejected as `LowerError::Invalid` (NOT `Unsupported`), and the message
/// must NOT suggest `--codegen v1` (that path crashes). Regression guard
/// for the diagnostic-routing fix.
#[test]
fn self_recursive_record_is_rejected_as_invalid_not_v1_suggestion() {
    let src = r#"
struct Node
    next : Node
    val  : uint<8>
end struct Node

test SelfCycleTest
    let dut : Top
    run
        let n : Node
    end run
end test SelfCycleTest
"#;
    let err = lower_src(src).expect_err("self-recursive record must be rejected");
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "must be Invalid, not Unsupported: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        !msg.contains("--codegen v1"),
        "cyclic-record error must NOT suggest --codegen v1 (v1 crashes): {msg}"
    );
    assert!(msg.contains("Node"), "names the cyclic record: {msg}");
}

/// TWO mid-chain element selections in one access chain
/// (`outer[i].entries[j].tag` â€” a `Vec<Table, N>` whose element record
/// itself holds a `Vec<Entry, M>`): the position-tagged `mid_indices`
/// machinery is generic over chain depth, and emission interleaves each
/// `[idx]` after its own segment (harc#522 review follow-up).
#[test]
fn vec_of_record_double_mid_index_chain_lowers() {
    let src = r#"
struct Entry
    tag : uint<8>
end struct Entry

struct Table
    entries : Vec<Entry, 4>
end struct Table

struct Bank
    tables : Vec<Table, 2>
end struct Bank

test DoubleMidIndexTest
    let dut : Top
    run
        let bank : Bank
        bank.tables[1].entries[2].tag = 0x77
        let v = bank.tables[1].entries[2].tag
        assert v == 0x77 else fail("tag: 0x${v:02x}")
        assert bank.tables[0].entries[2].tag == 0 else fail("neighbor table clobbered")
        assert bank.tables[1].entries[3].tag == 0 else fail("neighbor entry clobbered")
    end run
end test DoubleMidIndexTest
"#;
    let prog = lower_src(src).expect("double-mid-index chain lowers");
    verify::verify_program(&prog).expect("verifies");
    let dump = format!("{prog}");
    assert!(
        dump.contains("RecordFieldWrite(%bank.tables[1].entries[2].tag, 119)"),
        "both mid indices render at their own segments: {dump}"
    );
    let cpp = emit_cpp_src(src);
    assert!(
        cpp.contains("bank.tables[1].entries[2].tag = 119;"),
        "direct double-indexed member chain in C++: {cpp}"
    );
}

/// Element access rooted at a TESTBENCH record field (`tbl.entries[i].tag`
/// inside an impl-form body, desugared to `_tb.tblâ€¦`): the chain resolver's
/// `field_start` offset must not shift the mid-index position (harc#522).
#[test]
fn vec_of_record_access_through_testbench_field_lowers() {
    let src = r#"
struct Entry
    tag : uint<8>
end struct Entry

struct EntryTable
    entries : Vec<Entry, 4>
end struct EntryTable

testbench VecTbFieldTb
    dut : Top
    tbl : EntryTable
end testbench VecTbFieldTb

impl VecTbFieldTest for VecTbFieldTb
    run
        tbl.entries[1].tag = 42
        assert tbl.entries[1].tag == 42 else fail("tb-field element access")
    end run
end impl VecTbFieldTest
"#;
    let prog = lower_src(src).expect("testbench-field-rooted element access lowers");
    verify::verify_program(&prog).expect("verifies");
    let dump = format!("{prog}");
    assert!(
        dump.contains(".entries[1].tag, 42"),
        "mid-indexed write through the tb record field: {dump}"
    );
}

/// A LITERAL element index that is statically out of range is rejected at
/// lowering as `Invalid` (both backends would otherwise emit `std::array`
/// UB â€” v1 included, so the message must NOT suggest `--codegen v1`).
/// Covers all three index sites: leaf read, leaf write, and a mid-chain
/// `Vec<Record, N>` selection. A runtime index stays unchecked (runtime
/// range behavior is the backends', as before).
#[test]
fn literal_out_of_range_vec_index_is_rejected() {
    let cases = [
        // Leaf read on a scalar-element Vec.
        ("let v = tbl.entries[0].data[2]", "EntryTable.entries.data"),
        // Leaf write on a scalar-element Vec.
        ("tbl.entries[0].data[9] = 1", "EntryTable.entries.data"),
        // Mid-chain selection on the record-element Vec.
        ("tbl.entries[4].tag = 1", "EntryTable.entries"),
    ];
    for (stmt, dotted) in cases {
        let src = format!(
            r#"
struct Entry
    tag  : uint<8>
    data : Vec<uint<16>, 2>
end struct Entry

struct EntryTable
    entries : Vec<Entry, 4>
end struct EntryTable

test OobIndexTest
    let dut : Top
    run
        let tbl : EntryTable
        {stmt}
    end run
end test OobIndexTest
"#
        );
        let err = lower_src(&src).expect_err("literal OOB index must be rejected");
        assert!(
            matches!(err, lower::LowerError::Invalid(_)),
            "must be Invalid, not Unsupported ({stmt}): {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("out of range") && msg.contains(dotted),
            "names the range violation and field ({stmt}): {msg}"
        );
        assert!(
            !msg.contains("--codegen v1"),
            "OOB error must NOT suggest --codegen v1 (v1 is UB there): {msg}"
        );
    }
}

/// A record containing a `Vec` of ITSELF (`Node { kids : Vec<Node, 2> }`)
/// is the same infinitely-sized by-value cycle through the array member â€”
/// the cycle check follows record-element `Vec` edges too (harc#522).
#[test]
fn vec_of_self_record_is_rejected_as_invalid() {
    let src = r#"
struct Node
    kids : Vec<Node, 2>
    val  : uint<8>
end struct Node

test VecSelfCycleTest
    let dut : Top
    run
        let n : Node
    end run
end test VecSelfCycleTest
"#;
    let err = lower_src(src).expect_err("Vec-of-self record must be rejected");
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "must be Invalid, not Unsupported: {err:?}"
    );
    assert!(
        !err.to_string().contains("--codegen v1"),
        "cyclic-record error must NOT suggest --codegen v1: {err}"
    );
}

/// Mutual recursion (`A { b : B }`, `B { a : A }`) is likewise a
/// by-value cycle and rejected as `Invalid`.
#[test]
fn mutually_recursive_records_are_rejected_as_invalid() {
    let src = r#"
struct A
    b : B
end struct A

struct B
    a : A
end struct B

test MutualCycleTest
    let dut : Top
    run
        let x : A
    end run
end test MutualCycleTest
"#;
    let err = lower_src(src).expect_err("mutually-recursive records must be rejected");
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "must be Invalid: {err:?}"
    );
    assert!(
        !err.to_string().contains("--codegen v1"),
        "cyclic-record error must NOT suggest --codegen v1: {err}"
    );
}

/// A DIAMOND (shared but acyclic) nesting â€” `Outer { a : Mid, b : Mid }`
/// where `Mid` is reached twice but the graph has no cycle â€” must NOT be
/// false-rejected by the cycle check. This locks the gray/black DFS's
/// handling of a Black (already-finished) node on a second visit.
#[test]
fn diamond_shared_record_is_accepted() {
    let src = r#"
struct Mid
    x : uint<8>
end struct Mid

struct Outer
    a : Mid
    b : Mid
end struct Outer

test DiamondTest
    let dut : Top
    run
        let o : Outer
        o.a.x = 3
        o.b.x = 4
        assert o.a.x == 3 else fail("diamond a lost")
        assert o.b.x == 4 else fail("diamond b lost")
    end run
end test DiamondTest
"#;
    lower_src(src).expect("diamond (shared acyclic) record must lower cleanly");
}

/// A whole-`Vec` record-field READ in scalar position (here a format
/// arg) must be REJECTED with a structured diagnostic â€” NOT lowered into
/// `harc_printf_ll(r.data)`, which miscompiles as a raw clang error
/// ("cannot convert std::array to long long"). Regression guard for the
/// over-broad rejection removal in #443.
#[test]
fn whole_vec_field_read_in_format_arg_is_rejected() {
    let src = r#"
struct Bundle
    data : Vec<uint<32>, 4>
end struct Bundle

test WholeVecReadTest
    let dut : Top
    run
        let r : Bundle
        r.data[0] = 1
        log(info, "${r.data}")
    end run
end test WholeVecReadTest
"#;
    let err = lower_src(src).expect_err("whole-Vec field read must be rejected");
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("whole-`Vec` read of record field"),
        "names the read: {msg}"
    );
    assert!(
        msg.contains("Bundle.data"),
        "names the offending field: {msg}"
    );
}

/// A whole-`Vec` record-field READ compared in an `assert` must be
/// rejected too (it only "worked" by luck before this guard).
#[test]
fn whole_vec_field_read_in_assert_is_rejected() {
    let src = r#"
struct Bundle
    data : Vec<uint<32>, 4>
end struct Bundle

test WholeVecAssertTest
    let dut : Top
    run
        let r : Bundle
        r.data[0] = 1
        assert r.data == r.data else fail("nope")
    end run
end test WholeVecAssertTest
"#;
    let err = lower_src(src).expect_err("whole-Vec field read in assert must be rejected");
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("whole-`Vec` read of record field"),
        "names the read: {msg}"
    );
}

/// A scalar RHS assigned into a whole-`Vec` record field
/// (`dst.data = 5`) must be REJECTED â€” NOT lowered into
/// `dst.data = 5;`, which miscompiles ("no viable overloaded '='" on
/// `std::array`). Only a matching-shape whole-`Vec` field read RHS is
/// admissible.
#[test]
fn scalar_rhs_into_whole_vec_field_is_rejected() {
    let src = r#"
struct Bundle
    data : Vec<uint<32>, 4>
end struct Bundle

test ScalarRhsTest
    let dut : Top
    run
        let dst : Bundle
        dst.data = 5
    end run
end test ScalarRhsTest
"#;
    let err = lower_src(src).expect_err("scalar RHS into whole-Vec field must be rejected");
    // Same site as the width-mismatch case above, which v1 compiles â€”
    // one rejection covering landings with different v1 outcomes cannot
    // claim `EmitsUncompilable`, even for the arm where that is true.
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("whole-`Vec` write of record field"),
        "names the write: {msg}"
    );
    assert!(msg.contains("non-matching RHS"), "names the reason: {msg}");
}

/// A whole-`Vec` field copy whose RHS field has a MISMATCHED shape
/// (different element width) must be rejected â€” the C++ `std::array`
/// copy would be ill-typed.
#[test]
fn mismatched_shape_whole_vec_copy_is_rejected() {
    let src = r#"
struct Wide
    data : Vec<uint<32>, 4>
end struct Wide

struct Narrow
    data : Vec<uint<16>, 4>
end struct Narrow

test MismatchTest
    let dut : Top
    run
        let w : Wide
        let n : Narrow
        n.data[0] = 1
        w.data = n.data
    end run
end test MismatchTest
"#;
    let err = lower_src(src).expect_err("mismatched-shape whole-Vec copy must be rejected");
    // Keeps its `--codegen v1` suggestion: "non-matching" is a HARC
    // judgement, and v1 collapses every scalar of 64 bits or fewer to
    // `uint64_t`. `Vec<uint<32>, 4> = Vec<uint<16>, 4>` really does
    // emit `std::array<uint64_t, 4> = std::array<uint64_t, 4>` there,
    // which compiles.
    let msg = assert_unsupported(&err);
    assert!(msg.contains("non-matching RHS"), "names the reason: {msg}");
}

/// The sanctioned whole-`Vec` field copy (`dst.data = src.data`, same
/// element type and length) STILL lowers cleanly â€” the #443 feature is
/// preserved. Lowers to a `RecordFieldWrite { index: None }` whose value
/// is the matching whole-`Vec` `RecordField { index: None }` read.
#[test]
fn matching_whole_vec_field_copy_still_lowers() {
    let src = r#"
struct Bundle
    data : Vec<uint<32>, 4>
end struct Bundle

test CopyTest
    let dut : Top
    run
        let src : Bundle
        let dst : Bundle
        src.data[0] = 1
        dst.data = src.data
    end run
end test CopyTest
"#;
    let prog = lower_src(src).expect("matching whole-Vec field copy lowers");
    let found = prog
        .functions
        .iter()
        .flat_map(|f| &f.blocks)
        .flat_map(|b| &b.stmts)
        .any(|s| {
            matches!(
                s,
                ir::Stmt::RecordFieldWrite {
                    index: None,
                    value: ir::Expr::RecordField { index: None, .. },
                    ..
                }
            )
        });
    assert!(
        found,
        "expected a whole-Vec RecordFieldWrite copy in the lowered body"
    );
}

/// A `struct` and a `transaction` sharing a name would resolve
/// ambiguously through `record_ids`; reject the collision.
#[test]
fn struct_name_collides_with_transaction() {
    let src = r#"
transaction Dup
    a : uint<8>
end transaction Dup

struct Dup
    b : uint<8>
end struct Dup

test C
    let dut : Top
    run
        wait 1 cycle
    end run
end test C
"#;
    let err = lower_src(src).expect_err("name collision must be rejected");
    assert!(
        format!("{err}").contains("collides"),
        "names the collision: {err}"
    );
}

/// A DUT read in a record-field-write value hoists through a DutRead
/// temp â€” same no-inline-ports discipline as `Assign`.
#[test]
fn record_field_write_hoists_dut_reads() {
    let src = r#"
transaction Req
    addr : uint<32>
end transaction Req

test HoistTest
    let dut : Top
    run
        let t : Req
        t.addr = dut.count_out
    end run
end test HoistTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let run = prog.function(prog.tests[0].run);
    let b = &run.blocks[0];
    assert!(
        matches!(b.stmts[1], ir::Stmt::DutRead(..)),
        "port hoisted before the field write:\n{run}"
    );
    assert!(
        matches!(
            &b.stmts[2],
            ir::Stmt::RecordFieldWrite {
                value: ir::Expr::Local(_),
                ..
            }
        ),
        "field write consumes the hoisted temp:\n{run}"
    );
}

/// Unknown fields are hard lowering errors (v1 would defer to a C++
/// compile failure; the IR rejects at lowering) â€” both on writes and
/// on reads.
#[test]
fn record_unknown_field_is_invalid() {
    let src = r#"
transaction Req
    addr : uint<32>
end transaction Req

test BadFieldTest
    let dut : Top
    run
        let t : Req
        t.nosuch = 1
    end run
end test BadFieldTest
"#;
    let err = lower_src(src).unwrap_err();
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "hard error, not Unsupported: {err:?}"
    );
    assert!(
        err.to_string().contains("no field `nosuch`"),
        "names the field: {err}"
    );
}

/// Whole-record assignment: a same-typed record-local copy lowers
/// (C++ struct assignment in both backends); anything else is a
/// precise lowering rejection, not a verifier error or C++ compile
/// failure.
#[test]
fn record_whole_value_assignment_rules() {
    let copy_src = r#"
transaction Req
    addr : uint<32> default 3
end transaction Req

test CopyTest
    let dut : Top
    run
        let t : Req
        let u : Req
        t.addr = 9
        u = t
        assert u.addr == 9 else fail("copy lost addr=${u.addr}")
    end run
end test CopyTest
"#;
    let prog = lower_src(copy_src).expect("record-to-record copy lowers");
    verify::verify_program(&prog).expect("verifies");

    let bad_src = r#"
transaction Req
    addr : uint<32>
end transaction Req

test BadCopyTest
    let dut : Top
    run
        let t : Req
        t = true
    end run
end test BadCopyTest
"#;
    let err = lower_src(bad_src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("non-`Req` value"),
        "names the record type: {msg}"
    );
}

/// Record-typed `let` copy: `let t2 : Txn = t1` (and the untyped bare
/// `let t3 = t1`) lower to a record-typed local bound by `Stmt::Assign`
/// â€” v1's by-value C++ struct copy. A non-record initializer into a
/// record-annotated local stays a precise lowering rejection.
#[test]
fn record_let_copy_lowers_by_value() {
    // Typed copy `let t2 : Req = t1`.
    let typed_src = r#"
transaction Req
    addr : uint<32> default 3
end transaction Req

test TypedCopyTest
    let dut : Top
    run
        let t1 : Req
        t1.addr = 9
        let t2 : Req = t1
        assert t2.addr == 9 else fail("copy lost addr=${t2.addr}")
    end run
end test TypedCopyTest
"#;
    let prog = lower_src(typed_src).expect("typed record let-copy lowers");
    verify::verify_program(&prog).expect("verifies");
    let rid = match prog.records.iter().position(|r| r.name == "Req") {
        Some(i) => ir::RecordId(i as u32),
        None => panic!("Req record present"),
    };
    let run = prog.function(prog.tests[0].run);
    // The copy is `Stmt::Assign(dest, Expr::Local(src))` where `dest` is
    // a record-typed local â€” the generic record-local declare + struct
    // copy carries it (no RecordInit at the copy site).
    let copy_dest = run
        .blocks
        .iter()
        .flat_map(|b| b.stmts.iter())
        .find_map(|s| match s {
            ir::Stmt::Assign(d, ir::Expr::Local(_)) => Some(*d),
            _ => None,
        })
        .expect("a record-local copy Assign");
    assert_eq!(
        run.locals[copy_dest.index()].ty,
        ir::IrType::Record(rid),
        "copy dest local is record-typed"
    );

    // Untyped bare copy `let t3 = t1` â€” tbir types the dest from the
    // source record (v1 would mis-declare it `int64_t`, so this form is
    // tbir-only and exercised here rather than in the equivalence suite).
    let untyped_src = r#"
transaction Req
    addr : uint<32> default 3
end transaction Req

test UntypedCopyTest
    let dut : Top
    run
        let t1 : Req
        t1.addr = 9
        let t3 = t1
        assert t3.addr == 9 else fail("copy lost addr=${t3.addr}")
    end run
end test UntypedCopyTest
"#;
    let prog = lower_src(untyped_src).expect("untyped record let-copy lowers");
    verify::verify_program(&prog).expect("verifies");
    let run = prog.function(prog.tests[0].run);
    assert!(
        run.blocks.iter().flat_map(|b| b.stmts.iter()).any(|s| {
            matches!(s, ir::Stmt::Assign(d, ir::Expr::Local(_))
                if matches!(run.locals[d.index()].ty, ir::IrType::Record(_)))
        }),
        "untyped record copy dest is record-typed:\n{run}"
    );

    // A non-record initializer into a record-annotated local stays a
    // precise rejection (no scalar-into-record type confusion).
    let bad_src = r#"
transaction Req
    addr : uint<32>
end transaction Req

test BadLetCopyTest
    let dut : Top
    run
        let t : Req = 7
    end run
end test BadLetCopyTest
"#;
    let err = lower_src(bad_src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("non-`Req` initializer"),
        "names the non-record-initializer rejection: {msg}"
    );

    // A copy from a *different* record type is rejected precisely (the
    // dest is annotated `Req` but the source is `Other`) â€” not deferred
    // to a verifier TypeMismatch or a C++ compile error.
    let mismatch_src = r#"
transaction Req
    addr : uint<32>
end transaction Req

transaction Other
    val : uint<32>
end transaction Other

test MismatchCopyTest
    let dut : Top
    run
        let o : Other
        let t : Req = o
    end run
end test MismatchCopyTest
"#;
    let err = lower_src(mismatch_src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("non-`Req` initializer"),
        "cross-type record copy rejected: {msg}"
    );
}

/// Every non-scalar record leaf, against what v1 emits for it â€” the
/// member declaration AND, where it exists, the randomize body.
///
/// The arm serves a dozen shapes and they split two ways, so one probe
/// cannot classify it. Two rounds of this went wrong in opposite
/// directions: a `queue` probe generalised to the whole arm denied
/// `list` its working escape hatch, and then a hand-written copy of
/// v1's type rules called a 128-bit field a flattening (v1 gives it a
/// correct `_harc_u128`) while calling `list<Inner>` a working hatch
/// (v1 gives it a `std::vector<uint64_t>` it never randomizes).
///
/// So each row carries the exact C++ v1 emits, and the verdict follows
/// from that rather than from the type name. Both wrong versions redden
/// here: the table is the measurement.
#[test]
fn a_non_scalar_record_leaf_is_classified_by_what_v1_emits_for_it() {
    let field = |ty: &str| {
        format!(
            r#"struct Inner
    x : uint<8>
end struct Inner

transaction Resp
    data : {ty}
end transaction Resp

test T
    let dut : Top
    run
        let r : Resp
        randomize(r)
        log(info, "x")
    end run
end test T"#
        )
    };
    // v1 gives the leaf a member that means what was written AND a
    // draw that fills it â€” a working escape hatch. A scalar row is
    // width-correct at any width; a container row keeps its shape.
    for (ty, shape, draw) in [
        ("uint<65>", "_harc_u128 data = 0;", "harc_rng_u128"),
        ("uint<128>", "_harc_u128 data = 0;", "harc_rng_u128"),
        ("sint<128>", "_harc_u128 data = 0;", "harc_rng_u128"),
        ("bits<128>", "_harc_u128 data = 0;", "harc_rng_u128"),
        ("uint<200>", "harc_rt::HarcWide<7> data = 0;", "harc_rng_wide<7>"),
        ("uint<256>", "harc_rt::HarcWide<8> data = 0;", "harc_rng_wide<8>"),
        ("list<uint<8>>", "std::vector<uint64_t> data", "data[_i] = harc_rt::random::harc_rng_uint"),
        (
            "list<uint<256>>",
            "std::vector<harc_rt::HarcWide<8>> data",
            "data[_i] = harc_rt::random::harc_rng_wide<8>",
        ),
        ("list<bool>", "std::vector<bool> data", "data[_i] = harc_rt::random::harc_rng_range"),
        (
            "list<sint<8>>",
            "std::vector<int64_t> data",
            "data[_i] = harc_rt::random::harc_rng_range(harc_rng_next, -(1LL << 7), (1LL << 7) - 1)",
        ),
        (
            "list<sint<64>>",
            "std::vector<int64_t> data",
            "data[_i] = harc_rt::random::harc_rng_uint(harc_rng_next, 64)",
        ),
        ("Vec<uint, 4>", "std::array<uint64_t, 4> data", "data = {}"),
        // `int` is 32-bit signed in HARC and this backend maps it
        // through the UNSIGNED width helper, so a `Vec<int, N>` member
        // is `std::array<uint64_t, N>` â€” which is also what TB-IR
        // mirrors. Pinned because the `Int` arm of `scalar_leaf_c_type`
        // had nothing measuring it.
        ("Vec<int, 4>", "std::array<uint64_t, 4> data", "data = {}"),
        ("Vec<uint<128>, 4>", "std::array<_harc_u128, 4> data", "data = {}"),
        (
            "Vec<uint<256>, 4>",
            "std::array<harc_rt::HarcWide<8>, 4> data",
            "data = {}",
        ),
        (
            "Vec<Vec<uint<8>, 2>, 4>",
            "std::array<std::array<uint64_t, 2>, 4> data",
            "data = {}",
        ),
    ] {
        let src = field(ty);
        let msg = assert_unsupported(&lower_src(&src).unwrap_err());
        assert!(msg.contains("non-scalar"), "`{ty}`: {msg}");
        let v1 = cpp_tb::emit(&merged_src(&src)).unwrap_or_else(|e| panic!("`{ty}`: {e}"));
        assert!(
            v1.contains(shape),
            "`{ty}`: v1 must emit `{shape}` for the suggestion to be honest"
        );
        assert!(v1.contains(draw), "`{ty}`: v1 must emit `{draw}`");
    }
    // v1 keeps the container but has no per-element draw for it, so the
    // loop body is `[_i] = 0` into a `std::array` â€” the emitted C++
    // does not compile, for any program that declares the record.
    for ty in ["list<Vec<uint<8>, 2>>", "list<Vec<Vec<uint<8>, 2>, 2>>"] {
        let src = field(ty);
        assert_not_implemented(
            &lower_src(&src).unwrap_err(),
            lower::V1Status::EmitsUncompilable,
        );
        let v1 = cpp_tb::emit(&merged_src(&src)).unwrap_or_else(|e| panic!("`{ty}`: {e}"));
        assert!(
            v1.contains("std::vector<std::array<") && v1.contains("t->data[_i] = 0;"),
            "`{ty}`: v1 assigns 0 into an array member: {v1}"
        );
    }
    // v1 flattens it â€” the field means something else, and the program
    // still compiles and runs. The `list` rows are the ones a
    // non-recursive carve-out gets backwards.
    for (ty, shape) in [
        ("queue<uint<8>>", "uint64_t data = 0;"),
        ("string", "int64_t data = 0;"),
        ("event<uint<8>>", "uint64_t data = 0;"),
        ("object", "int64_t data = 0;"),
        ("Vec<string, 4>", "uint64_t data = 0;"),
        ("Vec<uint<8>, N>", "uint64_t data = 0;"),
        // The subtle ones: the CONTAINER survives, its element does not.
        ("Vec<queue<uint<8>>, 4>", "std::array<uint64_t, 4> data"),
        ("list<Inner>", "std::vector<uint64_t> data"),
        ("list<string>", "std::vector<uint64_t> data"),
        ("list<queue<uint<8>>>", "std::vector<uint64_t> data"),
        ("list<event<uint<8>>>", "std::vector<uint64_t> data"),
        ("list<Vec<uint<8>, N>>", "std::vector<uint64_t> data"),
        // `int` is 32-bit signed in HARC and 64-bit unsigned here, and
        // `emit_field_random`'s list arm has no draw for it either.
        ("list<int>", "std::vector<uint64_t> data"),
    ] {
        let src = field(ty);
        let msg = assert_not_implemented(
            &lower_src(&src).unwrap_err(),
            lower::V1Status::SilentlyMisLowers,
        );
        assert!(msg.contains("non-scalar"), "`{ty}`: {msg}");
        // These flatten; they do NOT have an unreadable width. A
        // `queue<T>` / `event<T>` payload arrives in the same argument
        // POSITION a width would, so the two arms are only told apart
        // by which kind of type argument it is.
        assert!(
            msg.contains("flattens the field to a plain scalar") && !msg.contains("plain decimal"),
            "`{ty}` is the flatten arm, not the unreadable-width one: {msg}"
        );
        let v1 = cpp_tb::emit(&merged_src(&src)).unwrap_or_else(|e| panic!("`{ty}`: {e}"));
        assert!(v1.contains(shape), "`{ty}`: v1 flattens it to `{shape}`");
    }
    // The flattening `list` rows lose their per-element draw one of two
    // ways â€” a skipped field or a bare `0` â€” and the working ones keep
    // it. That is the second half of the same rule.
    for (ty, marker) in [
        ("list<Inner>", "// data : list (named, not yet supported)"),
        ("list<string>", "// data : list (named, not yet supported)"),
        ("list<queue<uint<8>>>", "t->data[_i] = 0;"),
        ("list<int>", "t->data[_i] = 0;"),
    ] {
        let v1 = cpp_tb::emit(&merged_src(&field(ty))).unwrap_or_else(|e| panic!("`{ty}`: {e}"));
        assert!(v1.contains(marker), "`{ty}`: v1 emits `{marker}`");
    }
}

/// A width slot this compiler cannot read as a plain decimal.
///
/// It is NOT a zero-width type â€” `uint<0x0>` does not panic v1 â€” and it
/// is NOT an escape hatch either: v1 cannot read the width and says
/// nothing, substituting a different fallback in every place that needs
/// one. `uint<0x8>` against `uint<8>` is the whole measurement.
#[test]
fn an_unreadable_width_is_a_silent_mis_lowering_not_a_hatch() {
    let field = |ty: &str| {
        format!(
            r#"const W = 8

struct Inner
    x : uint<8>
end struct Inner

transaction Req
    op : uint<8>
    data : {ty}
end transaction Req

test T
    let dut : Top
    run
        let r : Req
        randomize(r)
        log(info, "x")
    end run
end test T"#
        )
    };
    for ty in [
        "uint<0x0>",
        "uint<0b0>",
        "uint<0x8>",
        "uint<0b1000>",
        "uint<W>",
        // v1 substitutes the same fallbacks PER ELEMENT, so a `Vec` of
        // one is the same silent mis-lowering.
        "Vec<uint<0x8>, 4>",
        "Vec<uint<W>, 4>",
    ] {
        let src = field(ty);
        let err = lower_src(&src).unwrap_err();
        assert!(
            !matches!(err, lower::LowerError::Invalid(_)),
            "`{ty}` is not Invalid â€” v1 does not panic on it: {err:?}"
        );
        assert_not_implemented(&err, lower::V1Status::SilentlyMisLowers);
    }
    // The `Vec` rows pack four 64-bit slots where the decimal spelling
    // packs four 8-bit ones.
    let vhex = cpp_tb::emit(&merged_src(&field("Vec<uint<0x8>, 4>"))).expect("v1 emits");
    let vdec = cpp_tb::emit(&merged_src(&field("Vec<uint<8>, 4>"))).expect("v1 emits");
    assert!(
        vhex.contains("harc_rt::harc_wide_write_bits(_packed, 0, 64, value.data[0]);")
            && vdec.contains("harc_rt::harc_wide_write_bits(_packed, 0, 8, value.data[0]);"),
        "the per-element width is substituted too:\n{vhex}"
    );
    for ty in ["uint<0x0>", "uint<0x8>", "uint<W>"] {
        let v1 = cpp_tb::emit(&merged_src(&field(ty))).unwrap_or_else(|e| panic!("`{ty}`: {e}"));
        assert!(v1.contains("uint64_t data = 0;"), "`{ty}`: {v1}");
    }
    // A RECORD payload is not a width slot, even though it arrives in
    // the same argument position as one. This arm must not claim it.
    for ty in ["queue<Inner>", "event<Inner>"] {
        let src = field(ty);
        let msg = assert_not_implemented(
            &lower_src(&src).unwrap_err(),
            lower::V1Status::SilentlyMisLowers,
        );
        assert!(
            !msg.contains("plain decimal"),
            "`{ty}` has no width slot â€” it is the flatten arm: {msg}"
        );
    }
    // The three widths v1 substitutes, none of them the declared 8.
    let hex = cpp_tb::emit(&merged_src(&field("uint<0x8>"))).expect("v1 emits");
    for needle in [
        "harc_rt::harc_wide_write_bits(_packed, 0, 64, value.data);",
        "value.data = (uint64_t)harc_rt::harc_bits(_packed, 63, 0);",
        "t->data = harc_rt::random::harc_rng_uint(harc_rng_next, 32);",
    ] {
        assert!(hex.contains(needle), "v1 emits `{needle}`: {hex}");
    }
    // â€¦against the decimal spelling, which is consistent at 8.
    let dec = cpp_tb::emit(&merged_src(&field("uint<8>"))).expect("v1 emits");
    for needle in [
        "harc_rt::harc_wide_write_bits(_packed, 0, 8, value.data);",
        "value.data = (uint64_t)harc_rt::harc_bits(_packed, 7, 0);",
        "t->data = harc_rt::random::harc_rng_uint(harc_rng_next, 8);",
    ] {
        assert!(dec.contains(needle), "v1 emits `{needle}`: {dec}");
    }
}

/// A zero-width record leaf, and the build profile that decides it.
///
/// This arm was `Invalid` on the grounds that v1 PANICS on it â€”
/// "attempt to subtract with overflow" in `emit_unpack_bits`. That
/// panic is a DEBUG-build artifact: Rust turns integer overflow checks
/// off under `--release`, which is how CI builds and how the `harc`
/// binary ships. CI caught it; no amount of debug-profile `cargo test`
/// could have.
///
/// In release v1 emits a complete testbench that compiles clean, with
/// the zero-width field as a full 64-bit member packed at width 0 â€” it
/// carries no bits and nothing says so. `Invalid` claims no backend
/// runs it in ANY configuration, and a release-built v1 runs it.
///
/// The assertions hold in BOTH profiles: the verdict is
/// profile-independent, and the emitted evidence is checked only where
/// v1 got far enough to produce it.
#[test]
fn a_zero_width_record_leaf_is_a_silent_mis_lowering_in_the_shipped_profile() {
    let field = |ty: &str| {
        format!(
            r#"struct Resp
    data : {ty}
end struct Resp

test T
    let dut : Top
    run
        log(info, "x")
    end run
end test T"#
        )
    };
    for (ty, member) in [
        ("uint<0>", "uint64_t data = 0;"),
        ("sint<0>", "int64_t data = 0;"),
        ("bits<0>", "uint64_t data = 0;"),
        ("Vec<uint<0>, 4>", "std::array<uint64_t, 4> data = {};"),
    ] {
        let src = field(ty);
        let msg = assert_not_implemented(
            &lower_src(&src).unwrap_err(),
            lower::V1Status::SilentlyMisLowers,
        );
        assert!(msg.contains("zero-width"), "`{ty}`: {msg}");
        // Debug panics on the width arithmetic; release does not. Check
        // the emission only where there is one â€” and where there is, it
        // must be the silent full-width member, not a diagnostic.
        if let Ok(Ok(v1)) = std::panic::catch_unwind(|| cpp_tb::emit(&merged_src(&field(ty)))) {
            assert!(v1.contains(member), "`{ty}`: v1 emits `{member}`: {v1}");
            assert!(
                v1.contains("harc_wide_write_bits(_packed, 0, 0, value.data"),
                "`{ty}`: â€¦and packs it at width zero: {v1}"
            );
        }
    }
    // The length, not the width â€” `std::array<uint64_t, 0>`, in either
    // profile.
    let src = field("Vec<uint<8>, 0>");
    assert_unsupported(&lower_src(&src).unwrap_err());
    let v1 = cpp_tb::emit(&merged_src(&src)).expect("v1 emits");
    assert!(v1.contains("std::array<uint64_t, 0> data"), "{v1}");

    // A zero-width PAYLOAD under a container is not this rule at all:
    // v1 never reads a width there and its output compiles. The first
    // version of the guard recursed through every type argument and
    // made all three `Invalid`.
    for (ty, shape) in [
        ("list<uint<0>>", "std::vector<uint64_t> data"),
        ("queue<uint<0>>", "uint64_t data = 0;"),
        ("event<uint<0>>", "uint64_t data = 0;"),
    ] {
        let src = field(ty);
        let err = lower_src(&src).unwrap_err();
        assert!(
            !matches!(err, lower::LowerError::Invalid(_)),
            "`{ty}` is not Invalid: {err:?}"
        );
        let v1 = cpp_tb::emit(&merged_src(&src)).unwrap_or_else(|e| panic!("`{ty}`: {e}"));
        assert!(v1.contains(shape), "`{ty}`: v1 emits `{shape}`");
    }
}

/// The other `records.rs` arms, each measured on its own.
///
/// | construct | v1 | verdict |
/// |---|---|---|
/// | `keep` in a struct | `_s.add(z3::ult(_z_a, â€¦10â€¦))` â€” it reaches the solver | a real escape hatch |
/// | `default` on a nested-record field | `Inner i = 0;` â€” g++ rejects the conversion | `EmitsUncompilable` |
/// | `default` on a `Vec` field | `std::array<T, N> v = 0;` â€” same | `EmitsUncompilable` |
/// | `default 4'd3`, `8'hFF`, `4'b1010` | folds to the same value | a real escape hatch |
/// | `default 128'hFFâ€¦`, `0xFFâ€¦`, `999â€¦` | folds past 64 bits and truncates | `SilentlyMisLowers` |
///
/// The literal rows do not split on the apostrophe â€” the width prefix
/// is not the value. `4'd3` folds to `3` and `128'hFFâ€¦` folds to a
/// `_harc_u128` composite that the 64-bit member truncates, and an
/// unsized decimal past `u64` does the same thing with a different
/// diagnostic. The guard normalizes through `cpp_tb`'s own folder and
/// asks whether the result fits.
///
/// Compiled with `-std=gnu++20`, the standard `src/main.rs` passes.
#[test]
fn the_record_field_arms_split_on_measured_v1_behaviour() {
    let prog = |decls: &str| {
        format!(
            "{decls}\ntest T\n    let dut : Top\n    run\n        let r : Rec\n        \
             randomize(r)\n        log(info, \"x\")\n    end run\nend test T"
        )
    };

    // A `keep` on a struct DOES reach the solver â€” the absence of a
    // randomize metadata entry, which is what the first pass measured,
    // says nothing about the generated solver lambda.
    let keep = prog("struct Rec\n    a : uint<8>\n    keep a < 10\nend struct Rec\n");
    assert_unsupported(&lower_src(&keep).unwrap_err());
    let v1 = cpp_tb::emit(&merged_src(&keep)).expect("v1 emits");
    assert!(
        v1.contains("_s.add(z3::ult(_z_a, _ctx.bv_val((uint64_t)10, 64)));"),
        "v1 emits the constraint into the solver: {v1}"
    );

    // Both `default` shapes make v1 emit an initializer g++ rejects.
    for (decls, needle) in [
        (
            "struct Inner\n    x : uint<8>\nend struct Inner\n\nstruct Rec\n    i : Inner default 0\nend struct Rec\n",
            "Inner i = 0;",
        ),
        (
            "struct Rec\n    v : Vec<uint<8>, 4> default 0\nend struct Rec\n",
            "std::array<uint64_t, 4> v = 0;",
        ),
    ] {
        let src = prog(decls);
        assert_not_implemented(
            &lower_src(&src).unwrap_err(),
            lower::V1Status::EmitsUncompilable,
        );
        let v1 = cpp_tb::emit(&merged_src(&src)).expect("v1 emits");
        assert!(v1.contains(needle), "v1 emits `{needle}`: {v1}");
    }

    // A literal whose VALUE fits the 64-bit member folds correctly,
    // whatever its spelling â€” a real escape hatch.
    for (lit, folded) in [
        ("4'd3", "uint64_t a = 3;"),
        ("8'hFF", "uint64_t a = 0xFF;"),
        ("4'b1010", "uint64_t a = 0b1010;"),
    ] {
        let src = prog(&format!(
            "struct Rec\n    a : uint<8> default {lit}\nend struct Rec\n"
        ));
        assert_unsupported(&lower_src(&src).unwrap_err());
        let v1 = cpp_tb::emit(&merged_src(&src)).expect("v1 emits");
        assert!(v1.contains(folded), "`{lit}` folds to `{folded}`: {v1}");
    }
    // One that does not fit truncates into the member with only a
    // warning â€” and a SIZED literal is on this side of the line too,
    // which is what splitting on the apostrophe got wrong.
    for (lit, folded) in [
        (
            "128'hFFFFFFFFFFFFFFFFFFFF",
            "uint64_t a = (((_harc_u128)0xFFFFULL << 64) | (_harc_u128)0xFFFFFFFFFFFFFFFFULL);",
        ),
        (
            "0xFFFFFFFFFFFFFFFFF",
            "uint64_t a = (((_harc_u128)0xFULL << 64) | (_harc_u128)0xFFFFFFFFFFFFFFFFULL);",
        ),
        (
            "99999999999999999999999",
            "uint64_t a = 99999999999999999999999;",
        ),
    ] {
        let src = prog(&format!(
            "struct Rec\n    a : uint<8> default {lit}\nend struct Rec\n"
        ));
        assert_not_implemented(
            &lower_src(&src).unwrap_err(),
            lower::V1Status::SilentlyMisLowers,
        );
        let v1 = cpp_tb::emit(&merged_src(&src)).expect("v1 emits");
        assert!(v1.contains(folded), "`{lit}` folds to `{folded}`: {v1}");
    }
}

/// The `scoreboards.rs` arms, measured against v1's emitted struct.
///
/// | construct | v1 emits | verdict |
/// |---|---|---|
/// | `bound to` on the scoreboard | output BYTE-IDENTICAL to the unbound one | `SilentlyMisLowers` |
/// | `bound to` on a field | byte-identical likewise | `SilentlyMisLowers` |
/// | a directional (port) field | `uint64_t p;` â€” uninitialized, direction dropped | `SilentlyMisLowers` |
/// | a `default` on a queue field | `HarcQueue<uint64_t> q = 0;` â€” no such constructor | `EmitsUncompilable` |
/// | `list<uint<8>>` / `uint<128>` field | `std::vector<uint64_t> l;` / `_harc_u128 l;` | a real escape hatch |
/// | `list<Vec<uint<8>, 2>>` field | `std::vector<std::array<uint64_t, 2>> l;` | likewise â€” no randomize body to break |
/// | `string` / `event<T>` field | `int64_t s;` / `uint64_t e;` â€” uninitialized | `SilentlyMisLowers` |
///
/// The `bound to` rows are the load-bearing measurement: "v1 emits" is
/// true for both, and diffing against the unbound control is what shows
/// the clause left no trace at all.
#[test]
fn the_scoreboard_arms_split_on_what_v1_emits() {
    let prog = |sb: &str| {
        format!(
            r#"domain SysDomain
  freq_mhz: 100
end domain SysDomain

transactor Drv
    dut : Top
end transactor Drv

struct Inner
    x : uint<8>
end struct Inner

{sb}

testbench Tb
    dut : Top
    sb  : Sb
end testbench Tb

impl T for Tb
    clock clk = SysDomain
    run
        wait 1 cycle
    end run
end impl T"#
        )
    };
    let control = prog("scoreboard Sb\n    seen : uint<32> default 0\nend scoreboard Sb");
    let control_cpp = cpp_tb::emit(&merged_src(&control)).expect("v1 emits the control");
    lower_src(&control).expect("the control lowers");

    // Both `bound to` spellings are discarded â€” byte-identically.
    for (what, src) in [
        (
            "declaration",
            prog("scoreboard Sb bound to Drv\n    seen : uint<32> default 0\nend scoreboard Sb"),
        ),
        (
            "field",
            prog("scoreboard Sb\n    seen : uint<32> bound to Drv default 0\nend scoreboard Sb"),
        ),
    ] {
        let msg = assert_not_implemented(
            &lower_src(&src).unwrap_err(),
            lower::V1Status::SilentlyMisLowers,
        );
        assert!(msg.contains("`bound to`"), "{what}: {msg}");
        assert_eq!(
            cpp_tb::emit(&merged_src(&src)).expect("v1 emits"),
            control_cpp,
            "{what}: v1's output must be byte-identical to the unbound control â€” that is \
             the whole evidence that the clause is discarded"
        );
    }

    // A port field keeps its name and loses everything else.
    let port = prog("scoreboard Sb\n    p : in uint<8>\nend scoreboard Sb");
    assert_not_implemented(
        &lower_src(&port).unwrap_err(),
        lower::V1Status::SilentlyMisLowers,
    );
    assert!(
        cpp_tb::emit(&merged_src(&port))
            .expect("v1 emits")
            .contains("uint64_t p;"),
        "v1 emits an uninitialized scalar with no direction"
    );

    // A queue default is the one that does not compile.
    let qd = prog("scoreboard Sb\n    q : queue<uint<32>> default 0\nend scoreboard Sb");
    assert_not_implemented(
        &lower_src(&qd).unwrap_err(),
        lower::V1Status::EmitsUncompilable,
    );
    assert!(
        cpp_tb::emit(&merged_src(&qd))
            .expect("v1 emits")
            .contains("harc_rt::HarcQueue<uint64_t> q = 0;"),
        "v1 emits an initializer `HarcQueue` has no constructor for"
    );

    // The field-type arm asks the SAME predicate as the record one
    // rather than a second copy of it â€” but maps its third outcome
    // differently, and that difference is measured, not assumed. A
    // scoreboard emits no randomize body, so the leaf whose randomize
    // body is what stops compiling keeps a correct member here.
    for (ty, shape) in [
        ("list<uint<8>>", "std::vector<uint64_t> l;"),
        ("uint<128>", "_harc_u128 l;"),
        (
            "list<Vec<uint<8>, 2>>",
            "std::vector<std::array<uint64_t, 2>> l;",
        ),
        // A record-typed field. `txn_field_c_type` alone maps every
        // named type to `int64_t`, and asking only it called this a
        // flattening â€” but the member picker these fields actually go
        // through adds one layer, and v1 emits the record itself.
        ("Inner", "Inner l;"),
    ] {
        let src = prog(&format!("scoreboard Sb\n    l : {ty}\nend scoreboard Sb"));
        assert_unsupported(&lower_src(&src).unwrap_err());
        assert!(
            cpp_tb::emit(&merged_src(&src))
                .expect("v1 emits")
                .contains(shape),
            "`{ty}`: v1 emits `{shape}`, which is what keeps the suggestion honest"
        );
    }
    for (ty, shape) in [("string", "int64_t s;"), ("event<uint<8>>", "uint64_t s;")] {
        let src = prog(&format!("scoreboard Sb\n    s : {ty}\nend scoreboard Sb"));
        assert_not_implemented(
            &lower_src(&src).unwrap_err(),
            lower::V1Status::SilentlyMisLowers,
        );
        assert!(
            cpp_tb::emit(&merged_src(&src))
                .expect("v1 emits")
                .contains(shape),
            "`{ty}`: v1 flattens it to `{shape}`"
        );
    }
}

/// The five `= bind <name>` arms, and the hole between them.
///
/// A regblock, an addrmap, an initiator-BFM instance, a bound-to
/// event-driven transactor and a target-TLM responder all require a
/// bare identifier on the right of `= bind`, and all five checked for
/// it with their own copy of the same four-line match â€” each saying
/// "re-run with `--codegen v1`". v1 REJECTS a non-identifier RHS itself,
/// with its own diagnostic, so every one of those suggestions sent the
/// user to an identical refusal.
///
/// The hole is what the five copies were all guarding the wrong side
/// of: with NO `= bind` at all, `l.bind` is false, none of the five
/// arms is reached, and a regblock's mirror record shares the
/// regblock's name â€” so the `let` landed on the ordinary record-local
/// path and lowered clean. The emitted testbench then served every
/// register access from the mirror and issued no bus traffic at all.
#[test]
fn a_bind_needs_a_bare_name_and_a_regblock_needs_a_bind() {
    let fixture_with = |name: &str, from: &str, to: &str| {
        let src = fixture(name);
        assert_eq!(src.matches(from).count(), 1, "`{from}` is unique in {name}");
        src.replace(from, to)
    };
    let lower_bus =
        |src: &str| lower::lower_program(&merged_with_stdlib_bus(src, "BusAxiLite.arch"));

    // Every non-identifier spelling, at every one of the five landings.
    // v1 refuses each with its own message, so none may suggest it.
    for (name, from, to, construct) in [
        (
            "regblock_access_test.harc",
            "let regs   : DmaRegs = bind helper",
            "let regs   : DmaRegs = bind helper.x",
            "regblock binding `regs` to a non-identifier helper",
        ),
        (
            "regblock_access_test.harc",
            "let regs   : DmaRegs = bind helper",
            "let regs   : DmaRegs = bind 5",
            "regblock binding `regs` to a non-identifier helper",
        ),
        (
            "regblock_access_test.harc",
            "let regs   : DmaRegs = bind helper",
            "let regs   : DmaRegs = bind (helper)",
            "regblock binding `regs` to a non-identifier helper",
        ),
        (
            "regblock_access_test.harc",
            "let helper : AxilHelper active = bind axil",
            "let helper : AxilHelper active = bind axil.aw",
            "initiator-BFM instance `helper` bound to a non-identifier",
        ),
        (
            "regblock_addrmap_test.harc",
            "let chip   : Soc = bind helper",
            "let chip   : Soc = bind helper.x",
            "addrmap binding `chip` to a non-identifier helper",
        ),
        (
            "axilite_bound_mon_test.harc",
            "let drv  : AxilXactor active  = bind axil",
            "let drv  : AxilXactor active  = bind axil.aw",
            "bound-to event-driven transactor `drv` bound to a non-identifier",
        ),
    ] {
        let src = fixture_with(name, from, to);
        let msg = assert_not_implemented(&lower_bus(&src).unwrap_err(), lower::V1Status::Rejects);
        assert!(msg.contains(construct), "{msg}");
        // The evidence: v1 refuses it too, rather than emitting.
        let err = cpp_tb::emit(&merged_with_stdlib_bus(&src, "BusAxiLite.arch"))
            .expect_err("v1 refuses a non-identifier bind RHS");
        assert!(
            format!("{err}").contains("bind <expr>"),
            "v1's own refusal names the shape: {err}"
        );
    }
    // The fifth landing lives in a different fixture and bus.
    let tlm = {
        let src = fixture("dma_engine_tlm_target_test.harc");
        src.replace(
            "let target : DmaMemTarget passive = bind mem",
            "let target : DmaMemTarget passive = bind mem.x",
        )
    };
    let merged =
        merge::merge_for_sim(vec![parse_source(&tlm).expect("parses")], None).expect("merge");
    let msg = assert_not_implemented(
        &lower::lower_program(&merged).unwrap_err(),
        lower::V1Status::Rejects,
    );
    assert!(msg.contains("target-TLM responder `target`"), "{msg}");
    assert!(
        format!("{}", cpp_tb::emit(&merged).expect_err("v1 refuses")).contains("bind <expr>"),
        "v1 refuses it too"
    );

    // A regblock-typed SCOREBOARD leaf is not a record leaf, even
    // though a regblock's mirror record lands in `record_ids`. v1's own
    // `is_record_type` is transactions âˆª structs, so it flattens this
    // one to `int64_t l;` â€” and asking the wrong set turned the arm
    // into a false escape hatch.
    let sb_rb = fixture_with(
        "regblock_access_test.harc",
        "testbench RegblockAccessTb\n    dut : AxiLiteRegs\nend testbench RegblockAccessTb",
        "scoreboard Sb\n    l : DmaRegs\nend scoreboard Sb\n\ntestbench RegblockAccessTb\n             dut : AxiLiteRegs\n    sb  : Sb\nend testbench RegblockAccessTb",
    );
    assert_not_implemented(
        &lower_bus(&sb_rb).unwrap_err(),
        lower::V1Status::SilentlyMisLowers,
    );
    assert!(
        cpp_tb::emit(&merged_with_stdlib_bus(&sb_rb, "BusAxiLite.arch"))
            .expect("v1 emits")
            .contains("int64_t l;"),
        "v1 flattens a regblock-typed scoreboard leaf"
    );

    // The hole. All three spellings of a regblock-typed `let` with no
    // `= bind` used to lower clean and drop every bus access â€” and it
    // is reachable from a hookable method body and a `tseq` body, not
    // just from test scope, so every `LowerCtx` carries the type set.
    for (name, from, to) in [
        (
            "regblock_access_test.harc",
            "let regs   : DmaRegs = bind helper",
            "let regs   : DmaRegs",
        ),
        (
            "regblock_access_test.harc",
            "        let v = regs.MM2S_LEN",
            "        let snap : DmaRegs\n        let v = regs.MM2S_LEN",
        ),
        (
            "regblock_access_test.harc",
            "        let v = regs.MM2S_LEN",
            "        let snap : DmaRegs = regs\n        let v = regs.MM2S_LEN",
        ),
        (
            "regblock_addrmap_test.harc",
            "let chip   : Soc = bind helper",
            "let chip   : Soc",
        ),
        // Inside a hookable METHOD body â€” a different `LowerCtx`, which
        // used to carry an empty type set and let the whole thing
        // through.
        (
            "regblock_access_test.harc",
            "        hookable read(addr: uint<8>) -> uint<32>",
            "        hookable probe2()\n            let regs4 : DmaRegs\n                         let z = regs4.MM2S_LEN\n        end probe2\n\n        hookable read(addr:              uint<8>) -> uint<32>",
        ),
        // â€¦and inside a `tseq` body, which is a third `LowerCtx`. Both
        // of these lowered AND verified clean before every context
        // carried the type set, so a status-only assertion on the test
        // scope pinned nothing about them.
        (
            "regblock_access_test.harc",
            "regblock DmaRegs via AxilHelper width 32",
            "tseq Str() -> TSeq<uint<8>>\n    let regs5 : DmaRegs\n    yield 1\nend tseq Str\n\nregblock DmaRegs via AxilHelper width 32",
        ),
    ] {
        let src = fixture_with(name, from, to);
        let err = lower_bus(&src).unwrap_err();
        let lower::LowerError::Invalid(msg) = &err else {
            panic!("`{to}` is Invalid, not `{err:?}`");
        };
        assert!(msg.contains("without a bus"), "{msg}");
        // v1 states the same rule, which is where the wording came from.
        let v1 = cpp_tb::emit(&merged_with_stdlib_bus(&src, "BusAxiLite.arch"))
            .expect_err("v1 refuses an unbound regblock instantiation");
        assert!(
            format!("{v1}").contains("requires `= bind <helper>`"),
            "v1's own rule: {v1}"
        );
    }
}

/// The unbound-transactor item arms, from BOTH declaration positions.
///
/// These were two copies of the same 120-line match â€” the always-on
/// walk and the `when active` walk â€” differing in one expression, so
/// five of the rejections were the same rejection written twice. Every
/// one said "re-run with `--codegen v1`"; only one of the six shapes
/// they cover is something v1 gets right.
///
/// | item | v1 emits | verdict |
/// |---|---|---|
/// | `req : in event<uint<8>>` | a real subscriber vector plus a fan-out at the emit site | a real escape hatch |
/// | `p : in uint<8>` / `out uint<8>` | `uint64_t p;` â€” the direction is dropped | `SilentlyMisLowers` |
/// | `dut : Top default 1` | `VTop* dut = 1;` â€” "invalid conversion from 'int' to 'VTop*'" | `EmitsUncompilable` |
/// | a second module-typed field | `_tb.drv.dut = dut; _tb.drv.other = dut;` â€” both bound, both driven | a real escape hatch |
/// | `apply Some.Policy` | nothing at all | `SilentlyMisLowers` |
///
/// Every row is asserted in both positions, because that is the
/// property the merge is for.
#[test]
fn the_unbound_transactor_item_arms_agree_across_both_positions() {
    let drv = |outer: &str, active: &str, body: &str| {
        format!(
            r#"domain SysDomain
  freq_mhz: 100
end domain SysDomain

transactor Drv
{outer}
    when active
{active}
        hookable step(n : uint<8>)
{body}
            wait 1 cycle
        end hookable
    end when
end transactor Drv

testbench Tb
    dut : Top
    drv : Drv active
end testbench Tb

impl T for Tb
    clock clk = SysDomain
    run
        drv.step(1)
        wait 2 cycles
    end run
end impl T"#
        )
    };
    let poke = "            dut.en = 1";
    lower_src(&drv("    dut : Top", "", poke)).expect("the control lowers");

    // Each row, declared OUTSIDE the `when active` block and INSIDE it.
    // The two spellings must give the identical verdict â€” that is what
    // the merged walk buys, and what two copies of it kept losing.
    let both = |field: &str| {
        [
            drv(&format!("    dut : Top\n{field}"), "", poke),
            drv("    dut : Top", &format!("    {field}"), poke),
        ]
    };

    // v1 models a directional EVENT â€” a real `std::function` fan-out.
    for src in both("    req : in event<uint<8>>") {
        let msg = assert_unsupported(&lower_src(&src).unwrap_err());
        assert!(msg.contains("directional event field"), "{msg}");
        let v1 = cpp_tb::emit(&merged_src(&src)).expect("v1 emits");
        assert!(
            v1.contains("std::vector<std::function<void(uint64_t)>> req;"),
            "v1 models the event field: {v1}"
        );
    }
    // The declaration alone is not the claim â€” an `emit` into it has to
    // produce a real fan-out, or "v1 models it" is only half measured.
    let emitting = drv("    dut : Top\n    req : in event<uint<8>>", "", poke).replace(
        "        drv.step(1)",
        "        emit drv.req(1)\n        drv.step(1)",
    );
    assert_unsupported(&lower_src(&emitting).unwrap_err());
    let v1 = cpp_tb::emit(&merged_src(&emitting)).expect("v1 emits");
    assert!(
        v1.contains("for (auto& _s : _tb.drv.req) _s(1);"),
        "v1 fans the emit out over the subscriber vector: {v1}"
    );
    // â€¦and flattens a directional SCALAR to an uninitialized member.
    for field in ["    p : in uint<8>", "    p : out uint<8>"] {
        for src in both(field) {
            let msg = assert_not_implemented(
                &lower_src(&src).unwrap_err(),
                lower::V1Status::SilentlyMisLowers,
            );
            assert!(msg.contains("directional scalar field"), "{msg}");
            let v1 = cpp_tb::emit(&merged_src(&src)).expect("v1 emits");
            assert!(v1.contains("uint64_t p;"), "v1 flattens it: {v1}");
        }
    }
    // A second DUT handle: v1 emits a `V<Name>*` member for each while
    // including only the TESTBENCH DUT's Verilated header, and this
    // function cannot see which module that is â€” transactors lower
    // first. So the arm takes the worst of what is under it.
    for src in both("    other : Top") {
        let msg = assert_not_implemented(
            &lower_src(&src).unwrap_err(),
            lower::V1Status::EmitsUncompilable,
        );
        assert!(
            msg.contains("more than one module-typed field"),
            "the arm that fires is this one, not a later unrelated              assignment rejection: {msg}"
        );
    }
    let bound_both = format!(
        r#"domain SysDomain
  freq_mhz: 100
end domain SysDomain

transactor Drv
    dut : Top
    other : Top

    when active
        hookable step(n : uint<8>)
            dut.en = 1
            other.en = 1
            wait 1 cycle
        end hookable
    end when
end transactor Drv

testbench Tb
    dut : Top
    drv : Drv active
end testbench Tb

impl T for Tb
    clock clk = SysDomain
    run
        drv.dut = dut
        drv.other = dut
        drv.step(1)
        wait 2 cycles
    end run
end impl T"#
    );
    assert_not_implemented(
        &lower_src(&bound_both).unwrap_err(),
        lower::V1Status::EmitsUncompilable,
    );
    let v1 = cpp_tb::emit(&merged_src(&bound_both)).expect("v1 emits");
    assert!(
        v1.contains("_tb.drv.dut = dut;") && v1.contains("_tb.drv.other = dut;"),
        "v1 does bind both handles â€” that half is real: {v1}"
    );
    // â€¦and this is the half that decides the verdict: two handles of a
    // module that is NOT the testbench's DUT. A split on the
    // transactor's own handle type calls them equal, and v1 emits
    // `VFoo*` twice against a `VTop.h`-only include.
    let two_foo = drv("    d1 : Foo\n    d2 : Foo", "", "            d1.en = 1");
    assert_not_implemented(
        &lower_src(&two_foo).unwrap_err(),
        lower::V1Status::EmitsUncompilable,
    );
    let v1 = cpp_tb::emit(&merged_src(&two_foo)).expect("v1 emits");
    assert!(
        v1.contains("VFoo* d1 = nullptr;")
            && v1.contains("VFoo* d2 = nullptr;")
            && v1.contains("#include \"VTop.h\"")
            && !v1.contains("#include \"VFoo.h\""),
        "v1 emits both members and only the test DUT's header: {v1}"
    );
    // â€¦but only for the SAME module type. v1 includes just the one
    // Verilated header the testbench's DUT needs, so a second named
    // field of any other type is an undeclared type in the emitted C++.
    for (field, cty) in [
        ("    other : AxiLiteRegs", "VAxiLiteRegs* other = nullptr;"),
        ("    other : Nonesuch", "VNonesuch* other = nullptr;"),
        ("    mode : Color", "Color mode;"),
    ] {
        for src in both(field) {
            let src = src.replace(
                "\ntransactor Drv\n",
                "\nenum Color { Red, Blue }\n\ntransactor Drv\n",
            );
            let msg = assert_not_implemented(
                &lower_src(&src).unwrap_err(),
                lower::V1Status::EmitsUncompilable,
            );
            assert!(
                msg.contains("more than one module-typed field"),
                "`{field}`: {msg}"
            );
            let v1 = cpp_tb::emit(&merged_src(&src)).expect("v1 emits");
            assert!(v1.contains(cty), "`{field}`: v1 emits `{cty}`: {v1}");
        }
    }

    // `apply` leaves no trace whatsoever.
    for src in both("    apply Some.Policy") {
        assert_not_implemented(
            &lower_src(&src).unwrap_err(),
            lower::V1Status::SilentlyMisLowers,
        );
    }
    assert_eq!(
        cpp_tb::emit(&merged_src(&drv(
            "    dut : Top\n    apply Some.Policy",
            "",
            poke
        )))
        .expect("v1 emits"),
        cpp_tb::emit(&merged_src(&drv("    dut : Top", "", poke))).expect("v1 emits"),
        "the `apply` item leaves v1's output byte-identical"
    );

    // The DUT-handle default: `0` is a null pointer constant and
    // compiles, every other literal does not, so the arm takes the
    // worse of the two.
    for lit in ["0", "1", "5"] {
        for src in both(&format!("    other : Top default {lit}")) {
            assert_not_implemented(
                &lower_src(&src).unwrap_err(),
                lower::V1Status::EmitsUncompilable,
            );
        }
    }
    for (lit, init) in [("0", "VTop* dut = 0;"), ("1", "VTop* dut = 1;")] {
        let src = drv(&format!("    dut : Top default {lit}"), "", poke);
        let v1 = cpp_tb::emit(&merged_src(&src)).expect("v1 emits");
        assert!(v1.contains(init), "`default {lit}` pastes `{init}`: {v1}");
    }
}

/// The `on <event>(arg)` subscription arms in `components.rs`.
///
/// Six sites; five were provably dead. `event_subscription` â€” the
/// predicate that ROUTES a handler here â€” already establishes that the
/// trigger is a `Call` on a bare identifier naming an `event` field, and
/// a periodic handler never reaches the loop at all. The resolver then
/// re-derived all four facts and carried a rejection arm for each. Every
/// one of those shapes lands on a different diagnostic entirely, which
/// is what this test pins: if the routing predicate ever loosens, these
/// stop matching and the dead arms are dead no longer.
///
/// This pins where each lands, NOT that the arm it lands on is itself
/// correctly classified â€” those arms are outside this one's scope.
///
/// | trigger | where it actually lands |
/// |---|---|
/// | `on 3 cycles` | lowers â€” a periodic handler |
/// | `on clk` | the unresolved-name arm |
/// | `on tagger.in_ev(t)` | the transactor/method-call arm |
/// | `on other(t)` (a scalar field) | the helper-call arm |
/// | `on nosuch(t)` | the helper-call arm |
///
/// The two live arms split on measurement: `on in_ev()` compiles and
/// runs (v1 synthesizes `_v` for a payload the body cannot name anyway),
/// while `on in_ev(t, u)` drops the extra parameter without a word.
#[test]
fn the_event_subscription_arms_are_two_live_ones_and_five_dead() {
    let agent = |trigger: &str, body: &str| {
        format!(
            r#"domain SysDomain
  freq_mhz: 100
end domain SysDomain

agent Tagger
    in_ev : event<uint<8>>
    seen  : uint<32> default 0
    other : uint<8>  default 0

    on {trigger}
        {body}
    end on
end agent Tagger

test T
    let dut    : Top
    let tagger : Tagger
    clock clk = SysDomain
    run
        wait 2 cycles
        emit tagger.in_ev(1)
        wait 2 cycles
        log(info, "seen={{}}", tagger.seen)
    end run
end test T"#
        )
    };

    // The five shapes the routing predicate excludes. None of them may
    // reach the subscription arms â€” each is claimed elsewhere first.
    lower_src(&agent("3 cycles", "seen = seen + 1")).expect("a periodic handler lowers");
    for (trigger, elsewhere) in [
        ("clk", "the unresolved name `clk`"),
        ("tagger.in_ev(t)", "transactor/method call `.in_ev(...)`"),
        ("other(t)", "helper call `other(...)`"),
        ("nosuch(t)", "helper call `nosuch(...)`"),
    ] {
        let msg = match lower_src(&agent(trigger, "seen = seen + 1")) {
            Ok(_) => panic!("`on {trigger}` unexpectedly lowered"),
            Err(e) => format!("{e:?}"),
        };
        assert!(
            msg.contains(elsewhere),
            "`on {trigger}` lands on `{elsewhere}`, not a subscription arm: {msg}"
        );
    }

    // No payload name: v1 synthesizes one and the handler runs, which
    // is what was written â€” the payload is simply unbound.
    let none = agent("in_ev()", "seen = seen + 1");
    let msg = assert_unsupported(&lower_src(&none).unwrap_err());
    assert!(msg.contains("no payload argument"), "{msg}");
    let v1 = cpp_tb::emit(&merged_src(&none)).expect("v1 emits");
    assert!(
        v1.contains("tagger.in_ev.push_back([&](uint64_t _v) {"),
        "v1 synthesizes a payload name: {v1}"
    );

    // A second payload name: v1 drops it without a word. Naming it in
    // the body does NOT reliably fail to compile â€” that was this arm's
    // first verdict and it is the lesser of the two things v1 does.
    // Give `u` something else to bind to and v1 compiles and runs to a
    // value the source never asked for, which is the worse one and so
    // the arm's label.
    for body in ["seen = seen + 1", "seen = seen + u"] {
        let two = agent("in_ev(t, u)", body);
        assert_not_implemented(
            &lower_src(&two).unwrap_err(),
            lower::V1Status::SilentlyMisLowers,
        );
        let v1 = cpp_tb::emit(&merged_src(&two)).expect("v1 emits");
        assert!(
            v1.contains("tagger.in_ev.push_back([&](uint64_t t) {") && !v1.contains("uint64_t u"),
            "v1 emits the lambda with only the first parameter: {v1}"
        );
    }
    // The two shapes that make the drop silent rather than loud. Each
    // resolves `u` to something that is NOT the payload, and the whole
    // point is that v1 says nothing about it.
    let shadowed = |extra_decl: &str, extra_field: &str| {
        format!(
            r#"domain SysDomain
  freq_mhz: 100
end domain SysDomain
{extra_decl}
agent Tagger
    in_ev : event<uint<8>>
    seen  : uint<32> default 0
{extra_field}
    on in_ev(t, u)
        seen = seen + u
    end on
end agent Tagger

test T
    let dut    : Top
    let tagger : Tagger
    clock clk = SysDomain
    run
        wait 2 cycles
        emit tagger.in_ev(1)
        wait 2 cycles
        log(info, "x")
    end run
end test T"#
        )
    };
    for (src, resolves_to) in [
        (
            shadowed("", "    u     : uint<8>  default 7"),
            "tagger.seen + tagger.u;",
        ),
        (shadowed("\nconst u = 9\n", ""), "tagger.seen + u;"),
    ] {
        assert_not_implemented(
            &lower_src(&src).unwrap_err(),
            lower::V1Status::SilentlyMisLowers,
        );
        let v1 = cpp_tb::emit(&merged_src(&src)).expect("v1 emits");
        assert!(
            v1.contains(resolves_to),
            "v1 silently binds `u` to `{resolves_to}`: {v1}"
        );
    }
    // â€¦and the one-argument control really does bind the name, so the
    // difference above is the dropped parameter and nothing else.
    let one = agent("in_ev(t)", "seen = seen + t");
    lower_src(&one).expect("the control lowers");
    assert_eq!(
        cpp_tb::emit(&merged_src(&one)).expect("v1 emits"),
        cpp_tb::emit(&merged_src(&agent("in_ev(t, u)", "seen = seen + t"))).expect("v1 emits"),
        "the extra parameter leaves no trace in v1's output"
    );
}

/// The four `helpers.rs` arms, and two of them were right already â€”
/// which is worth a test precisely because it was measured rather than
/// assumed.
///
/// | arm | v1 | verdict |
/// |---|---|---|
/// | a DUT/sync-touching helper call in a message | compiles; calls it at the failure site | `Unsupported` |
/// | a testbench method call in a message | compiles; same | `Unsupported` |
/// | a helper param of module type, non-DUT arg | `no match for call to <lambda(VTop*)> (Model&)` | `EmitsUncompilable` |
/// | a testbench method param of module type, non-DUT arg | `no match for call to <lambda(Tb&, VTop*)> (Tb&, Model&)` | `EmitsUncompilable` |
///
/// The first two only fire for a CONDITIONALLY-evaluated message â€” an
/// assert's `else fail(...)`. An unconditional `log(...)` hoists the
/// call ahead of the statement and lowers fine, so probing with a
/// `log` measures nothing at all; the routing gate is `lower_fmt` vs
/// `lower_fmt_hoisting`, not the arm.
#[test]
fn the_helper_arms_split_between_a_real_hatch_and_an_uncompilable_call() {
    let base = fixture("msg_call_hoist_test.harc");
    const LOG: &str = "        log(info, \"dbl=${dbl(v)}\")";
    assert!(base.contains(LOG), "fixture shape changed");

    // Conditionally-evaluated message: v1 evaluates the call at the
    // failure site, which is exactly the laziness TB-IR cannot inline.
    for (what, msg, needle) in [
        (
            "impure helper",
            "assert dut.count_out >= 0 else fail(\"cur=${cur_plus(dut, 1)}\")",
            "cur_plus(dut, 1)",
        ),
        (
            "testbench method",
            "assert dut.count_out >= 0 else fail(\"dbl=${dbl(v)}\")",
            "MsgCallHoistTb_dbl(_tb, v)",
        ),
    ] {
        let src = base.replacen(LOG, &format!("        {msg}"), 1);
        assert_ne!(src, base, "{what}: the message must actually change");
        let m = assert_unsupported(&lower_src(&src).unwrap_err());
        assert!(m.contains("inside a message"), "{what}: {m}");
        let v1 = cpp_tb::emit(&merged_src(&src)).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert!(
            v1.contains(&format!("sim_log_line(\"FAIL\"")) && v1.contains(needle),
            "{what}: v1 calls it at the failure site, which is what makes the suggestion honest"
        );
    }
    // A PURE helper in the same position lowers â€” the arm is about
    // CFG-inlined calls, not about messages.
    lower_src(&base.replacen(
        LOG,
        "        assert dut.count_out >= 0 else fail(\"inc=${inc(v)}\")",
        1,
    ))
    .expect("a pure helper in a conditional message still lowers");

    // A module-typed parameter given something that is not the DUT:
    // v1 types the lambda on the module and passes the argument
    // through, so the call does not compile.
    let helper = r#"domain SysDomain
  freq_mhz: 100
end domain SysDomain

agent Model
    v : uint<32> default 1
end agent Model

function touch(d: Top) -> uint<32>
    return d.count_out
end function touch

test T
    let dut : Top
    let m : Model
    clock clk = SysDomain
    run
        let s : uint<32> = touch(ARG)
        wait 1 cycle
    end run
end test T"#;
    lower_src(&helper.replace("ARG", "dut")).expect("the DUT argument lowers");
    let msg = assert_not_implemented(
        &lower_src(&helper.replace("ARG", "m")).unwrap_err(),
        lower::V1Status::EmitsUncompilable,
    );
    assert!(msg.contains("helper parameter `d`"), "{msg}");
    let v1 = cpp_tb::emit(&merged_src(&helper.replace("ARG", "m"))).expect("v1 emits");
    assert!(
        v1.contains("auto touch = [&](VTop* d)") && v1.contains("touch(m)"),
        "v1 passes a component to a `VTop*` lambda: {v1}"
    );

    // The testbench-method sibling, measured on its own.
    let method = r#"domain SysDomain
  freq_mhz: 100
end domain SysDomain

agent Model
    v : uint<32> default 1
end agent Model

testbench Tb
    dut : Top
    m   : Model
    function peek(d: Top) -> uint<32>
        return d.count_out
    end peek
end testbench Tb

impl T for Tb
    clock clk = SysDomain
    run
        let s : uint<32> = peek(ARG)
        wait 1 cycle
    end run
end impl T"#;
    lower_src(&method.replace("ARG", "dut")).expect("the DUT argument lowers");
    let msg = assert_not_implemented(
        &lower_src(&method.replace("ARG", "m")).unwrap_err(),
        lower::V1Status::EmitsUncompilable,
    );
    assert!(msg.contains("testbench method parameter `d`"), "{msg}");
    let v1 = cpp_tb::emit(&merged_src(&method.replace("ARG", "m"))).expect("v1 emits");
    assert!(
        v1.contains("Tb_peek(_tb, _tb.m)"),
        "v1 passes the component field through: {v1}"
    );
}

/// `when` subtype blocks stay outside the lowered record shape, and
/// this arm took three rounds of measurement because each of the first
/// two looked at exactly one shape.
///
/// | shape | v1 |
/// |---|---|
/// | `randomize(q)` on the transaction itself | emits the guard and gates the conditional field on it |
/// | the same subtype nested in another record, outer randomized | reaches it through an unconditional `randomize_Req` â€” no guard anywhere, and the field is not in the solve |
/// | in a struct | the field is gone from the emitted struct entirely |
///
/// The first round measured a program with no `randomize` at all, so
/// nothing about the guard was observable; the second measured the
/// direct path and called the arm an escape hatch. The nested path is
/// the worst of the three and so is the arm's label.
#[test]
fn record_when_subtype_splits_by_where_it_is_randomized_from() {
    let prog = |decl: &str, kind: &str| {
        format!(
            r#"{decl}

test WhenTest
    let dut : Top
    run
        let q : {kind}
        randomize(q)
        log(info, "x")
    end run
end test WhenTest
"#
        )
    };
    let txn = prog(
        "transaction Req\n    op : uint<2>\n    when op == 1\n        addr : uint<32>\n    \
         end when\nend transaction Req",
        "Req",
    );
    let msg = assert_not_implemented(
        &lower_src(&txn).unwrap_err(),
        lower::V1Status::SilentlyMisLowers,
    );
    assert!(msg.contains("`when` subtype"), "names the construct: {msg}");
    // DIRECTLY randomized, v1 emits the guard and gates the conditional
    // field on it. This half is why the arm was once called an escape
    // hatch, and it is real â€” it is just not the whole arm.
    let v1 = cpp_tb::emit(&merged_src(&txn)).expect("v1 emits");
    assert!(
        v1.contains("if (q.op == 1) {   // active when-subtype field addr"),
        "v1 emits the subtype guard: {v1}"
    );
    assert!(
        v1.contains("q.addr = _val_addr;"),
        "â€¦and assigns the conditional field only under it: {v1}"
    );
    // Reached through an OUTER record, the same subtype loses the guard
    // entirely. `randomize_Req` assigns the conditional field
    // unconditionally and the solver's problem table never mentions it.
    // That is the worst thing v1 does under this arm, so it is the
    // arm's label.
    let nested = prog(
        "transaction Req\n    op : uint<8>\n    when op == 1\n        addr : uint<16>\n    \
         end when\nend transaction Req\n\ntransaction Outer\n    tag : uint<8>\n    \
         inner : Req\nend transaction Outer",
        "Outer",
    );
    assert_not_implemented(
        &lower_src(&nested).unwrap_err(),
        lower::V1Status::SilentlyMisLowers,
    );
    let v1 = cpp_tb::emit(&merged_src(&nested)).expect("v1 emits");
    assert!(
        !v1.contains("op == 1"),
        "v1 drops the guard on the nested path: {v1}"
    );
    assert!(
        v1.contains("t->addr = harc_rt::random::harc_rng_uint(harc_rng_next, 16);"),
        "â€¦and draws the conditional field unconditionally: {v1}"
    );
    assert!(
        !v1.contains("inner.addr"),
        "â€¦and leaves it out of the solve entirely: {v1}"
    );

    // The struct spelling loses the field, which no `--codegen v1` run
    // recovers. Diffing against the control is the whole measurement.
    let st = prog(
        "struct Rec\n    a : uint<8>\n    when a == 1\n        b : uint<8>\n    end when\n\
         end struct Rec",
        "Rec",
    );
    let ctl = prog("struct Rec\n    a : uint<8>\nend struct Rec", "Rec");
    assert_not_implemented(
        &lower_src(&st).unwrap_err(),
        lower::V1Status::SilentlyMisLowers,
    );
    let with_when = cpp_tb::emit(&merged_src(&st)).expect("v1 emits");
    let without = cpp_tb::emit(&merged_src(&ctl)).expect("v1 emits the control");
    let structs = |src: &str| src.split("struct Rec {").nth(1).unwrap_or("")[..80].to_string();
    assert_eq!(
        structs(&with_when),
        structs(&without),
        "the conditional field is gone from v1's struct"
    );
}

/// Record locals cannot live in *pure* helpers (they emit as
/// scalar-only file-scope C++ functions in the tbir backend). Note the
/// body must stay inside the pure scan subset to reach this gate â€” a
/// field access would classify the helper impure and CFG-inline it,
/// where record locals are legal.
#[test]
fn record_let_in_pure_helper_is_unsupported() {
    let src = r#"
transaction Req
    addr : uint<32> default 9
end transaction Req

function mk() -> uint<32>
    let t : Req
    return 1
end function mk

test PureHelperTest
    let dut : Top
    run
        let x = mk()
    end run
end test PureHelperTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("pure helper"),
        "names the helper context: {msg}"
    );
}

#[test]
fn helper_call_with_dut_access_is_unsupported() {
    let src = r#"
test HelperTest
    let dut : Top
    run
        poke(dut, 1)
    end run
end test HelperTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("poke"), "names the helper: {msg}");
    assert!(msg.contains("--codegen v1"), "suggests v1: {msg}");
}

// â”€â”€ Helper functions: pure C++ calls vs CFG inlining â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Source with one impure helper (DUT access + `wait` + `return`),
/// called twice, plus one pure helper called from the run body.
const HELPER_MIX_SRC: &str = r#"
function read_addr(d: Top, addr: uint<3>) -> uint<8>
    d.rd_addr = addr
    d.rd_en = 1
    wait 1 cycle
    return d.rd_data
end function read_addr

function double_it(x: uint<8>) -> uint<8>
    return x + x
end function double_it

test HelperMixTest
    let dut : Top
    run
        assert read_addr(dut, 2) == 90 else fail("bad rom value")
        let d = double_it(read_addr(dut, 3))
        assert d == 236 else fail("bad doubled value")
    end run
end test HelperMixTest
"#;

/// Locks the inlined-CFG dump-ir text: the impure helper's body
/// (DutWrite / WaitCycles / DutRead-return) appears inline in the run
/// function with remapped blocks and deduplicated param locals, while
/// the pure helper stays a standalone `Helper` function invoked via
/// `Expr::Call`.
#[test]
fn helper_inline_dump_ir_snapshot() {
    let prog = lower_src(HELPER_MIX_SRC).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("helper_inline_dump_ir", format!("{prog}"));
}

/// Categorization: DUT/sync-touching helpers are inlined (no standalone
/// function); pure helpers lower once as `FunctionKind::Helper` and the
/// call site stays `Expr::Call(CallTarget::Helper, ...)`.
#[test]
fn helper_categorization_pure_vs_impure() {
    let prog = lower_src(HELPER_MIX_SRC).expect("lowers");
    let helper_fns: Vec<&ir::TbFunction> = prog
        .functions
        .iter()
        .filter(|f| f.kind == ir::FunctionKind::Helper)
        .collect();
    assert_eq!(helper_fns.len(), 1, "only the pure helper is standalone");
    assert_eq!(helper_fns[0].name, "double_it");
    assert!(
        helper_fns[0].ret.is_some(),
        "pure helper carries a ret slot"
    );

    // The run body inlined read_addr (WaitCyclesSync from the helper
    // body â€” inlined waits take v1's synchronous lambda path) and
    // calls double_it by name.
    let run = prog.function(prog.tests[0].run);
    let waits = run
        .blocks
        .iter()
        .filter(|b| matches!(b.terminator, ir::Terminator::WaitCyclesSync(..)))
        .count();
    assert_eq!(waits, 2, "one inlined wait per read_addr call:\n{run}");
    let calls_double_it = run.blocks.iter().any(|b| {
        b.stmts.iter().any(|s| {
            matches!(s, ir::Stmt::Assign(_, e)
                if format!("{:?}", e).contains("Helper(\"double_it\")"))
        })
    });
    assert!(calls_double_it, "pure call survives as Expr::Call:\n{run}");
}

/// Param remapping: each inline site gets fresh locals for the helper's
/// params â€” two calls must not share the `addr` slot.
#[test]
fn helper_inline_param_remapping() {
    let prog = lower_src(HELPER_MIX_SRC).expect("lowers");
    let run = prog.function(prog.tests[0].run);
    let addr_locals: Vec<&str> = run
        .locals
        .iter()
        .map(|l| l.name.as_str())
        .filter(|n| n.starts_with("addr"))
        .collect();
    assert_eq!(
        addr_locals,
        vec!["addr", "addr_2"],
        "each inline site declares its own param local:\n{run}"
    );
}

/// Recursion through a DUT/sync-touching helper is rejected with the
/// cycle path: such a helper is CFG-inlined at each call site, and
/// inlining a cycle does not terminate. v1 has no answer here either â€”
/// it emits every helper as an `auto` lambda, which cannot name itself
/// in its own initializer â€” so the diagnostic must not point at v1.
#[test]
fn recursion_through_an_impure_helper_is_rejected() {
    let src = r#"
function spin(d: Top) -> uint<8>
    return spin(d)
end function spin

test RecTest
    let dut : Top
    run
        let x = spin(dut)
    end run
end test RecTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_not_implemented(&err, lower::V1Status::EmitsUncompilable);
    assert!(msg.contains("DUT/sync-touching helper"), "{msg}");
    assert!(msg.contains("spin -> spin"), "names the cycle: {msg}");
}

/// A PURE helper emits as a file-scope C++ function with its prototype
/// ahead of every body, so it can call itself â€” direct or mutual. v1
/// cannot express this at all (its helpers are `auto` lambdas), which is
/// why the recursion check now runs AFTER the purity fixpoint rather
/// than before it.
#[test]
fn pure_helper_recursion_lowers() {
    let direct = r#"
function fact(n: uint<8>) -> uint<32>
    if n <= 1
        return 1
    end if
    return n * fact(n - 1)
end function fact

test RecTest
    let dut : Top
    run
        let x = fact(5)
        assert x == 120 else fail("fact")
    end run
end test RecTest
"#;
    let prog = lower_src(direct).expect("a pure recursive helper lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = emit_cpp_src(direct);
    assert!(
        cpp.contains("static uint64_t harc_helper_fact(uint64_t n);"),
        "the prototype must precede the body so the self-call resolves; got:\n{cpp}"
    );
    assert!(
        cpp.contains("harc_helper_fact((n - 1))"),
        "the self-call must emit as a real call; got:\n{cpp}"
    );

    // Mutual recursion among pure helpers is the same story â€” every
    // prototype is emitted before any body.
    let mutual = r#"
function ping(x: uint<8>) -> uint<8>
    if x == 0
        return 0
    end if
    return pong(x - 1)
end function ping

function pong(x: uint<8>) -> uint<8>
    return ping(x)
end function pong

test MutRecTest
    let dut : Top
    run
        dut.en = ping(3)
    end run
end test MutRecTest
"#;
    let prog = lower_src(mutual).expect("mutually recursive pure helpers lower");
    verify::verify_program(&prog).expect("verifies");
}

/// An impure helper call inside a `${...}` message capture cannot be
/// inlined (messages evaluate lazily at the failure site).
#[test]
fn helper_impure_call_in_message_is_unsupported() {
    let src = r#"
function peek(d: Top) -> uint<8>
    wait 1 cycle
    return d.rd_data
end function peek

test FmtTest
    let dut : Top
    run
        assert dut.rd_data == 0 else fail("got ${peek(dut)}")
    end run
end test FmtTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("peek"), "names the helper: {msg}");
    assert!(msg.contains("--codegen v1"), "suggests v1: {msg}");
}

/// `break` inside an inlined helper body must not bind to a loop open
/// at the call site â€” helpers are free functions.
#[test]
fn helper_inline_break_cannot_bind_caller_loop() {
    let src = r#"
function bail(d: Top)
    d.en = 0
    break
end function bail

test BreakTest
    let dut : Top
    run
        for i in 0 .. 4
            bail(dut)
        end for
    end run
end test BreakTest
"#;
    let err = lower_src(src).unwrap_err();
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "break outside a loop is a structural error: {err:?}"
    );
    assert!(err.to_string().contains("break"), "{err}");
}

/// tbir emission for the helper mix: the pure helper becomes a
/// file-scope C++ function; the impure helper's wait shows up as a
/// synchronous tick loop in the run coroutine (CFG-inlined, not a
/// call â€” and sync, not co_await, mirroring v1's lambda-body waits).
#[test]
fn tbir_emit_helper_mix() {
    let merged = merged_src(HELPER_MIX_SRC);
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits");
    for marker in [
        "static uint64_t harc_helper_double_it(uint64_t x);",
        "static uint64_t harc_helper_double_it(uint64_t x) {",
        "harc_helper_double_it(__t",
        "for (int _w = 0; _w < 1; _w++) tick();",
    ] {
        assert!(cpp.contains(marker), "missing marker `{marker}` in:\n{cpp}");
    }
    assert!(
        !cpp.contains("read_addr"),
        "impure helper must be fully inlined, not emitted as a function"
    );
}

// â”€â”€ `wait N cycles on <clock>` â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

const WAIT_ON_CLOCK_SRC: &str = r#"
test WaitOnClockTest
    let dut : Top
    clock clk = 10ns
    clock aux_clk = 4ns
    run
        wait 2 cycles on aux_clk
        dut.en = 1
    end run
end test WaitOnClockTest
"#;

/// A clock-qualified wait lowers to `WaitCycles` carrying the resolved
/// `WaitClock` (declaration-order index == runtime scheduler index);
/// the dump-ir text names the clock; the lower_coroutine trigger
/// renders it too.
#[test]
fn wait_on_clock_lowers_with_clock_qualifier() {
    let prog = lower_src(WAIT_ON_CLOCK_SRC).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let f = prog.function(prog.tests[0].run);
    let ir::Terminator::WaitCycles(_, Some(clock), _) = &f.blocks[0].terminator else {
        panic!("expected clock-qualified WaitCycles terminator:\n{f}");
    };
    assert_eq!(clock.name, "aux_clk");
    assert_eq!(
        clock.index, 1,
        "declaration-order index into TestSchema::clocks"
    );
    assert!(
        format!("{f}").contains("WaitCycles(2 on aux_clk, b1)"),
        "display names the clock:\n{f}"
    );
    let meta = lower_coroutine::run(&prog).expect("tags");
    assert!(
        format!("{}", meta.display(&prog)).contains("wait_cycles(2 on aux_clk)"),
        "pass trigger names the clock:\n{}",
        meta.display(&prog)
    );
}

/// An unknown clock after `on` is a structured lowering error naming
/// the clock and the declared ones (v1 deferred this to emission).
#[test]
fn wait_on_unknown_clock_is_invalid() {
    let src = r#"
test WaitBadClockTest
    let dut : Top
    clock clk = 10ns
    clock aux_clk = 4ns
    run
        wait 1 cycle on nope
    end run
end test WaitBadClockTest
"#;
    let err = lower_src(src).unwrap_err();
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "unknown clock is Invalid, not Unsupported: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("no clock named `nope`"),
        "names the clock: {msg}"
    );
    assert!(
        msg.contains("declared clocks: clk, aux_clk"),
        "lists the declared clocks: {msg}"
    );
}

/// The verifier cross-checks every clock-qualified wait against the
/// test's declared clocks: an out-of-range index (which codegen would
/// turn into an out-of-bounds `clocks_[i]` access) or an index/name
/// disagreement is a programmer-error verify failure.
#[test]
fn verifier_catches_bad_wait_clock() {
    let prog = lower_src(WAIT_ON_CLOCK_SRC).expect("lowers");
    let run_idx = prog.tests[0].run.index();

    let mut broken = prog.clone();
    for b in &mut broken.functions[run_idx].blocks {
        if let ir::Terminator::WaitCycles(_, Some(wc), _) = &mut b.terminator {
            wc.index = 7; // out of range â€” only 2 clocks declared
        }
    }
    let errs = verify::verify_program(&broken).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| e.to_string().contains("only 2 clock(s) are declared")),
        "{errs:?}"
    );

    let mut broken = prog;
    for b in &mut broken.functions[run_idx].blocks {
        if let ir::Terminator::WaitCycles(_, Some(wc), _) = &mut b.terminator {
            wc.index = 0; // valid slot, but it is `clk`, not `aux_clk`
        }
    }
    let errs = verify::verify_program(&broken).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| e.to_string().contains("that slot is `clk`")),
        "{errs:?}"
    );
}

/// tbir emission of a clock-qualified wait mirrors v1's inline
/// eval_clocks_until loop (no coroutine yield): advance to whichever
/// clock's next edge is sooner until the named clock has seen N more
/// rising edges, then run the checkers.
#[test]
fn tbir_emit_wait_on_clock_inline_loop() {
    let merged = merged_src(WAIT_ON_CLOCK_SRC);
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits");
    for marker in [
        "{ long long _target = clocks_[1].rising_count + (long long)(2); \
         while (clocks_[1].rising_count < _target) {",
        "long long _next = clocks_[0].next_edge_ps;",
        "for (auto& _ck : clocks_) if (_ck.next_edge_ps < _next) _next = _ck.next_edge_ps;",
        "eval_clocks_until(_next);",
        "} for (auto& _c : _checkers) _c(); }",
    ] {
        assert!(
            cpp.contains(marker),
            "missing wait-on-clock marker `{marker}` in:\n{cpp}"
        );
    }
    assert!(
        !cpp.contains("co_await harc_rt::wait_cycles"),
        "clock-qualified wait must not yield to the scheduler (v1 parity)"
    );
}

/// Core lowering shape: a `for` loop becomes init / header-branch /
/// body / latch / exit, with the counter init outside the loop.
#[test]
fn for_loop_lowers_to_header_latch_exit() {
    let src = r#"
test LoopTest
    let dut : Top
    run
        for i in 0 .. 4
            dut.en = 1
        end for
    end run
end test LoopTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let f = prog.function(prog.tests[0].run);
    // init block jumps to a header that branches on `i < 4`.
    let n_branches = f
        .blocks
        .iter()
        .filter(|b| matches!(b.terminator, ir::Terminator::Branch(..)))
        .count();
    assert_eq!(n_branches, 1, "one loop header:\n{f}");
    let has_latch = f.blocks.iter().any(|b| {
        matches!(b.terminator, ir::Terminator::Jump(_))
            && b.stmts
                .iter()
                .any(|s| matches!(s, ir::Stmt::Assign(_, ir::Expr::Binary(ir::BinOp::Add, ..))))
    });
    assert!(has_latch, "latch block increments the counter:\n{f}");
}

/// DUT reads hoist into `DutRead` temps everywhere except the allowed
/// port positions (assert conds, format args, DutWrite values).
#[test]
fn dut_read_in_let_hoists_to_dut_read_stmt() {
    let src = r#"
test HoistTest
    let dut : Top
    run
        let doubled = dut.count_out + dut.count_out
    end run
end test HoistTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let f = prog.function(prog.tests[0].run);
    let reads = f.blocks[0]
        .stmts
        .iter()
        .filter(|s| matches!(s, ir::Stmt::DutRead(..)))
        .count();
    assert_eq!(reads, 2, "both port reads hoisted:\n{f}");
}

/// The verifier rejects programs with dangling successors and
/// use-before-def locals (programmer-error net under IR mutation).
#[test]
fn verifier_catches_bad_successor_and_use_before_def() {
    let prog = lower_src(&fixture("top_counter_test.harc")).expect("lowers");
    let mut broken = prog.clone();
    // Dangling successor.
    let f = &mut broken.functions[0];
    if let Some(b) = f.blocks.first_mut() {
        b.terminator = ir::Terminator::Jump(ir::BlockId(9999));
    }
    let errs = verify::verify_program(&broken).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| matches!(e, verify::VerifyError::BadSuccessor { .. })),
        "{errs:?}"
    );

    // Use-before-def: read a fresh local that nothing assigns.
    let mut broken = prog.clone();
    let f = &mut broken.functions[0];
    let ghost = ir::LocalId(f.locals.len() as u32);
    f.locals.push(ir::TypedLocal {
        name: "ghost".to_string(),
        ty: ir::IrType::Unknown,
    });
    f.blocks[0].stmts.insert(
        0,
        ir::Stmt::DutWrite(
            ir::PortRef {
                testbench_field: "dut".to_string(),
                port_path: vec!["en".to_string()],
                aggregate_path: false,
                direction: None,
                width: None,
                access: ir::PortAccess::Port,
                lane: None,
            },
            ir::Expr::Local(ghost),
        ),
    );
    let errs = verify::verify_program(&broken).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| matches!(e, verify::VerifyError::LocalUseBeforeDef { .. })),
        "{errs:?}"
    );
}

/// The port-position rule: an `Expr::Port` inside an Assign value is a
/// verify error (lowering must hoist it).
#[test]
fn verifier_rejects_port_in_assign_value() {
    let prog = lower_src(&fixture("top_counter_test.harc")).expect("lowers");
    let mut broken = prog;
    let f = &mut broken.functions[0];
    let l = ir::LocalId(0);
    f.blocks[0].stmts.push(ir::Stmt::Assign(
        l,
        ir::Expr::Port(ir::PortRef {
            testbench_field: "dut".to_string(),
            port_path: vec!["count_out".to_string()],
            aggregate_path: false,
            direction: None,
            width: None,
            access: ir::PortAccess::Port,
            lane: None,
        }),
    ));
    let errs = verify::verify_program(&broken).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| matches!(e, verify::VerifyError::PortInDisallowedPosition { .. })),
        "{errs:?}"
    );
}

/// tbir emission carries the v1 scaffolding contract markers: context
/// struct, seed log, coroutine slot, loop-switch, dispatcher main.
#[test]
fn tbir_emit_scaffolding_contract() {
    let merged = merged_src(&fixture("top_counter_test.harc"));
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits");
    for marker in [
        "#include \"VTop.h\"",
        "struct HarcTestContext {",
        "struct TopCounterTb {",
        "sim_log_line(\"INFO\", \"seed=%llu\", (long long)harc_rng.state);",
        "harc_rt::trace::harc_start_trace(trace, harc_rng.state, \"Top\", \"TopCounterTest\", cycle_count);",
        "harc_rt::ThreadSlot _run_slot;",
        "co_await harc_rt::wait_cycles(_slot, (uint32_t)(3));",
        "int __bb = 0;",
        "while (!__done) {",
        "_tb.dut = dut;",
        "clocks_.push_back(ClockState{\"clk\", 5000, 5000, 0, 0});",
        "return harc_rt::log::harc_finish_sim_run(log_ctx, trace, cycle_count, errors);",
        "int main(int argc, char** argv) {",
        "if (std::strcmp(test_sel, \"TopCounterTest\") == 0) return run_TopCounterTest(argc, argv);",
    ] {
        assert!(cpp.contains(marker), "missing scaffolding marker `{marker}`");
    }
}

/// The harness's divergence detector (`harc trace-diff` wraps this):
/// one changed log line between two otherwise identical JSONL traces
/// must surface as a divergence.
#[test]
fn trace_diff_flags_single_log_line_change() {
    let a = r#"{"type":"meta","seq":0,"dut_backend":"verilator","top":"Top","test":"T","seed":1}
{"type":"log","seq":1,"cycle":3,"severity":"INFO","message":"PASS: counter counts"}
{"type":"log","seq":2,"cycle":7,"severity":"INFO","message":"PASS: counter holds"}
{"type":"sim_end","seq":3,"cycle":9,"errors":0}
"#;
    let b = a.replace("PASS: counter holds", "FAIL: counter wedged");
    let divs = harc::check_backends::diff_trace_strings(a, &b).expect("diff runs");
    assert_eq!(divs.len(), 1, "exactly the changed line diverges: {divs:?}");
    assert_eq!(divs[0].event_type, "log");
    assert_eq!(divs[0].cycle, Some(7));
    assert!(divs[0].arch_line.contains("counter holds"), "{divs:?}");
    assert!(divs[0].sv_line.contains("counter wedged"), "{divs:?}");
}

/// Backend-implementation noise (`seq` numbering) must NOT count as
/// divergence: traces identical modulo `seq` compare clean.
#[test]
fn trace_diff_ignores_seq_field() {
    let a = r#"{"type":"log","seq":1,"cycle":3,"severity":"INFO","message":"PASS: counter counts"}
{"type":"sim_end","seq":2,"cycle":9,"errors":0}
"#;
    let b = r#"{"type":"log","seq":41,"cycle":3,"severity":"INFO","message":"PASS: counter counts"}
{"type":"sim_end","seq":42,"cycle":9,"errors":0}
"#;
    let divs = harc::check_backends::diff_trace_strings(a, b).expect("diff runs");
    assert!(divs.is_empty(), "seq-only differences are noise: {divs:?}");
}

/// Locks the dump-ir text for the covergroup fixture: covgroup schema
/// (points, bins, trigger), the testbench cov field, the synthesized
/// SamplerAuto function, and check-phase CovReport / CovBin reads.
#[test]
fn sync_fifo_dump_ir_snapshot() {
    let prog = lower_src(&fixture("sync_fifo_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("sync_fifo_dump_ir", format!("{prog}"));
}

/// Bin-spec lowering: set literals and bare integer literals flatten
/// into the schema's finite value sets, in declaration order.
#[test]
fn covergroup_bin_specs_lower_to_value_sets() {
    let src = r#"
covergroup Cov @(posedge dut.clk)
    cp_mode : cover dut.mode
        bins
            idle = {0}
            busy = {1, 2, 3}
            hexy = {0x10, 0b101}
        end bins
end covergroup Cov

testbench Tb
    dut : Top
    cov : Cov
end testbench Tb

impl CovTest for Tb
    run
        dut.en = 1
        wait 1 cycle
    end run
    check
        cov.report()
        assert cov.cp_mode.idle > 0 else fail("idle hole")
    end check
end impl CovTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    assert_eq!(prog.covgroups.len(), 1);
    let cg = &prog.covgroups[0];
    assert_eq!(cg.name, "Cov");
    assert_eq!(cg.points.len(), 1);
    let p = &cg.points[0];
    assert_eq!(p.name, "cp_mode");
    match &p.target {
        ir::Expr::Port(port) => assert_eq!(port.port_path, vec!["mode".to_string()]),
        other => panic!("expected port-backed coverpoint target, got {other:?}"),
    }
    assert_eq!(
        bins_repr(p),
        vec![
            ("idle", vec![("eq", Some(0), None)]),
            (
                "busy",
                vec![
                    ("eq", Some(1), None),
                    ("eq", Some(2), None),
                    ("eq", Some(3), None)
                ]
            ),
            (
                "hexy",
                vec![("eq", Some(0x10), None), ("eq", Some(0b101), None)]
            ),
        ]
    );
    // Testbench schema records the cov field; lowering synthesized one
    // SamplerAuto bound to the same covgroup.
    let tb = prog.testbench(prog.tests[0].testbench);
    assert_eq!(tb.cov_fields, vec![("cov".to_string(), ir::CovgroupId(0))]);
    let samplers: Vec<_> = prog
        .functions
        .iter()
        .filter(|f| matches!(f.kind, ir::FunctionKind::SamplerAuto { .. }))
        .collect();
    assert_eq!(samplers.len(), 1);
}

/// Bin specs may use file-scope consts and enum variants anywhere a literal
/// value is accepted. This keeps default TBIR aligned with v1's constexpr-style
/// covergroup bin emission.
#[test]
fn covergroup_bin_specs_lower_const_and_enum_values() {
    let src = r#"
const IDLE : uint<4> = 0
const BUSY : uint<4> = 3

enum Mode { RED, GREEN, BLUE }

covergroup Cov @(posedge dut.clk)
    cp_mode : cover dut.mode
        bins
            idle = {IDLE}
            mix = {GREEN, BUSY}
            range = [IDLE..BLUE]
        end bins
end covergroup Cov

testbench Tb
    dut : Top
    cov : Cov
end testbench Tb

impl CovTest for Tb
    run
        wait 1 cycle
    end run
end impl CovTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    assert_eq!(
        bins_repr(&prog.covgroups[0].points[0]),
        vec![
            ("idle", vec![("eq", Some(0), None)]),
            ("mix", vec![("eq", Some(1), None), ("eq", Some(3), None)]),
            ("range", vec![("range", Some(0), Some(2))]),
        ]
    );
}

/// Range bin specs lower to inclusive `CovBinValue::Range` entries:
/// closed (`[a..b]`), open-low (`[..b]`), and the set-of-ranges mix
/// (`{[1..3], 7}`). (Open-high `[a..]` does not parse â€” the `..` infix
/// requires a right operand; only the bracket-prefix `[..b]`/`[..]`
/// forms produce open bounds.) Bounds match v1's hit test
/// (`_v >= lo && _v <= hi` â€” inclusive on both ends).
#[test]
fn covergroup_range_bins_lower() {
    let src = r#"
covergroup Cov @(posedge dut.clk)
    cp_mode : cover dut.mode
        bins
            closed   = [4..9]
            openlow  = [..3]
            mixed    = {[1..3], 7}
        end bins
end covergroup Cov

testbench Tb
    dut : Top
    cov : Cov
end testbench Tb

impl CovTest for Tb
    run
        wait 1 cycle
    end run
end impl CovTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    assert_eq!(
        bins_repr(&prog.covgroups[0].points[0]),
        vec![
            ("closed", vec![("range", Some(4), Some(9))]),
            ("openlow", vec![("range", None, Some(3))]),
            (
                "mixed",
                vec![("range", Some(1), Some(3)), ("eq", Some(7), None)]
            ),
        ]
    );
}

/// #494 P2c: a range bound may be a genuine RUNTIME expression (a DUT
/// port, or an expression over one), not just a compile-time constant.
/// Lowering keeps the constant fast path (`Const`) and carries the
/// non-constant bound as a lowered `Expr` (`Runtime`); TBIR emission
/// renders it with the same expression lowerer used for point targets,
/// mirroring v1's per-sample `emit_expr(bound)`.
#[test]
fn covergroup_runtime_range_bounds_lower_and_emit() {
    let prog = lower_src(&fixture("cov_runtime_bound_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cp_count = &prog.covgroups[0].points[0];
    assert_eq!(cp_count.name, "cp_count");
    // `sel_lo = [dut.en .. 7]` â€” runtime LOW bound (Port), const HIGH bound.
    let sel_lo = &cp_count.bins[0];
    assert_eq!(sel_lo.name, "sel_lo");
    match &sel_lo.values[0] {
        ir::CovBinValue::Range { lo, hi } => {
            assert!(
                matches!(lo, Some(ir::CovBinBound::Runtime(ir::Expr::Port(_)))),
                "sel_lo low bound must be a runtime port read, got {lo:?}"
            );
            assert!(
                matches!(hi, Some(ir::CovBinBound::Const(7))),
                "sel_lo high bound must fold to const 7, got {hi:?}"
            );
        }
        other => panic!("sel_lo must be a Range, got {other:?}"),
    }
    // `sel_expr = [(dut.en + 4) .. 15]` â€” runtime LOW bound is a Binary
    // over the port; HIGH folds to a constant.
    let sel_expr = &cp_count.bins[1];
    assert_eq!(sel_expr.name, "sel_expr");
    match &sel_expr.values[0] {
        ir::CovBinValue::Range { lo, hi } => {
            assert!(
                matches!(lo, Some(ir::CovBinBound::Runtime(ir::Expr::Binary(..)))),
                "sel_expr low bound must be a runtime binary expr, got {lo:?}"
            );
            assert!(matches!(hi, Some(ir::CovBinBound::Const(15))));
        }
        other => panic!("sel_expr must be a Range, got {other:?}"),
    }
    // `const_hi = [8 .. 15]` stays fully constant (unchanged fast path).
    assert_eq!(
        bins_repr(cp_count)[2],
        ("const_hi", vec![("range", Some(8), Some(15))])
    );

    // Emission: the runtime bound reads the DUT port at sample time. The
    // `sel_lo` membership must compare `_v` against a live `dut->en` read,
    // exactly as v1 emits it (byte-for-byte per the equivalence gate).
    let cpp = emit_fixture_cpp("cov_runtime_bound_test.harc");
    assert!(
        cpp.contains("_v >= harc_rt::harc_read(dut->en)"),
        "runtime `sel_lo` low bound must emit a live port read; got:\n{cpp}"
    );
}

/// Width-method coverpoint targets carry source widths when they can be
/// inferred, so signed extension samples the same values as ordinary
/// TBIR expressions.
#[test]
fn covergroup_width_methods_lower_with_source_width() {
    let src = r#"
covergroup Cov @(posedge dut.clk)
    cp_signed : cover dut.mode[3:0].sext<8>()
        bins
            neg_one = {255}
        end bins
end covergroup Cov

testbench Tb
    dut : Top
    cov : Cov
end testbench Tb

impl CovTest for Tb
    run
        wait 1 cycle
    end run
end impl CovTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let target = &prog.covgroups[0].points[0].target;
    let ir::Expr::WidthCast {
        kind,
        width,
        src_width,
        ..
    } = target
    else {
        panic!("expected WidthCast target, got {target:?}");
    };
    assert_eq!(*kind, ir::WidthCastKind::Sext);
    assert_eq!(*width, 8);
    assert_eq!(*src_width, Some(4));
}

/// Covergroup width-method targets use the same basic guardrails as the
/// ordinary TBIR expression path: no zero widths, no >64-bit widths, and
/// no wrong-direction casts when the source width is known.
#[test]
fn covergroup_width_methods_reject_bad_widths() {
    let src_zero = r#"
covergroup Cov @(posedge dut.clk)
    cp_bad : cover dut.mode[3:0].trunc<0>()
end covergroup Cov

test BadWidthZero
    let dut : Top
    let cov : Cov
    run
        wait 1 cycle
    end run
end test
"#;
    let err = lower_src(src_zero).unwrap_err();
    assert!(
        err.to_string().contains("width must be greater than zero"),
        "{err}"
    );

    let src_wide = r#"
covergroup Cov @(posedge dut.clk)
    cp_bad : cover dut.mode[3:0].zext<65>()
end covergroup Cov

test BadWidthWide
    let dut : Top
    let cov : Cov
    run
        wait 1 cycle
    end run
end test
"#;
    let err = lower_src(src_wide).unwrap_err();
    assert_unsupported(&err);

    let src_wrong_direction = r#"
covergroup Cov @(posedge dut.clk)
    cp_bad : cover dut.mode[3:0].zext<2>()
end covergroup Cov

test BadWidthDirection
    let dut : Top
    let cov : Cov
    run
        wait 1 cycle
    end run
end test
"#;
    let err = lower_src(src_wrong_direction).unwrap_err();
    // v1's sentence verbatim, `â‰¥` and suggestion included â€” the
    // covergroup path used to print an ASCII paraphrase while claiming
    // to quote v1.
    assert!(
        err.to_string()
            .contains("width must be â‰¥ the source width (otherwise it narrows, wrong direction). Use `.trunc<2>()` to narrow."),
        "{err}"
    );
}

/// #494 P2c: a range bound that reads a DUT port (`[dut.lo..9]`) lowers
/// to a runtime bound carrying the port `Expr`, matching v1 (which emits
/// the raw bound per sample). Constant bounds keep folding.
#[test]
fn covergroup_dut_port_range_bound_lowers_runtime() {
    let src = r#"
covergroup Cov @(posedge dut.clk)
    cp_mode : cover dut.mode
        bins
            bad = [dut.lo..9]
        end bins
end covergroup Cov

testbench Tb
    dut : Top
    cov : Cov
end testbench Tb

impl CovTest for Tb
    run
        wait 1 cycle
    end run
end impl CovTest
"#;
    let prog = lower_src(src).expect("runtime range bound now lowers");
    verify::verify_program(&prog).expect("verifies");
    match &prog.covgroups[0].points[0].bins[0].values[0] {
        ir::CovBinValue::Range { lo, hi } => {
            match lo {
                Some(ir::CovBinBound::Runtime(ir::Expr::Port(p))) => {
                    assert_eq!(p.port_path, vec!["lo".to_string()]);
                }
                other => panic!("low bound must be a runtime port read, got {other:?}"),
            }
            assert!(matches!(hi, Some(ir::CovBinBound::Const(9))));
        }
        other => panic!("expected a Range bin, got {other:?}"),
    }
}

/// The axilite_cov fixture (range bins + declared cross + randomize)
/// previously tripped on its range bins; those now lower, so the
/// rejection shifted to the first construct still out of subset â€” the
/// cross-file `axil_write(...)` helper call (the fixture's helper and
/// `RegData` transaction live in axilite_regs_test.harc). When helpers
/// across registries land, this shifts again to `randomize`.
#[test]
fn axilite_cov_fixture_still_unsupported() {
    let err = lower_src(&fixture("axilite_cov_test.harc")).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(msg.contains("axil_write"), "names the helper call: {msg}");
}

/// Declared `cross` items lower into `CovgroupSchema::crosses`,
/// resolving point names to indices and keeping the item position
/// (v1's storage-name discriminator).
#[test]
fn covergroup_cross_lowers_to_schema() {
    let src = r#"
covergroup Cov @(posedge dut.clk)
    cp_a : cover dut.a
        bins
            hi = {1}
        end bins
    cp_b : cover dut.b
        bins
            hi = {1}
            lo = {0}
        end bins
    cross cp_a, cp_b
end covergroup Cov

testbench Tb
    dut : Top
    cov : Cov
end testbench Tb

impl CovTest for Tb
    run
        wait 1 cycle
    end run
end impl CovTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cg = &prog.covgroups[0];
    assert_eq!(cg.crosses.len(), 1);
    // Item index 2: the cross follows the two point items.
    assert_eq!(cg.crosses[0].item_index, 2);
    assert_eq!(cg.crosses[0].point_indices, vec![0, 1]);
}

/// A cross naming an unknown coverpoint is a hard lowering error
/// (v1 pushes the same complaint into its emission error list).
#[test]
fn covergroup_cross_unknown_point_is_invalid() {
    let src = r#"
covergroup Cov @(posedge dut.clk)
    cp_a : cover dut.a
        bins
            hi = {1}
        end bins
    cross cp_a, cp_nope
end covergroup Cov

testbench Tb
    dut : Top
    cov : Cov
end testbench Tb

impl CovTest for Tb
    run
        wait 1 cycle
    end run
end impl CovTest
"#;
    let err = lower_src(src).unwrap_err();
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "unknown cross point is Invalid, not Unsupported: {err:?}"
    );
    assert!(err.to_string().contains("cp_nope"), "{err}");
}

/// A check-phase read of an unknown point or bin is a hard lowering
/// error (v1 deferred this to a C++ compile failure).
#[test]
fn covergroup_unknown_bin_read_is_invalid() {
    let src = r#"
covergroup Cov @(posedge dut.clk)
    cp_mode : cover dut.mode
        bins
            idle = {0}
        end bins
end covergroup Cov

testbench Tb
    dut : Top
    cov : Cov
end testbench Tb

impl CovTest for Tb
    run
        wait 1 cycle
    end run
    check
        assert cov.cp_mode.nope > 0 else fail("hole")
    end check
end impl CovTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = err.to_string();
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "unknown bin is Invalid, not Unsupported: {err:?}"
    );
    assert!(msg.contains("nope"), "names the bin: {msg}");
}

/// tbir emission carries the covergroup contract markers: the struct
/// with bin counters and auto-cross matrix, report() print calls, the
/// `_checkers` sampler registration, and the check-phase report/bin
/// reads â€” all shapes that must match v1's runtime-observable output.
#[test]
fn tbir_emit_covergroup_contract() {
    let merged = merged_src(&fixture("sync_fifo_test.harc"));
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits");
    for marker in [
        "struct FifoCov {",
        "uint64_t yes = 0;",
        "} cp_empty;",
        "uint64_t _auto_cross_cp_empty__cp_full[2][2] = {};",
        "harc_rt::log::harc_print_covergroup_summary(\"FifoCov\", _hit, _total);",
        "harc_rt::log::harc_cov_json_summary(\"FifoCov\", _hit, _total);",
        "harc_rt::log::harc_print_covergroup_bin(\"cp_full\", \"no\", cp_full.no);",
        "harc_rt::log::harc_cov_json_bin(\"FifoCov\", \"cp_full\", \"no\", cp_full.no);",
        "harc_rt::log::harc_print_covergroup_cross_summary(\"FifoCov\", \"auto_cross\", \"cp_empty x cp_full\", _cross_hit, 4);",
        "harc_rt::log::harc_cov_json_cross_summary(\"FifoCov\", \"auto_cross\", \"cp_empty x cp_full\", _cross_hit, 4);",
        "FifoCov cov;",
        "_checkers.push_back([&]() {",
        "uint64_t _v = (uint64_t)(harc_rt::harc_read(dut->empty));",
        "if (((_v == 1))) { _tb.cov.cp_empty.yes++; _cg_hit_cp_empty[0] = true; }",
        "if (_cg_hit_cp_empty[_i] && _cg_hit_cp_full[_j]) _tb.cov._auto_cross_cp_empty__cp_full[_i][_j]++;",
        "_tb.cov.report();",
        "if (!((_tb.cov.cp_empty.yes > 0))) {",
    ] {
        assert!(cpp.contains(marker), "missing covergroup marker `{marker}`");
    }
}

/// Hook-triggered covergroups may classify sampled hook arguments through a
/// pure helper. This pins the default-TBIR path for `cover f(t.field, cycle)`,
/// where `cycle` is a bare scalar hook parameter rather than a record field.
#[test]
fn covergroup_hook_target_pure_helper_call_lowers() {
    let src = r#"
transaction Txn
    latency : uint<2>
end transaction Txn

function obs_latency(latency: uint<2>, cycle_seen: uint<8>) -> uint<3>
    if cycle_seen < 3
        return 0
    end if
    if latency == 1
        return 1
    end if
    return 2
end function obs_latency

scoreboard Sb
    hookable observe(t: Txn, cycle_seen: uint<8>)
    end observe
end scoreboard Sb

covergroup ObsCov @(sb.observe(t, cycle_seen) post)
    cp_obs : cover obs_latency(t.latency, cycle_seen)
        bins
            short = {0}
            medium = {1}
            long = {2}
        end bins
end covergroup ObsCov

testbench Tb
    dut : Top
    sb  : Sb
    sb2 : Sb
    cov : ObsCov
end testbench Tb

impl HookHelperCovTest for Tb
    run
        wait 1 cycle
    end run
end impl HookHelperCovTest
"#;
    let merged = merged_src(src);
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let dump = format!("{prog}");
    assert!(
        dump.contains("point cp_obs <- obs_latency(t.latency, cycle_seen):"),
        "coverpoint should preserve helper call over record and bare hook args: {dump}"
    );
    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits");
    assert!(
        cpp.contains("uint64_t harc_helper_obs_latency(")
            && cpp.contains(
                "uint64_t _v = (uint64_t)(harc_helper_obs_latency(t.latency, cycle_seen));"
            ),
        "hook coverpoint helper call should emit in the sampler body: {cpp}"
    );
    assert!(
        cpp.contains("std::vector<std::function<void(Txn, uint64_t)>> _harc_cov_observe_post;")
            && cpp
                .contains("sb._harc_cov_observe_post.push_back([&](Txn t, uint64_t cycle_seen) {")
            && cpp.contains("for (auto& _h : self._harc_cov_observe_post) _h(t, cycle_seen);"),
        "component hook coverage should use per-instance vectors inside the component struct: {cpp}"
    );
    assert!(
        !cpp.contains("Sb_observe_post") && !cpp.contains("sb2._harc_cov_observe_post.push_back"),
        "component hook coverage must not use type-shared vectors or register against sb2: {cpp}"
    );
    assert_eq!(
        cpp.matches("_harc_cov_observe_post.push_back").count(),
        1,
        "only the explicitly named receiver field should receive a sampler registration: {cpp}"
    );
    let sb_decl = cpp.find("Sb sb;").expect("component instance declaration");
    let hook_registration = cpp
        .find("sb._harc_cov_observe_post.push_back")
        .expect("component hook sampler registration");
    assert!(
        sb_decl < hook_registration,
        "component instance must be declared before hook sampler registration: {cpp}"
    );
}

#[test]
fn covergroup_hook_target_rejects_bare_record_arg() {
    let src = r#"
transaction Txn
    latency : uint<2>
end transaction Txn

scoreboard Sb
    hookable observe(t: Txn)
    end observe
end scoreboard Sb

covergroup BadCov @(sb.observe(t) post)
    cp_bad : cover t
        bins
            any = {0}
        end bins
end covergroup BadCov

testbench Tb
    dut : Top
    sb  : Sb
    cov : BadCov
end testbench Tb

impl BadBareRecordHookArg for Tb
    run
        wait 1 cycle
    end run
end impl BadBareRecordHookArg
"#;
    let err = lower_src(src).expect_err("bare record hook arg must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("hook target must be scalar") && msg.contains("record `Txn`"),
        "{msg}"
    );
}

#[test]
fn covergroup_hook_target_rejects_helper_record_arg_mismatch() {
    let src = r#"
transaction Txn
    latency : uint<2>
end transaction Txn

function classify(v: uint<2>) -> uint<2>
    return v
end function classify

scoreboard Sb
    hookable observe(t: Txn)
    end observe
end scoreboard Sb

covergroup BadCov @(sb.observe(t) post)
    cp_bad : cover classify(t)
        bins
            any = {0}
        end bins
end covergroup BadCov

testbench Tb
    dut : Top
    sb  : Sb
    cov : BadCov
end testbench Tb

impl BadHelperRecordArg for Tb
    run
        wait 1 cycle
    end run
end impl BadHelperRecordArg
"#;
    let err = lower_src(src).expect_err("record helper arg must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("helper `classify` argument 1 expects uint<2>, got record `Txn`"),
        "{msg}"
    );
}

#[test]
fn covergroup_point_rejects_extern_arity_mismatch() {
    let src = r#"
extern function classify(v: uint<2>) -> uint<2>

covergroup BadCov
    cp_bad : cover classify(dut.value, dut.other)
        bins
            any = {0}
        end bins
end covergroup BadCov

test BadExternArity
    let dut : Top
    let cov : BadCov
    run
        wait 1 cycle
    end run
end test BadExternArity
"#;
    let err = lower_src(src).expect_err("extern coverpoint arity mismatch must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("extern function `classify` takes 1 argument(s), call passes 2"),
        "{msg}"
    );
}

/// Locks the dump-ir text for the declared-cross + range-bin fixture
/// (schema crosses line, range bin rendering) and its emitted C++
/// (flat `_cross_*` storage, range hit tests, "cross" report blocks).
#[test]
fn cov_cross_bins_dump_ir_snapshot() {
    let prog = lower_src(&fixture("cov_cross_bins_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("cov_cross_bins_dump_ir", format!("{prog}"));
}

#[test]
fn cov_cross_bins_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "cov_cross_bins_emitted_cpp",
        emit_fixture_cpp("cov_cross_bins_test.harc")
    );
}

/// tbir emission carries the declared-cross contract markers mirrored
/// from v1's `emit_covergroup_struct` / sample path: flat storage named
/// `_cross_<item_idx>_<p1>__<p2>`, the inclusive range hit test, the
/// "cross" (not "auto_cross") report summary, the suppressed auto-cross
/// for the declared pair, and the row-major sample update.
#[test]
fn tbir_emit_declared_cross_contract() {
    let merged = merged_src(&fixture("cov_cross_bins_test.harc"));
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits");
    for marker in [
        "uint64_t _cross_2_cp_count__cp_en[6] = {};",
        "if (((_v >= 0 && _v <= 3))) { _tb.cov.cp_count.low++; _cg_hit_cp_count[0] = true; }",
        "if (((_v >= 10 && _v <= 14) || (_v == 15))) { _tb.cov.cp_count.high++; _cg_hit_cp_count[2] = true; }",
        "harc_rt::log::harc_print_covergroup_cross_summary(\"CountCov\", \"cross\", \"cp_count x cp_en\", _cross_hit, 6);",
        "harc_rt::log::harc_cov_json_cross_summary(\"CountCov\", \"cross\", \"cp_count x cp_en\", _cross_hit, 6);",
        "harc_rt::log::harc_cov_json_cross_bin(\"CountCov\", \"cross\", \"cp_count x cp_en\", \"cp_count.low x cp_en.en0\", _cross_2_cp_count__cp_en[0]);",
        "harc_rt::log::harc_print_covergroup_missing_bin(\"cp_count.low x cp_en.en0\")",
        "harc_rt::log::harc_print_covergroup_more_missing(_cross_missing, 16, \"cross\");",
        "if (_cg_hit_cp_count[_i0] && _cg_hit_cp_en[_i1]) {",
        "_tb.cov._cross_2_cp_count__cp_en[(_i0 * 2 + _i1)]++;",
    ] {
        assert!(cpp.contains(marker), "missing declared-cross marker `{marker}`");
    }
    // The declared pair suppresses its auto-cross.
    assert!(
        !cpp.contains("_auto_cross_cp_count__cp_en"),
        "declared cp_count x cp_en cross must suppress the auto-cross"
    );
}

/// `--mt` is now accepted by the tbir emitter (multi-thread actor model,
/// issue #425). For a program with no bound monitors / target actors the
/// `--mt` output is byte-identical to the cooperative default â€” `--mt`
/// only changes emission for programs that actually carry actors, so an
/// actor-free testbench keeps the single-thread scaffolding.
#[test]
fn tbir_accepts_mt() {
    let merged = merged_src(&fixture("top_counter_test.harc"));
    let prog = lower::lower_program(&merged).expect("lowers");
    let opts_mt = cpp_tb::EmitOpts {
        mt: true,
        ..Default::default()
    };
    let cpp_mt = tbir::emit(&prog, &merged, &opts_mt).expect("emits under --mt");
    let cpp_default = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits");
    assert_eq!(
        cpp_mt, cpp_default,
        "an actor-free program must emit identical C++ with and without --mt"
    );
}

// â”€â”€ placement pass snapshots â€” tier/timing annotation per block plus
//    the capability-diagnostic surface under both built-in profiles. â”€

/// top_counter under the default single-site profile: pin-driving
/// blocks anchored by WaitCycles classify cycle-exact / Tier 0; pure
/// logging blocks land in Tier 2. Diagnostics must be `none` â€” the
/// single-site profile can never diagnose (design-doc guarantee).
#[test]
fn top_counter_placement_snapshot() {
    let prog = lower_src(&fixture("top_counter_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let profile = placement::TargetProfile::single_site();
    let table = placement::run(&prog, &profile);
    assert!(table.diagnostics.is_empty(), "single-site never diagnoses");
    insta::assert_snapshot!(
        "top_counter_placement",
        format!("{}", table.display(&prog, &profile))
    );
}

/// wait_until_counter under split-strict: wait-until regions are
/// timing-tolerant over architectural ports, so even the constrained
/// profile must place them diagnostic-free.
#[test]
fn wait_until_counter_placement_split_strict_snapshot() {
    let prog = lower_src(&fixture("wait_until_counter_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let profile = placement::TargetProfile::split_strict();
    let table = placement::run(&prog, &profile);
    insta::assert_snapshot!(
        "wait_until_counter_placement_split_strict",
        format!("{}", table.display(&prog, &profile))
    );
}

/// The pass is a side-table: running it must not perturb the IR, and
/// its rendering is byte-stable across runs.
#[test]
fn placement_leaves_ir_untouched_and_is_deterministic() {
    let prog = lower_src(&fixture("top_counter_test.harc")).expect("lowers");
    let before = format!("{prog}");
    let profile = placement::TargetProfile::single_site();
    let a = format!(
        "{}",
        placement::run(&prog, &profile).display(&prog, &profile)
    );
    let b = format!(
        "{}",
        placement::run(&prog, &profile).display(&prog, &profile)
    );
    assert_eq!(a, b, "rendering must be byte-stable");
    assert_eq!(before, format!("{prog}"), "pass must not perturb the IR");
}

// â”€â”€ Bus construct: bindings, protocol-typed signal access, channel
//    handshakes, and TLM method-call edges â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Locks the dump-ir text for the Scope-A bus fixture: an inline bus
/// declaration, a `bind dut` binding on the testbench schema, and
/// two-level `<bind>.<ch>.<sig>` accesses lowering to flat-path
/// DutRead/DutWrite (`dut.axil.aw.valid` â†’ `axil_aw_valid`).
#[test]
fn axilite_bus_dump_ir_snapshot() {
    let prog = lower_src(&fixture("axilite_bus_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("axilite_bus_dump_ir", format!("{prog}"));
}

/// Locks the dump-ir text for the blocking TLM fixture: the
/// `TransactorMethod` call edges survive lowering UNINLINED â€” each
/// `mem.read`/`mem.poke` is `Assign(dest, mem.<method>(args))`, and
/// the binding's method schemas ride the testbench line.
#[test]
fn tlm_blocking_bus_dump_ir_snapshot() {
    let prog = lower_src(&fixture("tlm_method_blocking_bus_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let text = format!("{prog}");
    assert!(
        text.contains("mem.read(5)") && text.contains("mem.poke(8, 3405691582)"),
        "call edges must stay visible (never inlined) in the IR:\n{text}"
    );
    insta::assert_snapshot!("tlm_blocking_bus_dump_ir", text);
}

/// Locks the emitted C++ for the blocking TLM fixture: the call edge
/// expands to v1's req/rsp wire protocol (arg wires, valid/ready
/// budget loops, rsp_data capture, tlm_call trace events).
#[test]
fn tlm_blocking_bus_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "tlm_blocking_bus_emitted_cpp",
        emit_fixture_cpp("tlm_method_blocking_bus_test.harc")
    );
}

/// Locks the emitted C++ for a blocking RHS-fork TLM issue: unlike the OOO
/// path, the request side must still wait for `req_ready` before the trailing
/// accept-edge tick and `req_valid` deassert.
#[test]
fn tlm_blocking_fork_bus_emitted_cpp_snapshot() {
    let text = emit_fixture_cpp("tlm_method_blocking_fork_bus_test.harc");
    assert!(
        text.contains("while (!dut->mem_read_req_ready && _b > 0)")
            && text.contains("// join_all bus.read response"),
        "blocking fork must preserve the req_ready wait path:\n{text}"
    );
    insta::assert_snapshot!("tlm_blocking_fork_bus_emitted_cpp", text);
}

const SEND_RECV_SRC: &str = r#"
bus PingBus
    handshake_channel tx: send kind: valid_ready
        data: uint<32>
    end handshake_channel tx

    handshake_channel rx: receive kind: valid_ready
        data: uint<32>
        resp: uint<2>
    end handshake_channel rx
end bus PingBus

testbench PingTb
    dut : PingDut
end testbench PingTb

impl PingTest for PingTb
    let p : PingBus = bind dut

    run
        p.tx.send(7)
        let v = p.rx.recv()
        assert v == 7 else fail("got ${v}")
    end run
end impl PingTest
"#;

/// `bus.<ch>.send/recv` CFG-inline to v1's auto-handshake: drive
/// payload + valid (send) / ready (recv), 16-cycle budget loop on the
/// opposite signal, capture-before-tick (recv), trailing tick, drop.
/// The recv capture reads the FIRST payload signal (documented
/// divergence from v1's payload struct â€” equivalent for everything
/// the IR can express).
#[test]
fn bus_send_recv_dump_ir_snapshot() {
    let prog = lower_src(SEND_RECV_SRC).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let text = format!("{prog}");
    assert!(
        text.contains("DutRead(%v, dut.p.rx.data)"),
        "recv must capture the first payload signal before the tick:\n{text}"
    );
    insta::assert_snapshot!("bus_send_recv_dump_ir", text);
}

/// Struct-shaped DUT ports lower through the same multi-segment
/// `PortRef` path as bus bindings. This keeps default TB-IR usable for
/// ARCH modules with packed struct ports such as `exc_cause.irq_int`,
/// without falling back to the retired v1 backend.
#[test]
fn nested_dut_struct_port_paths_lower() {
    let src = r#"
testbench NestedDutPortTb
    dut : IfStageLike
end testbench NestedDutPortTb

impl NestedDutPortTest for NestedDutPortTb
    run
        dut.exc_cause.irq_int = 1
        dut.exc_cause.lower_cause = 11
        let got = dut.exc_cause.irq_ext
        assert got == 0 else fail("got ${got}")
    end run
end impl NestedDutPortTest
"#;
    let prog = lower_src(src).expect("lowers nested DUT port paths");
    verify::verify_program(&prog).expect("verifies");
    let text = format!("{prog}");
    assert!(
        text.contains("DutWrite(dut.exc_cause.irq_int, 1)")
            && text.contains("DutWrite(dut.exc_cause.lower_cause, 11)")
            && text.contains("DutRead(%got, dut.exc_cause.irq_ext)"),
        "nested DUT struct paths should remain visible in IR:\n{text}"
    );
}

/// Initiator-side fork/join_all TLM issue lowers to `TlmFork` request
/// statements + a `TlmJoinAll` drain (unblocks tlm_method_bus_test).
/// `out_of_order` forks get monotonic per-(field,method) tags; the
/// join_all carries every pending descriptor self-contained.
#[test]
fn bus_fork_join_lowers() {
    let prog = lower_src(&fixture("tlm_method_bus_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let text = format!("{prog}");
    // Two OOO forks, tags allocated 0 then 1, drained by one join_all.
    assert!(
        text.contains("TlmFork(%forked0 = mem.read_ooo([9]) tag=0)"),
        "first fork must carry tag 0:\n{text}"
    );
    assert!(
        text.contains("TlmFork(%forked1 = mem.read_ooo([10]) tag=1)"),
        "second fork must carry tag 1:\n{text}"
    );
    assert!(
        text.contains(
            "TlmJoinAll([%forked0 = mem.read_ooo([9]) tag=0, \
             %forked1 = mem.read_ooo([10]) tag=1])"
        ),
        "join_all must drain both pending forks:\n{text}"
    );
}

/// A `fork` with no matching `join_all` leaves its request side hanging
/// â€” rejected precisely at the end of the function rather than
/// mis-lowered.
#[test]
fn bus_fork_without_join_all_is_rejected() {
    let src = r#"
bus OooBus
    tlm_method read_ooo(addr: uint<8>) -> uint<32>: out_of_order tags 2;
end bus OooBus

testbench OooTb
    dut : TlmMemory
end testbench OooTb

impl OooTest for OooTb
    let mem : OooBus = bind dut
    run
        let x = fork mem.read_ooo(9)
    end run
end impl OooTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("has no matching `join_all`") && msg.contains("read_ooo"),
        "{msg}"
    );
}

/// Mixing a `blocking` (untagged) fork and an `out_of_order` (tagged)
/// fork before one `join_all` is rejected â€” the two routing strategies
/// (issue-order FIFO vs tag-match) cannot share a barrier.
#[test]
fn bus_fork_mixed_tagged_untagged_is_rejected() {
    let src = r#"
bus MixBus
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
    tlm_method read_ooo(addr: uint<8>) -> uint<32>: out_of_order tags 2;
end bus MixBus

testbench MixTb
    dut : TlmMemory
end testbench MixTb

impl MixTest for MixTb
    let mem : MixBus = bind dut
    run
        let a = fork mem.read(1)
        let b = fork mem.read_ooo(2)
        join_all
    end run
end impl MixTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("mix tagged") && msg.contains("untagged"),
        "{msg}"
    );
}

/// A direct (non-fork) call of an `out_of_order` method is rejected by
/// mode, naming the mode and the call site â€” and NOT by pointing at v1,
/// which rejects the same shape (see
/// `the_remaining_bus_shapes_are_not_v1_escape_hatches`).
#[test]
fn bus_ooo_direct_call_is_rejected_by_mode() {
    let src = r#"
bus OooBus
    tlm_method read_ooo(addr: uint<8>) -> uint<32>: out_of_order tags 2;
end bus OooBus

testbench OooTb
    dut : TlmMemory
end testbench OooTb

impl OooTest for OooTb
    let mem : OooBus = bind dut
    run
        let x = mem.read_ooo(9)
    end run
end impl OooTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_not_implemented(&err, lower::V1Status::Rejects);
    assert!(
        msg.contains("`out_of_order` tlm_method calls") && msg.contains("mem.read_ooo"),
        "{msg}"
    );
}

// â”€â”€ Transactor declarations + method call edges â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Shared inline fixture: an unbound DUT-poking transactor with a
/// void method and a value-returning method, instantiated `active`
/// on the testbench, bound and called from `run`.
const XACTOR_SRC: &str = r#"
transactor Xt
    dut : Top

    when active
        hookable pulse(n: uint<8>)
            dut.en = 1
            wait 1 cycle
            dut.en = 0
        end pulse

        hookable readv() -> uint<32>
            wait 1 cycle
            return dut.count_out
        end readv
    end when
end transactor Xt

testbench XtTb
    dut : Top
    xt  : Xt active
end testbench XtTb

impl XtTest for XtTb
    run
        xt.dut = dut
        xt.pulse(3)
        let v = xt.readv()
        assert v == 0 else fail("v=${v}")
    end run
end impl XtTest
"#;

/// The structural contract: one schema per transactor, one
/// `TbFunction` (kind `TransactorBody`) per method with mirrored
/// params and a `ret` slot for `-> T` methods; calls lower to
/// `Stmt::TransactorCall` (statement form `dest: None`, let form
/// `dest: Some`), with the call edge never inlined; the
/// `xt.dut = dut` bind is validated and erased.
#[test]
fn transactor_methods_lower_to_functions_and_call_edges() {
    let prog = lower_src(XACTOR_SRC).expect("lowers");
    verify::verify_program(&prog).expect("verifies");

    assert_eq!(prog.transactors.len(), 1);
    let x = &prog.transactors[0];
    assert_eq!(
        (x.name.as_str(), x.dut_field.as_str(), x.dut_type.as_str()),
        ("Xt", "dut", "Top")
    );
    assert_eq!(x.methods.len(), 2);
    let pulse = x.method("pulse").expect("pulse");
    let readv = x.method("readv").expect("readv");
    // The schema carries the declared parameter NAMES, not just a
    // count â€” that is what lets a call site check a named argument
    // against the declaration.
    assert_eq!(
        (pulse.param_names.as_slice(), pulse.has_ret),
        (["n".to_string()].as_slice(), false)
    );
    assert_eq!(
        (readv.param_names.as_slice(), readv.has_ret),
        ([].as_slice(), true)
    );

    let pf = prog.function(pulse.function);
    assert_eq!(
        pf.kind,
        ir::FunctionKind::TransactorBody {
            transactor: ir::TransactorId(0)
        }
    );
    assert_eq!(pf.params.len(), 1);
    assert_eq!(pf.locals[0].name, "n");
    assert!(pf.ret.is_none());
    // The body suspends (wait 1 cycle) and drives the DUT.
    assert!(
        pf.blocks
            .iter()
            .any(|b| matches!(b.terminator, ir::Terminator::WaitCycles(..))),
        "pulse body keeps its wait:\n{pf}"
    );
    let rf = prog.function(readv.function);
    assert!(rf.ret.is_some(), "-> T method carries a ret slot");

    // The testbench schema records the instance field.
    let tb = prog.testbench(prog.tests[0].testbench);
    assert_eq!(
        tb.transactor_fields,
        vec![("xt".to_string(), ir::TransactorId(0))]
    );

    // Run body: the bind is erased; the calls are TransactorCall stmts.
    let run = prog.function(prog.tests[0].run);
    let calls: Vec<&ir::Stmt> = run
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter(|s| matches!(s, ir::Stmt::TransactorCall { .. }))
        .collect();
    assert_eq!(calls.len(), 2, "two call edges:\n{run}");
    let ir::Stmt::TransactorCall { dest: d0, call: c0 } = calls[0] else {
        unreachable!()
    };
    assert!(d0.is_none(), "statement call discards");
    let ir::Expr::Call(ir::CallTarget::TransactorMethod { bus_field, method }, args) = c0 else {
        panic!("call edge payload: {c0:?}");
    };
    assert_eq!(
        (bus_field.as_str(), method.as_str(), args.len()),
        ("xt", "pulse", 1)
    );
    let ir::Stmt::TransactorCall { dest: d1, .. } = calls[1] else {
        unreachable!()
    };
    assert!(d1.is_some(), "let call binds the result");
}

#[test]
fn transactor_method_bare_sibling_calls_lower_to_self_calls() {
    let src = r#"
transactor Xt
    dut : Top

    when active
        hookable idle()
            dut.en = 0
            wait 1 cycle
        end idle

        hookable readv() -> uint<32>
            let v = dut.value
            return v
        end readv

        hookable pulse()
            dut.en = 1
            wait 1 cycle
            idle()
        end pulse

        hookable read_twice() -> uint<32>
            let a = readv()
            let b = readv()
            return a + b
        end read_twice
    end when
end transactor Xt

testbench XtTb
    dut : Top
    xt  : Xt active
end testbench XtTb

impl XtNestedCallTest for XtTb
    run
        xt.dut = dut
        xt.pulse()
        let v = xt.read_twice()
        assert v == 0 else fail("v=${v}")
    end run
end impl XtNestedCallTest
"#;
    let prog = lower_src(src).expect("nested transactor sibling calls lower");
    verify::verify_program(&prog).expect("verifies");

    let x = &prog.transactors[0];
    let pulse = prog.function(x.method("pulse").expect("pulse").function);
    assert!(
        pulse.blocks.iter().flat_map(|b| &b.stmts).any(|s| {
            matches!(
                s,
                ir::Stmt::TransactorSelfCall {
                    dest: None,
                    call: ir::Expr::Call(
                        ir::CallTarget::TransactorSelfMethod { method, .. },
                        _
                    )
                } if method == "idle"
            )
        }),
        "pulse should dispatch idle() through a TransactorSelfCall:\n{pulse}"
    );

    let read_twice = prog.function(x.method("read_twice").expect("read_twice").function);
    let readv_calls = read_twice
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter(|s| {
            matches!(
                s,
                ir::Stmt::TransactorSelfCall {
                    dest: Some(_),
                    call: ir::Expr::Call(
                        ir::CallTarget::TransactorSelfMethod { method, .. },
                        _
                    )
                } if method == "readv"
            )
        })
        .count();
    assert_eq!(
        readv_calls, 2,
        "read_twice should hoist value-returning readv() sibling calls:\n{read_twice}"
    );
}

#[test]
fn transactor_always_on_self_call_to_active_only_sibling_is_rejected() {
    let src = r#"
transactor Xt
    dut : Top

    hookable outer()
        active_only()
    end outer

    when active
        hookable active_only()
            dut.en = 1
        end active_only
    end when
end transactor Xt

testbench XtTb
    dut : Top
    xt  : Xt active
end testbench XtTb

impl XtPassiveBackdoorTest for XtTb
    run
        xt.dut = dut
        xt.outer()
    end run
end impl XtPassiveBackdoorTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("active_only") && msg.contains("when active") && msg.contains("outer"),
        "{msg}"
    );
}

#[test]
fn transactor_self_call_in_wait_until_predicate_is_rejected() {
    let src = r#"
transactor Xt
    dut : Top

    when active
        hookable ready() -> uint<1>
            return 1
        end ready

        hookable wait_ready()
            wait until ready()
        end wait_ready
    end when
end transactor Xt

testbench XtTb
    dut : Top
    xt  : Xt active
end testbench XtTb

impl XtWaitUntilSelfCallTest for XtTb
    run
        xt.dut = dut
        xt.wait_ready()
    end run
end impl XtWaitUntilSelfCallTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("transactor method call inside a `wait until` predicate"),
        "{msg}"
    );
}

/// A transactor method that calls itself lowers to a synchronous
/// self-recursive `std::function` lambda, which overflows the C++ stack
/// at runtime. Lowering must reject the cycle with a clear diagnostic
/// rather than emit a crashing binary.
#[test]
fn transactor_method_direct_self_recursion_is_rejected() {
    let src = r#"
transactor Xt
    dut : Top

    when active
        hookable idle()
            dut.en = 0
            wait 1 cycle
            idle()
        end idle
    end when
end transactor Xt

testbench XtTb
    dut : Top
    xt  : Xt active
end testbench XtTb

impl XtSelfRecursionTest for XtTb
    run
        xt.dut = dut
        xt.idle()
    end run
end impl XtSelfRecursionTest
"#;
    let err = lower_src(src).unwrap_err();
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "expected LowerError::Invalid for self-recursion: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("recursive method-call cycle") && msg.contains("idle -> idle"),
        "diagnostic should name the cycle: {msg}"
    );
}

/// Mutual recursion between two transactor methods (`a -> b -> a`) is
/// the same hazard as direct self-recursion and must also be rejected,
/// with the full cycle rendered in the diagnostic.
#[test]
fn transactor_method_mutual_recursion_is_rejected() {
    let src = r#"
transactor Xt
    dut : Top

    when active
        hookable ping()
            dut.en = 1
            wait 1 cycle
            pong()
        end ping

        hookable pong()
            dut.en = 0
            wait 1 cycle
            ping()
        end pong
    end when
end transactor Xt

testbench XtTb
    dut : Top
    xt  : Xt active
end testbench XtTb

impl XtMutualRecursionTest for XtTb
    run
        xt.dut = dut
        xt.ping()
    end run
end impl XtMutualRecursionTest
"#;
    let err = lower_src(src).unwrap_err();
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "expected LowerError::Invalid for mutual recursion: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("recursive method-call cycle") && msg.contains("ping") && msg.contains("pong"),
        "diagnostic should render the ping/pong cycle: {msg}"
    );
}

/// A non-recursive sibling-call chain (`a -> b -> c`, no back-edge) must
/// still lower cleanly â€” the cycle guard must not over-reject legitimate
/// transactor-method composition.
#[test]
fn transactor_method_acyclic_sibling_chain_is_accepted() {
    let src = r#"
transactor Xt
    dut : Top

    when active
        hookable low()
            dut.en = 0
            wait 1 cycle
        end low

        hookable mid()
            dut.en = 1
            wait 1 cycle
            low()
        end mid

        hookable high()
            dut.en = 1
            mid()
        end high
    end when
end transactor Xt

testbench XtTb
    dut : Top
    xt  : Xt active
end testbench XtTb

impl XtAcyclicChainTest for XtTb
    run
        xt.dut = dut
        xt.high()
    end run
end impl XtAcyclicChainTest
"#;
    let prog = lower_src(src).expect("acyclic sibling chain lowers");
    verify::verify_program(&prog).expect("verifies");
}

/// Env-held DUT-poking BFMs route through the component path rather than
/// `TbProgram::transactors`, but their bare sibling calls still lower to
/// synchronous `TransactorSelfCall`s. The recursion guard must therefore
/// cover componentized transactors too.
#[test]
fn env_held_transactor_method_mutual_recursion_is_rejected() {
    let src = r#"
transactor Xt
    dut : Top

    when active
        hookable ping()
            dut.en = 1
            wait 1 cycle
            pong()
        end ping

        hookable pong()
            dut.en = 0
            wait 1 cycle
            ping()
        end pong
    end when
end transactor Xt

env E
    drv : Xt active
end env E

testbench Tb
    dut : Top
    env : E
end testbench Tb

impl Repro for Tb
    run
        env.drv.dut = dut
        env.drv.ping()
    end run
end impl Repro
"#;
    let err = lower_src(src).unwrap_err();
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "expected LowerError::Invalid for env-held mutual recursion: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("recursive method-call cycle") && msg.contains("ping") && msg.contains("pong"),
        "diagnostic should render the env-held ping/pong cycle: {msg}"
    );
}

#[test]
fn testbench_helper_wrapper_can_call_active_transactor_method() {
    let src = r#"
transactor Xt
    dut : Top

    when active
        hookable pulse(n: uint<32>)
            for _ in 1 .. n
                dut.en = 1
                wait 1 cycle
                dut.en = 0
                wait 1 cycle
            end for
        end pulse
    end when
end transactor Xt

testbench XtTb
    dut : Top
    xt  : Xt active

    function apply_pulse(n: uint<32>)
        xt.pulse(n)
    end apply_pulse
end testbench XtTb

impl XtHelperWrapperTest for XtTb
    run
        xt.dut = dut
        apply_pulse(2)
    end run
end impl XtHelperWrapperTest
"#;
    let prog = lower_src(src).expect("testbench wrapper lowers");
    verify::verify_program(&prog).expect("verifies");

    let run = prog.function(prog.tests[0].run);
    assert!(
        run.blocks.iter().flat_map(|b| &b.stmts).any(|s| {
            matches!(
                s,
                ir::Stmt::TransactorCall {
                    dest: None,
                    call: ir::Expr::Call(
                        ir::CallTarget::TransactorMethod { bus_field, method },
                        _
                    )
                } if bus_field == "xt" && method == "pulse"
            )
        }),
        "inlined testbench helper should keep xt.pulse() as a TransactorCall:\n{run}"
    );
}

/// Emitted-C++ shape for nested transactor sibling calls. The committed
/// `cam_value_basic` snapshot only locks the `auto`â†’`std::function`
/// predeclaration in isolation (no actual sibling call); this asserts
/// the generated code for (a) a FORWARD sibling call (`first()` calls
/// `second()`, declared later) and (b) a VALUE-returning sibling call
/// (`use_readv()` calls `readv() -> uint<32>`). The forward case only
/// compiles because every method is predeclared as a `std::function`
/// slot BEFORE any lambda is assigned â€” assert that ordering directly.
#[test]
fn transactor_sibling_calls_emit_synchronous_predeclared_invocations() {
    let src = r#"
transactor Xt
    dut : Top

    when active
        hookable first()
            dut.en = 1
            second()
        end first

        hookable second()
            dut.en = 0
            wait 1 cycle
        end second

        hookable readv() -> uint<32>
            let v = dut.value
            return v
        end readv

        hookable use_readv() -> uint<32>
            let a = readv()
            return a
        end use_readv
    end when
end transactor Xt

testbench XtTb
    dut : Top
    xt  : Xt active
end testbench XtTb

impl XtSiblingEmitTest for XtTb
    run
        xt.dut = dut
        xt.first()
        let r = xt.use_readv()
        assert r == 0 else fail("r=${r}")
    end run
end impl XtSiblingEmitTest
"#;
    let cpp = emit_cpp_src(src);

    // Each method is predeclared as a typed std::function slot.
    let void_slot = "std::function<void()> Xt_second";
    let ret_slot = "std::function<uint64_t()> Xt_readv";
    assert!(
        cpp.contains(void_slot),
        "expected predeclared void slot `{void_slot}`:\n{cpp}"
    );
    assert!(
        cpp.contains(ret_slot),
        "expected predeclared uint64_t slot `{ret_slot}`:\n{cpp}"
    );

    // Forward-reference soundness: the callee's slot is declared before
    // the *caller's* lambda is assigned (otherwise `Xt_second();` inside
    // `first` would reference an as-yet-undeclared name).
    let second_decl = cpp.find(void_slot).expect("second slot present");
    let first_assign = cpp.find("Xt_first = [&]").expect("first lambda assigned");
    assert!(
        second_decl < first_assign,
        "callee slot must be declared before the caller lambda is assigned"
    );

    // Sibling calls emit as direct synchronous invocations.
    assert!(
        cpp.contains("Xt_second();"),
        "forward sibling call should emit `Xt_second();`:\n{cpp}"
    );
    assert!(
        cpp.contains("Xt_readv("),
        "value-returning sibling call should invoke `Xt_readv(...)`:\n{cpp}"
    );
}

#[test]
fn method_wall_clock_wait_emits_inline_settle() {
    let src = r#"
domain D
  freq_mhz: 100
end domain D

transactor Driver
    dut : Top
    when active
        hookable settle()
            dut.en = dut.en
            wait 1ps
        end settle
    end when
end transactor Driver

sequencer ComponentDriver
    hookable settle()
        wait 1ps
    end settle
end sequencer ComponentDriver

testbench Tb
    dut : Top
    drv : Driver active
    comp : ComponentDriver

    function settle_tb()
        wait 1ps
    end function settle_tb
end testbench Tb

impl MethodWaitTimeTest for Tb
    on drv.settle pre
        wait 1ps
    end on

    clock clk = D
    run
        drv.dut = dut
        drv.settle()
        comp.settle()
        settle_tb()
    end run
end impl MethodWaitTimeTest
"#;
    let cpp = emit_cpp_src(src);
    assert!(
        cpp.matches("eval_clocks_until(now_ps + 1);").count() >= 4,
        "transactor, component, testbench, and hook methods should emit wall-clock settle waits:\n{cpp}"
    );
}

#[test]
fn clockless_test_reaching_method_wall_clock_wait_emits_time_runtime() {
    let src = r#"
sequencer ComponentDriver
    hookable settle()
        wait 1ps
    end settle
end sequencer ComponentDriver

test ClocklessMethodWaitTest
    let dut : Top
    let comp : ComponentDriver
    run
        log(info, "component method is emitted even when unused")
    end run
end test ClocklessMethodWaitTest
"#;
    let cpp = emit_cpp_src(src);
    assert!(
        cpp.contains("long long now_ps = 0;"),
        "clockless tests with wall waits should emit absolute time state:\n{cpp}"
    );
    assert!(
        cpp.contains("auto eval_clocks_until = [&](long long t_ps)"),
        "clockless tests with wall waits should emit a time-advance helper:\n{cpp}"
    );
    assert!(
        cpp.contains("eval_clocks_until(now_ps + 1);"),
        "method-local wall wait should advance clockless absolute time:\n{cpp}"
    );
}

#[test]
fn clockless_test_emitting_transactor_method_wall_clock_wait_emits_time_runtime() {
    let src = r#"
transactor Driver
    dut : Top
    when active
        hookable settle()
            wait 1ps
        end settle
    end when
end transactor Driver

testbench Tb
    dut : Top
    drv : Driver active
end testbench Tb

impl ClocklessTransactorMethodEmitTest for Tb
    run
        log(info, "transactor method is emitted even when unused")
    end run
end impl ClocklessTransactorMethodEmitTest
"#;
    let cpp = emit_cpp_src(src);
    assert!(
        cpp.contains("long long now_ps = 0;"),
        "clockless transactor method waits should emit absolute time state:\n{cpp}"
    );
    assert!(
        cpp.contains("auto eval_clocks_until = [&](long long t_ps)"),
        "clockless transactor method waits should emit a time-advance helper:\n{cpp}"
    );
    assert!(
        cpp.contains("eval_clocks_until(now_ps + 1);"),
        "transactor method wall wait should advance clockless absolute time:\n{cpp}"
    );
}

#[test]
fn clockless_test_emitting_test_hook_wall_clock_wait_emits_time_runtime() {
    let src = r#"
transactor Driver
    dut : Top
    when active
        hookable settle()
            dut.en = dut.en
        end settle
    end when
end transactor Driver

testbench Tb
    dut : Top
    drv : Driver active
end testbench Tb

impl ClocklessTestHookEmitTest for Tb
    on drv.settle pre
        wait 1ps
    end on

    run
        log(info, "hook is emitted even when method is unused")
    end run
end impl ClocklessTestHookEmitTest
"#;
    let cpp = emit_cpp_src(src);
    assert!(
        cpp.contains("long long now_ps = 0;"),
        "clockless hook waits should emit absolute time state:\n{cpp}"
    );
    assert!(
        cpp.contains("auto eval_clocks_until = [&](long long t_ps)"),
        "clockless hook waits should emit a time-advance helper:\n{cpp}"
    );
    assert!(
        cpp.contains("eval_clocks_until(now_ps + 1);"),
        "test hook wall wait should advance clockless absolute time:\n{cpp}"
    );
}

/// A sibling call with the wrong argument count is rejected at lowering
/// (`check_transactor_self_call` mirrors the same check at verify time,
/// but lowering fires first).
#[test]
fn transactor_sibling_call_arity_mismatch_is_rejected() {
    let src = r#"
transactor Xt
    dut : Top

    when active
        hookable inner(n: uint<8>)
            dut.x = n
            wait 1 cycle
        end inner

        hookable outer()
            inner()
        end outer
    end when
end transactor Xt

testbench XtTb
    dut : Top
    xt  : Xt active
end testbench XtTb

impl XtArityTest for XtTb
    run
        xt.dut = dut
        xt.outer()
    end run
end impl XtArityTest
"#;
    let err = lower_src(src).unwrap_err();
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "expected LowerError::Invalid for arity mismatch: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("takes 1 argument(s), call passes 0"),
        "diagnostic should name the arity mismatch: {msg}"
    );
}

/// Capturing the result of a void sibling method into a `let` is
/// rejected at lowering (the value-position lowering passes
/// `need_ret = true`; a void method has no value to bind).
#[test]
fn transactor_sibling_call_void_method_in_value_position_is_rejected() {
    let src = r#"
transactor Xt
    dut : Top

    when active
        hookable act()
            dut.x = 1
            wait 1 cycle
        end act

        hookable bad() -> uint<32>
            let v = act()
            return v
        end bad
    end when
end transactor Xt

testbench XtTb
    dut : Top
    xt  : Xt active
end testbench XtTb

impl XtVoidValueTest for XtTb
    run
        xt.dut = dut
        let r = xt.bad()
        assert r == 0 else fail("r=${r}")
    end run
end impl XtVoidValueTest
"#;
    let err = lower_src(src).unwrap_err();
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "expected LowerError::Invalid for void-in-value: {err:?}"
    );
    assert!(
        err.to_string().contains("returns no value"),
        "diagnostic should say the method returns no value: {err}"
    );
}

/// A named argument in a transactor sibling call is judged by WHERE it
/// is written, not by the fact that it is named.
///
/// This test used to assert that any named argument was rejected, on a
/// call â€” `inner(n = 5)` â€” whose name sits in its own position. That is
/// the inert form: v1 drops the name and binds by position, so it emits
/// exactly what the positional call emits. The old assertion pinned a
/// refusal of a working program.
///
/// The seam could not tell the cases apart because
/// `self_transactor_methods` carried a parameter COUNT. It carries the
/// declared names now, the same fix as
/// `TransactorMethodSchema::param_names`.
#[test]
fn a_transactor_sibling_call_judges_a_named_argument_by_its_position() {
    let src = |call: &str| {
        format!(
            r#"
transactor Xt
    dut : Top

    when active
        hookable inner(n: uint<8>, m: uint<8>)
            dut.x = n + m
            wait 1 cycle
        end inner

        hookable outer()
            {call}
        end outer
    end when
end transactor Xt

testbench XtTb
    dut : Top
    xt  : Xt active
end testbench XtTb

impl XtNamedArgTest for XtTb
    run
        xt.dut = dut
        xt.outer()
    end run
end impl XtNamedArgTest
"#
        )
    };

    // v1's behaviour is what the classification rests on: the in-order
    // form is byte-identical to positional, the reordered one is not.
    let emitted = |call: &str| -> String {
        cpp_tb::emit(&merged_src(&src(call)))
            .unwrap_or_else(|e| panic!("v1 emits `{call}`: {e}"))
            .lines()
            .filter(|l| l.contains("Xt_inner("))
            .map(|l| l.trim().to_string())
            .collect::<Vec<_>>()
            .join(" ;; ")
    };
    let positional = emitted("inner(5, 6)");
    assert_eq!(
        emitted("inner(n = 5, m = 6)"),
        positional,
        "in-order is inert"
    );
    assert_ne!(
        emitted("inner(m = 6, n = 5)"),
        positional,
        "reordered swaps"
    );

    // TB-IR: positional and in-order lower.
    lower_src(&src("inner(5, 6)")).expect("positional lowers");
    lower_src(&src("inner(n = 5, m = 6)")).expect("an in-order name lowers");

    // A reordered name is the silent mis-lowering.
    let msg = assert_not_implemented(
        &lower_src(&src("inner(m = 6, n = 5)")).unwrap_err(),
        lower::V1Status::SilentlyMisLowers,
    );
    assert!(
        msg.contains("transactor sibling method call")
            && msg.contains("`m` is parameter 2 here but was written in position 1"),
        "{msg}"
    );

    // A name matching no parameter is a program error under both.
    let msg = assert_invalid(&lower_src(&src("inner(nosuch = 5, 6)")).unwrap_err());
    assert!(msg.contains("`nosuch` names no parameter of"), "{msg}");
}

/// Locks the dump-ir text for the smallest corpus transactor fixture:
/// the transactor table, `TransactorBody` functions with mirrored
/// params, erased DUT bind, and TransactorCall statements.
#[test]
fn cam_value_basic_dump_ir_snapshot() {
    let prog = lower_src(&fixture("cam_value_basic_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("cam_value_basic_dump_ir", format!("{prog}"));
}

/// Locks the emitted tbir C++ for the same fixture: `<Type>_<method>`
/// lambdas with synchronous waits (`for (...) tick();` â€” v1's hookable
/// contract, no co_await), plain `return`, and direct call sites in
/// the run coroutine.
#[test]
fn cam_value_basic_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "cam_value_basic_emitted_cpp",
        emit_fixture_cpp("cam_value_basic_test.harc")
    );
}

/// A value-returning transactor method call in expression position
/// (`let v = xt.readv() + 1`) is hoisted into its own
/// `Stmt::TransactorCall { dest: Some(temp), .. }` and the result temp
/// substituted in the expression â€” the seam rule's sanctioned home (the
/// call edge never stays nested) and the call's internal `tick()` runs
/// at the hoist point, in source order. v1 emits the equivalent inline.
#[test]
fn transactor_call_in_expression_position_hoisted() {
    let src = XACTOR_SRC.replace("let v = xt.readv()", "let v = xt.readv() + 1");
    let prog = lower_src(&src).expect("expression-position call lowers via hoist");
    verify::verify_program(&prog).expect("verifies");

    let run = prog.function(prog.tests[0].run);
    // Two call edges: the `xt.pulse(3)` statement call (discards) and
    // the hoisted `xt.readv()` (binds a fresh temp), both as
    // TransactorCall statements.
    let binds: Vec<&ir::Stmt> = run
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter(|s| {
            matches!(
                s,
                ir::Stmt::TransactorCall { dest: Some(_), call: ir::Expr::Call(
                    ir::CallTarget::TransactorMethod { method, .. }, _
                ) } if method == "readv"
            )
        })
        .collect();
    assert_eq!(
        binds.len(),
        1,
        "readv() hoisted into a result-binding TransactorCall:\n{run}"
    );
    // No bare TransactorMethod call edge survives nested in an Assign.
    for s in run.blocks.iter().flat_map(|b| &b.stmts) {
        if let ir::Stmt::Assign(_, e) = s {
            assert!(
                !matches!(
                    e,
                    ir::Expr::Call(ir::CallTarget::TransactorMethod { .. }, _)
                ),
                "no bare transactor call edge as an Assign RHS"
            );
        }
    }
}

/// A method param typed as a declared `transaction`/`struct` record
/// lowers to `IrType::Record` and is passed by value: the method body
/// binds the record param and reads its fields. The call site passes the
/// caller's record local as a `Stmt::TransactorCall` argument.
#[test]
fn transactor_record_typed_method_param_lowers() {
    let src = r#"
transaction RunCmd
    ticks  : uint<8>  default 1
    expect : uint<32> default 0
end transaction RunCmd

transactor Drv
    dut : Top

    when active
        hookable run_for(cmd: RunCmd)
            dut.en = 1
            for _ in 1 .. cmd.ticks
                wait 1 cycle
            end for
            dut.en = 0
        end run_for
    end when
end transactor Drv

testbench DrvTb
    dut : Top
    drv : Drv active
end testbench DrvTb

impl DrvTest for DrvTb
    run
        drv.dut = dut
        let cmd : RunCmd
        cmd.ticks = 3
        drv.run_for(cmd)
    end run
end impl DrvTest
"#;
    let prog = lower_src(src).expect("record-typed method param lowers");
    verify::verify_program(&prog).expect("verifies");

    // The record exists; the method's single param is record-typed.
    assert_eq!(prog.records.len(), 1, "one transaction record");
    let rid = ir::RecordId(0);
    let x = &prog.transactors[0];
    let m = x.method("run_for").expect("run_for");
    assert_eq!(m.param_names, vec!["cmd".to_string()]);
    let mf = prog.function(m.function);
    assert_eq!(mf.params.len(), 1);
    assert_eq!(
        mf.params[0].ty,
        ir::IrType::Record(rid),
        "param is by-value record"
    );
    // The first local mirrors the record param (TB-IR convention) and is
    // record-typed; the body reads `cmd.ticks` (visible in the IR text).
    assert_eq!(
        mf.locals[0].ty,
        ir::IrType::Record(rid),
        "param local is the record"
    );
    assert!(
        format!("{mf}").contains("%cmd.ticks"),
        "run_for body reads cmd.ticks:\n{mf}"
    );

    // The call passes the caller's record local as an argument.
    let run = prog.function(prog.tests[0].run);
    let arg_is_local = run.blocks.iter().flat_map(|b| &b.stmts).any(|s| {
        matches!(s, ir::Stmt::TransactorCall {
            call: ir::Expr::Call(ir::CallTarget::TransactorMethod { method, .. }, args), ..
        } if method == "run_for" && matches!(args.as_slice(), [ir::Expr::Local(_)]))
    });
    assert!(
        arg_is_local,
        "run_for call passes the record local by value:\n{run}"
    );
}

/// Issue #494: a file-scope helper parameter typed as a declared
/// transaction/struct is a by-value record, not a DUT/module handle. The
/// helper is CFG-inlined because record field access is outside the pure
/// scalar-helper subset, but the parameter local must still be
/// `IrType::Record` so `cmd.value` lowers through the existing
/// `RecordField` path instead of the old "module type with a non-DUT
/// argument" rejection.
#[test]
fn file_helper_record_typed_param_lowers() {
    let src = r#"
transaction Cmd
    value : uint<8> default 0
end transaction Cmd

function get_value(cmd: Cmd) -> uint<8>
    return cmd.value
end function get_value

testbench HelperRecordParamTb
    dut : Top
end testbench HelperRecordParamTb

impl HelperRecordParamTest for HelperRecordParamTb
    run
        let cmd : Cmd
        cmd.value = 7
        let got = get_value(cmd)
        assert got == 7
            else fail("got ${got}, expected 7")
    end run
end impl HelperRecordParamTest
"#;
    let prog = lower_src(src).expect("record-typed file helper param lowers");
    verify::verify_program(&prog).expect("verifies");

    let run = prog.function(prog.tests[0].run);
    assert!(
        run.locals
            .iter()
            .any(|l| l.name.starts_with("cmd_") && matches!(l.ty, ir::IrType::Record(_))),
        "inlined helper declares a record-typed cmd parameter local:\n{run}"
    );
    assert!(
        format!("{run}").contains("%cmd_2.value"),
        "inlined helper reads the record field:\n{run}"
    );
}

/// Issue #494: the same record-vs-module classification must apply to
/// testbench helper methods. A `function observe(cmd: Cmd)` declared on a
/// testbench receives `cmd` by value and can read `cmd.value` when inlined
/// into a bound `impl` body.
#[test]
fn testbench_method_record_typed_param_lowers() {
    let src = r#"
transaction Cmd
    value : uint<8> default 0
end transaction Cmd

testbench TbRecordMethod
    dut : Top

    function observe(cmd: Cmd) -> uint<8>
        return cmd.value
    end function observe
end testbench TbRecordMethod

impl TbRecordMethodTest for TbRecordMethod
    run
        let cmd : Cmd
        cmd.value = 9
        let got = observe(cmd)
        assert got == 9
            else fail("got ${got}, expected 9")
    end run
end impl TbRecordMethodTest
"#;
    let prog = lower_src(src).expect("record-typed testbench method param lowers");
    verify::verify_program(&prog).expect("verifies");

    let run = prog.function(prog.tests[0].run);
    assert!(
        run.locals
            .iter()
            .any(|l| l.name.starts_with("cmd_") && matches!(l.ty, ir::IrType::Record(_))),
        "inlined testbench method declares a record-typed cmd parameter local:\n{run}"
    );
    assert!(
        format!("{run}").contains("%cmd_2.value"),
        "inlined testbench method reads the record field:\n{run}"
    );
}

/// Issue #494: scoreboard/component methods use their own parameter
/// classifier. A method such as `sink.observe(cmd: Cmd)` must bind `cmd`
/// as a by-value record so the body can read `cmd.value`.
#[test]
fn scoreboard_method_record_typed_param_lowers() {
    let src = r#"
transaction Cmd
    value : uint<8> default 0
end transaction Cmd

scoreboard Sink
    seen : uint<8> default 0

    function observe(cmd: Cmd)
        seen = cmd.value
    end function observe
end scoreboard Sink

testbench ScoreboardRecordParamTb
    dut : Top
    sink : Sink
end testbench ScoreboardRecordParamTb

impl ScoreboardRecordParamTest for ScoreboardRecordParamTb
    run
        let cmd : Cmd
        cmd.value = 11
        sink.observe(cmd)
        assert sink.seen == 11
            else fail("seen ${sink.seen}, expected 11")
    end run
end impl ScoreboardRecordParamTest
"#;
    let prog = lower_src(src).expect("record-typed scoreboard method param lowers");
    verify::verify_program(&prog).expect("verifies");

    let method = prog
        .components
        .iter()
        .find(|c| c.name == "Sink")
        .and_then(|c| c.method("observe"))
        .expect("Sink.observe");
    let func = prog.function(method.function);
    assert_eq!(
        func.params.first().map(|p| &p.ty),
        Some(&ir::IrType::Record(ir::RecordId(0))),
        "scoreboard method param is record-typed:\n{func}"
    );
    assert!(
        format!("{func}").contains("%cmd.value"),
        "scoreboard method reads the record field:\n{func}"
    );
}

/// Issue #494 follow-up: inlined file helpers can return a record by value.
/// The return temp must be `RecordInit`-initialized, not assigned scalar `0`.
#[test]
fn file_helper_record_typed_param_record_return_lowers() {
    let src = r#"
transaction Cmd
    value : uint<8> default 0
end transaction Cmd

function id_cmd(cmd: Cmd) -> Cmd
    return cmd
end function id_cmd

testbench HelperRecordReturnTb
    dut : Top
end testbench HelperRecordReturnTb

impl HelperRecordReturnTest for HelperRecordReturnTb
    run
        let cmd : Cmd
        cmd.value = 13
        let got : Cmd = id_cmd(cmd)
        assert got.value == 13
            else fail("got ${got.value}, expected 13")
    end run
end impl HelperRecordReturnTest
"#;
    let prog = lower_src(src).expect("record-returning file helper lowers");
    verify::verify_program(&prog).expect("verifies");

    let run = prog.function(prog.tests[0].run);
    assert_no_record_zero_assign(run);
    assert!(
        run.blocks
            .iter()
            .flat_map(|b| &b.stmts)
            .any(|s| matches!(s, ir::Stmt::RecordInit(_, ir::RecordId(0)))),
        "record-return temp is default-initialized with RecordInit:\n{run}"
    );
    let cpp = emit_cpp_src(src);
    assert!(
        cpp.contains("__t0 = Cmd{};") && !cpp.contains("__t0 = 0;"),
        "record-return helper must default-initialize the record temp, not scalar-zero it:\n{cpp}"
    );
}

/// Issue #494 follow-up: inlined testbench methods can also return a
/// record by value without scalar-zero initializing the record temp.
#[test]
fn testbench_method_record_typed_param_record_return_lowers() {
    let src = r#"
transaction Cmd
    value : uint<8> default 0
end transaction Cmd

testbench TbRecordReturn
    dut : Top

    function id_cmd(cmd: Cmd) -> Cmd
        return cmd
    end function id_cmd
end testbench TbRecordReturn

impl TbRecordReturnTest for TbRecordReturn
    run
        let cmd : Cmd
        cmd.value = 17
        let got : Cmd = id_cmd(cmd)
        assert got.value == 17
            else fail("got ${got.value}, expected 17")
    end run
end impl TbRecordReturnTest
"#;
    let prog = lower_src(src).expect("record-returning testbench method lowers");
    verify::verify_program(&prog).expect("verifies");

    let run = prog.function(prog.tests[0].run);
    assert_no_record_zero_assign(run);
    assert!(
        run.blocks
            .iter()
            .flat_map(|b| &b.stmts)
            .any(|s| matches!(s, ir::Stmt::RecordInit(_, ir::RecordId(0)))),
        "record-return temp is default-initialized with RecordInit:\n{run}"
    );
    let cpp = emit_cpp_src(src);
    assert!(
        cpp.contains("__t0 = Cmd{};") && !cpp.contains("__t0 = 0;"),
        "record-return testbench method must default-initialize the record temp, not scalar-zero it:\n{cpp}"
    );
}

fn assert_no_record_zero_assign(func: &ir::TbFunction) {
    for block in &func.blocks {
        for stmt in &block.stmts {
            if let ir::Stmt::Assign(
                local,
                ir::Expr::Literal {
                    value: 0,
                    ty: ir::IrType::Unknown,
                },
            ) = stmt
            {
                assert!(
                    !matches!(func.locals[local.index()].ty, ir::IrType::Record(_)),
                    "record-typed local `{}` must not be scalar-zero initialized:\n{func}",
                    func.locals[local.index()].name
                );
            }
        }
    }
}

/// ...and not inside lazily-evaluated log/fail messages either.
#[test]
fn transactor_call_in_message_rejected() {
    let src = XACTOR_SRC.replace("fail(\"v=${v}\")", "fail(\"v=${xt.readv()}\")");
    let err = lower_src(&src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(msg.contains("inside a message"), "{msg}");
}

/// Mode rules at the instance field: a `passive` instance is accepted
/// (its passive surface â€” persistent state + always-on handlers â€” is
/// lowered; #494 P0a/P1b), but calling one of its `when active` methods
/// on the passive instance is rejected at the call site as `Invalid`
/// (the method structurally does not exist there).
///
/// A mode-less field has nothing to inherit from at testbench scope, so
/// it stays rejected â€” and as `Invalid`, not with a v1 suggestion.
/// Measured: v1 refuses it too, with "transactor field `_tb.p : Poker`
/// has no mode and ...". The comment at the arm has always said the
/// mode rules "mirror v1"; this one does, so pointing at v1 was never
/// going to help.
#[test]
fn transactor_instance_mode_rules() {
    // `XACTOR_SRC` calls `xt.pulse(...)` / `xt.readv()` â€” both `when
    // active` methods â€” so a passive `xt` is rejected at the CALL site.
    let passive = XACTOR_SRC.replace("xt  : Xt active", "xt  : Xt passive");
    let err = lower_src(&passive).unwrap_err();
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "calling a `when active` method on a passive instance must be Invalid, \
         not Unsupported: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("when active") && msg.contains("passive instance"),
        "{msg}"
    );

    let modeless = XACTOR_SRC.replace("xt  : Xt active", "xt  : Xt");
    let msg = assert_invalid(&lower_src(&modeless).unwrap_err());
    assert!(
        msg.contains("needs an `active`/`passive` mode annotation"),
        "{msg}"
    );
    // The half that makes `Invalid` honest rather than merely tidier.
    let v1 = cpp_tb::emit(&merged_src(&modeless))
        .expect_err("v1 refuses a mode-less transactor field too");
    assert!(format!("{v1}").contains("has no mode"), "{v1}");
}

/// A `passive` instance whose `when active` methods are never CALLED is
/// accepted, and its persistent state is lowered per-instance. This is
/// the P0a/P1b gap: v1 accepts it, TB-IR previously rejected the binding
/// outright.
#[test]
fn passive_transactor_instance_accepted_when_methods_uncalled() {
    // Drop the `when active` method calls from the run body; keep the
    // passive binding. The state-only passive surface must lower.
    let src = r#"
transactor Xp
    dut : Top
    tag : uint<32> default 5
    when active
        hookable poke()
            dut.en = 1
        end poke
    end when
end transactor Xp

testbench XpTb
    dut : Top
    a : Xp passive
    b : Xp passive
end testbench XpTb

impl XpTest for XpTb
    run
        dut.en = 1
        wait 2 cycle
    end run
    check
        assert a.tag == 5 else fail("a=${a.tag}")
        assert b.tag == 5 else fail("b=${b.tag}")
    end check
end impl XpTest
"#;
    let prog = lower_src(src).expect("passive multi-instance lowers");
    let tb = &prog.testbenches[0];
    // Two passive instances, both recorded as passive.
    assert_eq!(tb.transactor_fields.len(), 2, "two passive instances");
    assert!(tb.passive_transactor_fields.contains("a"));
    assert!(tb.passive_transactor_fields.contains("b"));
    // Independent per-instance state structs (no shared-body clobber).
    assert_eq!(
        tb.unbound_state_actors.len(),
        2,
        "each passive instance gets its own state struct"
    );
}

#[test]
fn passive_dut_monitor_helper_field_lowers() {
    let src = r#"
transaction SampleTxn
    check : bool default true
end transaction SampleTxn

transactor Mon
    dut : Top
    samples : uint<32> default 0
    last : uint<32> default 0

    hookable observe()
        samples = samples + 1
        last = dut.count
    end observe
end transactor Mon

testbench MonTb
    dut : Top
    mon : Mon passive

    function sample(t: SampleTxn)
        mon.dut = dut
        mon.observe()
        assert t.check else fail("record param field did not lower")
        assert mon.last == dut.count else fail("monitor sampled wrong value")
    end function sample
end testbench MonTb

impl MonTest for MonTb
    run
        let t : SampleTxn
        sample(t)
        assert mon.samples == 1 else fail("monitor did not sample")
    end run
end impl MonTest
"#;
    let prog = lower_src(src).expect("passive monitor helper lowers");
    verify::verify_program(&prog).expect("verifies");
    assert!(
        prog.transactors.is_empty(),
        "passive monitor helper routes through component lowering"
    );
    assert!(
        prog.components.iter().any(|c| c.name == "Mon"),
        "monitor component schema exists"
    );
}

/// Unknown methods and arity mismatches are hard lowering errors â€”
/// v1 deferred both to C++ compile failures.
#[test]
fn transactor_call_resolution_is_checked() {
    let unknown = XACTOR_SRC.replace("xt.pulse(3)", "xt.nosuch(3)");
    let err = lower_src(&unknown).unwrap_err();
    assert!(matches!(err, lower::LowerError::Invalid(_)), "{err:?}");
    assert!(err.to_string().contains("no method `nosuch`"), "{err}");

    let arity = XACTOR_SRC.replace("xt.pulse(3)", "xt.pulse(3, 4)");
    let err = lower_src(&arity).unwrap_err();
    assert!(matches!(err, lower::LowerError::Invalid(_)), "{err:?}");
    assert!(
        err.to_string()
            .contains("takes 1 argument(s), call passes 2"),
        "{err}"
    );

    let void_let = XACTOR_SRC.replace("let v = xt.readv()", "let v = xt.pulse(3)");
    let err = lower_src(&void_let).unwrap_err();
    assert!(err.to_string().contains("returns no value"), "{err}");
}

/// The DUT bind statement is validated: the target must be the
/// transactor's module-typed field and the value must be the test DUT.
#[test]
fn transactor_dut_bind_is_validated() {
    let src = XACTOR_SRC.replace("xt.dut = dut", "xt.dut = 5");
    let msg = assert_unsupported(&lower_src(&src).unwrap_err());
    assert!(msg.contains("something other than the test DUT"), "{msg}");
}

/// A timed `wait until` inside a transactor method lowers to v1's
/// SYNCHRONOUS shape (spec Â§7.4's "synchronous context"): a method body
/// has no scheduler to defer to, so the budget is read once and the
/// predicate polled with `tick()` per cycle â€” not the coroutine
/// `wait_until_timeout` awaiter the run body uses.
#[test]
fn transactor_method_timed_wait_lowers_to_the_sync_poll_loop() {
    let timed = XACTOR_SRC.replace(
        "wait 1 cycle\n            dut.en = 0",
        "wait until dut.count_out == 1 timeout 5 cycles\n            dut.en = 0",
    );
    let prog = lower_src(&timed).expect("a timed wait in a method lowers");
    verify::verify_program(&prog).expect("verifies");
    let method = prog
        .functions
        .iter()
        .find(|f| f.name == "Xt_pulse")
        .expect("the method body");
    assert!(
        method
            .blocks
            .iter()
            .any(|b| matches!(b.terminator, ir::Terminator::WaitUntilTimeout { .. })),
        "the method body must carry the timeout terminator"
    );

    let cpp = emit_cpp_src(&timed);
    for needle in [
        "int64_t _wu_budget = (int64_t)(",
        "int64_t _wu_start = (int64_t)cycle_count;",
        ") < _wu_budget) tick();",
        "ctx.errors++;",
    ] {
        assert!(cpp.contains(needle), "missing `{needle}` in:\n{cpp}");
    }
    // The coroutine awaiter must NOT appear in the method body â€” a
    // method lambda cannot `co_await`.
    let body_start = cpp.find("Xt_pulse = [&]").expect("the method lambda");
    let body_end = body_start
        + cpp[body_start..]
            .find("\n    };")
            .expect("the method lambda closes");
    let body = &cpp[body_start..body_end];
    assert!(body.contains("_wu_budget"), "wrong slice: {body}");
    assert!(
        !body.contains("co_await"),
        "a synchronous method body must not co_await:\n{body}"
    );
}

/// The remaining suspension form whose sync emission is out of subset is
/// rejected at lowering with a method-specific message.
#[test]
fn transactor_method_sync_only_waits() {
    let clocked = XACTOR_SRC.replace(
        "wait 1 cycle\n            dut.en = 0",
        "wait 1 cycle on clk\n            dut.en = 0",
    );
    let msg = assert_unsupported(&lower_src(&clocked).unwrap_err());
    assert!(
        msg.contains("`wait N cycles on <clock>` inside a transactor method"),
        "{msg}"
    );
}

/// Bus calls suspend, so they are statement-level only: nesting one in
/// an expression is a precise rejection, not the generic method-call
/// message.
#[test]
fn bus_call_in_expression_position_is_unsupported() {
    let src = r#"
bus MemBus
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
end bus MemBus

testbench ExprTb
    dut : TlmMemory
end testbench ExprTb

impl ExprTest for ExprTb
    let mem : MemBus = bind dut
    run
        assert mem.read(5) == 261 else fail("nope")
    end run
end impl ExprTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("bus method calls in expression position"),
        "{msg}"
    );
}

/// `bind ... with { ... }` signal remaps now lower: the binding's
/// `remap` table records the `(channel, signal) â†’ port` override so the
/// wire emission resolves through it (mirrors v1's `bus_remap`).
#[test]
fn bus_bind_remap_lowers() {
    let src = r#"
bus RemapBus
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
end bus RemapBus

testbench RemapTb
    dut : TlmMemory
end testbench RemapTb

impl RemapTest for RemapTb
    let mem : RemapBus = bind dut with {
        read.req_valid: "mem_read_req_valid"
    }
    run
        let x = mem.read(5)
    end run
end impl RemapTest
"#;
    let prog = lower_src(src).expect("lowers");
    let binding = &prog.testbenches[0].bus_bindings[0];
    assert_eq!(
        binding.remap,
        vec![(
            ("read".to_string(), "req_valid".to_string()),
            "mem_read_req_valid".to_string()
        )]
    );
    // The override resolves; an unmapped signal falls back to the
    // `<field>_<channel>_<signal>` convention.
    assert_eq!(binding.wire_name("read", "req_valid"), "mem_read_req_valid");
    assert_eq!(binding.wire_name("read", "addr"), "mem_read_addr");
}

/// Regression for PR #424: the gated-bus access scan must recurse into
/// indexed aggregate reads too, not just top-level expressions. Without
/// this, a gated-OFF plain bus signal used as a `Vec` index slips
/// through emit-time validation and the generated C++ references a DUT
/// port `arch build` omitted.
#[test]
fn gated_bus_access_in_record_index_is_rejected() {
    let src = r#"
bus GatedIdxBus
    param EN: const = 0;
    generate_if EN
        idx: in uint<8>;
    end generate_if
end bus GatedIdxBus

transaction Packet
    data : Vec<uint<8>, 4>
end transaction Packet

testbench GateTb
    dut : Top
end testbench GateTb

impl GatedIdxTest for GateTb
    let b : GatedIdxBus = bind dut
    run
        let p : Packet
        assert p.data[b.idx] == 0 else fail("bad")
    end run
end impl GatedIdxTest
"#;
    let merged = merged_src(src);
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let err = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).unwrap_err();
    assert!(
        err.to_string().contains("signal `idx` is gated OFF"),
        "expected gated-OFF diagnostic for nested record index: {err}"
    );
}

/// `SeqIndex` is currently produced by internal `for t in <tseq>` lowering,
/// whose generated index is a local counter. This direct IR regression keeps
/// the emit-side traversal honest if future lowering allows a richer index.
#[test]
fn gated_bus_access_in_seq_index_is_rejected() {
    let src = r#"
bus GatedIdxBus
    param EN: const = 0;
    generate_if EN
        idx: in uint<8>;
    end generate_if
end bus GatedIdxBus

testbench GateTb
    dut : Top
end testbench GateTb

impl GatedSeqIdxTest for GateTb
    let b : GatedIdxBus = bind dut
    run
        wait 1 cycle
    end run
end impl GatedSeqIdxTest
"#;
    let merged = merged_src(src);
    let mut prog = lower::lower_program(&merged).expect("lowers");
    let run = prog.tests[0].run;
    let run_fn = &mut prog.functions[run.index()];
    run_fn.blocks[0].stmts.push(ir::Stmt::Assign(
        ir::LocalId(0),
        ir::Expr::SeqIndex {
            seq: ir::LocalId(0),
            index: Box::new(ir::Expr::Port(ir::PortRef {
                testbench_field: "dut".to_string(),
                port_path: vec!["b".to_string(), "idx".to_string()],
                aggregate_path: false,
                direction: None,
                width: None,
                access: ir::PortAccess::Port,
                lane: None,
            })),
        },
    ));
    let err = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).unwrap_err();
    assert!(
        err.to_string().contains("signal `idx` is gated OFF"),
        "expected gated-OFF diagnostic for nested seq index: {err}"
    );
}

/// A remap path must be exactly `<channel>.<signal>` (2 segments) â€”
/// a single- or 3+-segment path is a hard lowering error, matching
/// v1's `bind ... with` translation.
#[test]
fn bus_bind_remap_malformed_path_is_invalid() {
    let src = r#"
bus RemapBus
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
end bus RemapBus

testbench RemapTb
    dut : TlmMemory
end testbench RemapTb

impl RemapTest for RemapTb
    let mem : RemapBus = bind dut with {
        read.req.valid: "mem_read_req_valid"
    }
    run
        let x = mem.read(5)
    end run
end impl RemapTest
"#;
    let err = lower_src(src).unwrap_err();
    assert!(
        matches!(err, lower::LowerError::Invalid(ref m) if m.contains("must be exactly")),
        "{err:?}"
    );
}

/// `bind ... with { ch.sig: "port" }` on a HANDSHAKE-CHANNEL bus now
/// lowers (previously rejected as a follow-up slice). A mapped
/// `<channel>.<signal>` access collapses to the single-segment override
/// flat name (`aw.valid` â†’ `s_axi_awvalid`); an UNMAPPED channel signal
/// keeps the `<bind>_<ch>_<sig>` convention, and the already-flattened
/// `<ch>_<sig>` form is never remapped â€” mirroring v1's
/// `try_emit_bus_field_access`, which remaps the channel form only.
#[test]
fn bus_bind_remap_handshake_channel_lowers() {
    let src = r#"
bus HsBus
    handshake_channel aw: send kind: valid_ready
        addr: uint<8>
    end handshake_channel aw
end bus HsBus

testbench HsTb
    dut : AxiLiteRegs
end testbench HsTb

impl HsTest for HsTb
    let s_axi : HsBus = bind dut with {
        aw.valid: "s_axi_awvalid", aw.addr: "s_axi_awaddr"
    }
    run
        s_axi.aw.addr = 7
        s_axi.aw.valid = 1
        s_axi.aw.ready = 0
    end run
end impl HsTest
"#;
    let prog = lower_src(src).expect("lowers");
    let dump = format!("{prog}");
    // Mapped channel signals collapse to the override flat name (a
    // single-segment path â€” dump-ir renders it verbatim).
    assert!(dump.contains("DutWrite(dut.s_axi_awaddr, 7)"), "{dump}");
    assert!(dump.contains("DutWrite(dut.s_axi_awvalid, 1)"), "{dump}");
    // Unmapped channel signal (`ready`) keeps the canonical 3-segment
    // path (dump-ir renders segments dotted; the backend joins with `_`
    // â†’ `s_axi_aw_ready`).
    assert!(dump.contains("DutWrite(dut.s_axi.aw.ready, 0)"), "{dump}");
}

/// Locks the dump-ir text for the AMBA bind-remap fixture: a
/// handshake-channel `BusAxiLite` bound `with { ch.sig: "port" }` to a
/// DUT using AMBA one-word port names, exercised through both a
/// bound-transactor BFM (placeholder-prefix fill) and direct test-scope
/// channel access. Every mapped wire resolves to the AMBA name.
#[test]
fn bind_remap_dump_ir_snapshot() {
    let prog = lower_with_stdlib_bus("bind_remap_test.harc", "BusAxiLite.arch").expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("bind_remap_dump_ir", format!("{prog}"));
}

/// Unknown channel signals are hard errors with v1's diagnostic text
/// (v1 surfaces them as codegen errors; the IR rejects at lowering).
#[test]
fn bus_unknown_signal_is_invalid() {
    let src = r#"
bus TypoBus
    handshake_channel aw: send kind: valid_ready
        addr: uint<8>
    end handshake_channel aw
end bus TypoBus

testbench TypoTb
    dut : AxiLiteRegs
end testbench TypoTb

impl TypoTest for TypoTb
    let axil : TypoBus = bind dut
    run
        axil.aw.addrr = 24
    end run
end impl TypoTest
"#;
    let err = lower_src(src).unwrap_err();
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "typo must be a hard error: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("channel `aw` has no signal `addrr`")
            && msg.contains("valid: valid, ready, addr"),
        "{msg}"
    );
}

/// The blocking target-side TLM fixtures now lower end-to-end: a
/// `transactor X bound to <Bus>` with `thread bus.<m>(...)` responder
/// bodies, persistent scalar state fields read from the test, and the
/// per-instance state struct + actor schemas.
#[test]
fn tlm_target_blocking_responder_lowers() {
    let prog = lower_src(&fixture("tlm_target_thread_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    // The bound transactor carries the bus + one blocking target method.
    let x = &prog.transactors[0];
    assert_eq!(x.bound_bus.as_deref(), Some("TlmMemBus"));
    assert_eq!(x.target_methods.len(), 1);
    assert_eq!(x.target_methods[0].name, "read");
    assert!(x.target_methods[0].has_ret);
    // The test binds one passive responder actor on bus binding `mem`.
    let tb = &prog.testbenches[0];
    assert_eq!(tb.target_tlm_actors.len(), 1);
    assert_eq!(tb.target_tlm_actors[0].instance, "target");
    assert_eq!(tb.target_tlm_actors[0].bus_field, "mem");
}

/// State fields lower as `TransactorState` reads/writes, instance-filled
/// at the test bind, and the test reads them back (`target.read_count`).
#[test]
fn tlm_target_state_fields_lower() {
    let prog = lower_src(&fixture("tlm_target_thread_if_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let x = &prog.transactors[0];
    let names: Vec<&str> = x.state_fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["read_count", "prep_acc"]);
    // The responder body's state writes are instance-filled to `target`.
    let body = prog.function(x.target_methods[0].function);
    let filled = body.blocks.iter().flat_map(|b| &b.stmts).any(|s| {
        matches!(
            s,
            ir::Stmt::TransactorStateWrite { instance, .. } if instance == "target"
        )
    });
    assert!(
        filled,
        "responder body must carry instance-filled state writes"
    );
}

/// #494 P0a: a bound-to TARGET transactor may now carry NON-SCALAR
/// persistent state â€” a `queue<Record>` and a `queue<scalar>`. The state
/// fields lower as `StateFieldKind::Queue` reusing the scoreboard/
/// component `QueueElem` machinery; the responder body's push/pop are
/// instance-filled to the bound `responder` actor.
#[test]
fn target_nonscalar_queue_state_lowers() {
    let prog = lower_src(&fixture("target_nonscalar_state_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let x = &prog.transactors[0];
    assert_eq!(x.bound_bus.as_deref(), Some("TlmMemBus"));
    // Two non-scalar state fields: a record queue and a scalar queue.
    let names: Vec<&str> = x.state_fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["pending", "log_addrs"]);
    assert!(matches!(
        &x.state_fields[0].kind,
        ir::StateFieldKind::Queue {
            elem: ir::QueueElem::Record(_)
        }
    ));
    assert!(matches!(
        &x.state_fields[1].kind,
        ir::StateFieldKind::Queue {
            elem: ir::QueueElem::Scalar { signed: false }
        }
    ));
    // The responder body's state-queue push/pop are instance-filled to
    // the bound `responder` actor (placeholder resolved at test bind).
    let body = prog.function(x.target_methods[0].function);
    let has_push = body.blocks.iter().flat_map(|b| &b.stmts).any(|s| {
        matches!(
            s,
            ir::Stmt::TransactorStateQueuePush { instance, .. } if instance == "responder"
        )
    });
    let has_pop = body.blocks.iter().flat_map(|b| &b.stmts).any(|s| {
        matches!(
            s,
            ir::Stmt::TransactorStateQueuePop { instance, .. } if instance == "responder"
        )
    });
    assert!(
        has_push,
        "responder body must carry instance-filled state-queue push"
    );
    assert!(
        has_pop,
        "responder body must carry instance-filled state-queue pop"
    );
}

/// #494 P0a: the C++ shape for non-scalar target-transactor state reuses
/// the scoreboard/component queue machinery verbatim â€” the per-instance
/// state struct carries `harc_rt::HarcQueue<Beat>` / `HarcQueue<uint64_t>`
/// members, and push/pop/size/empty emit the same calls scoreboards do.
#[test]
fn target_nonscalar_queue_state_emits_harcqueue() {
    let cpp = emit_fixture_cpp("target_nonscalar_state_test.harc");
    // Per-instance state struct members are HarcQueue<T> â€” record element
    // by struct name, scalar element widened to uint64_t.
    assert!(
        cpp.contains("harc_rt::HarcQueue<Beat> pending;"),
        "record-queue state member; got:\n{cpp}"
    );
    assert!(
        cpp.contains("harc_rt::HarcQueue<uint64_t> log_addrs;"),
        "scalar-queue state member; got:\n{cpp}"
    );
    // Push/pop/size on the per-instance struct member.
    assert!(
        cpp.contains("responder.pending.push("),
        "record-queue push; got:\n{cpp}"
    );
    assert!(
        cpp.contains("responder.pending.pop()"),
        "record-queue pop; got:\n{cpp}"
    );
    assert!(
        cpp.contains("(uint64_t)responder.log_addrs.size()"),
        "size() cast to uint64; got:\n{cpp}"
    );
    assert!(
        cpp.contains("responder.log_addrs.pop()"),
        "test-scope scalar-queue pop; got:\n{cpp}"
    );
}

/// #494 P0a follow-up: a WHOLE value-record target-transactor state field
/// (`last : Beat`) lowers to `StateFieldKind::Record`, and its subfield
/// read/write ops are instance-filled to the bound `responder` actor.
#[test]
fn target_record_state_lowers() {
    let prog = lower_src(&fixture("target_record_state_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let x = &prog.transactors[0];
    assert_eq!(x.bound_bus.as_deref(), Some("TlmMemBus"));
    // One whole-record state field.
    let names: Vec<&str> = x.state_fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["last"]);
    assert!(matches!(
        &x.state_fields[0].kind,
        ir::StateFieldKind::Record { .. }
    ));
    // The responder body's record-subfield writes are instance-filled to
    // the bound `responder` actor (placeholder resolved at test bind).
    let body = prog.function(x.target_methods[0].function);
    let subfield_writes: Vec<String> = body
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter_map(|s| match s {
            ir::Stmt::TransactorStateRecordFieldWrite {
                instance,
                field,
                path,
                ..
            } if instance == "responder" && field == "last" => Some(path.join(".")),
            _ => None,
        })
        .collect();
    assert_eq!(
        subfield_writes,
        ["addr", "data"],
        "responder body must carry instance-filled record-subfield writes"
    );
    // And a subfield READ (`last.data`) instance-filled likewise (the
    // returned value / the in-body asserts).
    let has_subfield_read = body
        .blocks
        .iter()
        .any(|b| block_has_state_record_field_read(b, "responder", "last"));
    assert!(
        has_subfield_read,
        "responder body must carry an instance-filled record-subfield read"
    );
}

/// True when any expression reachable from `block` is a
/// `TransactorStateRecordField` read on `<instance>.<field>`.
fn block_has_state_record_field_read(block: &ir::BasicBlock, instance: &str, field: &str) -> bool {
    fn in_expr(e: &ir::Expr, instance: &str, field: &str) -> bool {
        match e {
            ir::Expr::TransactorStateRecordField {
                instance: i,
                field: f,
                ..
            } => i == instance && f == field,
            ir::Expr::Binary(_, a, b) => in_expr(a, instance, field) || in_expr(b, instance, field),
            ir::Expr::Unary(_, a)
            | ir::Expr::WidthCast { inner: a, .. }
            | ir::Expr::BitSlice { target: a, .. } => in_expr(a, instance, field),
            ir::Expr::Ternary(c, t, f) => {
                in_expr(c, instance, field)
                    || in_expr(t, instance, field)
                    || in_expr(f, instance, field)
            }
            ir::Expr::Call(_, args) => args.iter().any(|a| in_expr(a, instance, field)),
            _ => false,
        }
    }
    // The returned subfield (`return last.data`) lowers to an `Assign`
    // into the `__ret` temp, so the statement scan covers it; the asserts
    // read subfields too. `Terminator::Return` carries no expression.
    block.stmts.iter().any(|s| match s {
        ir::Stmt::Assign(_, e)
        | ir::Stmt::TransactorStateWrite { value: e, .. }
        | ir::Stmt::TransactorStateRecordFieldWrite { value: e, .. } => in_expr(e, instance, field),
        ir::Stmt::AssertCheck { cond, on_fail } => {
            in_expr(cond, instance, field)
                || on_fail
                    .args
                    .iter()
                    .any(|a| in_expr(&a.expr, instance, field))
        }
        _ => false,
    })
}

/// #494 P0a follow-up: the C++ shape for a whole-record target-transactor
/// state field carries the record struct BY VALUE (`Beat last{};`), and
/// subfield read/write emit `<instance>.<field>.<sub>` â€” no `HarcQueue`,
/// no `VBeat*` (the v1 miscompile this feature routes around).
#[test]
fn target_record_state_emits_value_record() {
    let cpp = emit_fixture_cpp("target_record_state_test.harc");
    assert!(
        cpp.contains("Beat last{};"),
        "whole-record state member emitted by value; got:\n{cpp}"
    );
    assert!(
        cpp.contains("responder.last.addr = "),
        "record-subfield write; got:\n{cpp}"
    );
    assert!(
        cpp.contains("responder.last.data = "),
        "record-subfield write; got:\n{cpp}"
    );
    assert!(
        cpp.contains("responder.last.data"),
        "record-subfield read; got:\n{cpp}"
    );
    // Guard against the v1 miscompile shape leaking into tbir.
    assert!(
        !cpp.contains("VBeat"),
        "tbir must not emit the v1 `VBeat*` shape; got:\n{cpp}"
    );
}

/// Nested forwarding: a bound-to responder re-issues a downstream
/// blocking TLM call (`let raw = back.read(addr)`) against a test-scope
/// bus binding. The pre-scanned downstream binding makes `back` resolve
/// to a `TransactorMethod` call edge inside the responder body, instead
/// of the generic transactor-method rejection.
#[test]
fn tlm_target_nested_forwarding_lowers() {
    let prog = lower_src(&fixture("tlm_target_forwarding_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let x = &prog.transactors[0];
    let body = prog.function(x.target_methods[0].function);
    // The responder body carries a downstream `back.read` blocking call
    // edge (Assign-RHS TransactorMethod), the nested-forwarding shape.
    let has_downstream = body.blocks.iter().flat_map(|b| &b.stmts).any(|s| {
        matches!(
            s,
            ir::Stmt::Assign(_, ir::Expr::Call(
                ir::CallTarget::TransactorMethod { bus_field, method },
                _,
            )) if bus_field == "back" && method == "read"
        )
    });
    assert!(
        has_downstream,
        "responder body must carry the downstream back.read edge"
    );
}

/// Fork-forwarding: a responder issues two downstream `fork
/// back.read_ooo(...)` requests and `join_all`s them. The #390
/// fork/join machinery composes inside the responder body once the
/// downstream OOO binding is in scope.
#[test]
fn tlm_target_fork_forwarding_lowers() {
    let prog = lower_src(&fixture("tlm_target_fork_forwarding_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let x = &prog.transactors[0];
    let body = prog.function(x.target_methods[0].function);
    let stmts: Vec<&ir::Stmt> = body.blocks.iter().flat_map(|b| &b.stmts).collect();
    let forks = stmts
        .iter()
        .filter(|s| matches!(s, ir::Stmt::TlmFork(_)))
        .count();
    let joins = stmts
        .iter()
        .filter(|s| matches!(s, ir::Stmt::TlmJoinAll(_)))
        .count();
    assert_eq!(forks, 2, "responder body must carry two downstream forks");
    assert_eq!(joins, 1, "responder body must carry one join_all");
}

/// `out_of_order tags N` target threads ARE lowered: the responder
/// method carries the folded, range-checked tag count on
/// `TargetTlmMethodSchema::ooo_tags` (emission generates the per-tag
/// dispatcher + N lane coroutines + arbiter). The blocking siblings keep
/// `ooo_tags == None`. The mixed-mode pairing fixture exercises both.
#[test]
fn tlm_target_ooo_responder_lowers() {
    let prog = lower_src(&fixture("tlm_pairing_arch_initiator_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let x = &prog.transactors[0];
    let ooo = x
        .target_methods
        .iter()
        .find(|m| m.name == "read_ooo")
        .expect("read_ooo responder present");
    assert_eq!(
        ooo.ooo_tags,
        Some(2),
        "read_ooo carries `out_of_order tags 2`"
    );
    // The blocking responders stay single-lane (no tag count).
    for m in x.target_methods.iter().filter(|m| m.name != "read_ooo") {
        assert_eq!(
            m.ooo_tags, None,
            "blocking responder `{}` has no tags",
            m.name
        );
    }
}

/// A zero / out-of-range literal tag count is rejected at lowering
/// (matching v1's 1..=64 gate), never silently emitted as a 0-lane or
/// 65-lane responder.
#[test]
fn tlm_target_ooo_responder_tag_count_range() {
    let mk = |tags: &str| {
        format!(
            r#"
bus MemBus
    tlm_method read_ooo(addr: uint<8>) -> uint<32>: out_of_order tags {tags};
end bus MemBus

transactor MemTarget bound to MemBus
    thread bus.read_ooo(addr: uint<8>)
        return 256 + addr
    end thread
end transactor MemTarget

testbench Tb
    dut : Dummy
end testbench Tb

impl T for Tb
    let mem : MemBus = bind dut
    let target : MemTarget passive = bind mem
    run
        wait 1 cycle
    end run
end impl T
"#
        )
    };
    assert!(lower_src(&mk("0")).is_err(), "tags 0 must be rejected");
    assert!(lower_src(&mk("65")).is_err(), "tags 65 must be rejected");
    assert!(lower_src(&mk("2")).is_ok(), "tags 2 must lower");
}

/// The responder `TbFunction`s are shared per transactor TYPE; binding
/// the same bound transactor to two instances across two tests would
/// clobber the first test's instance-filled bodies. The subset is one
/// passive instance per bound transactor â€” lowering rejects the second
/// bind loudly (in ALL build profiles), never silently mis-emits.
#[test]
fn tlm_target_multi_instance_unsupported() {
    let src = r#"
bus MemBus
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
end bus MemBus

transactor MemTarget bound to MemBus
    read_count : uint<32> default 0
    thread bus.read(addr: uint<8>)
        read_count = read_count + 1
        return 256 + addr
    end thread
end transactor MemTarget

testbench TbA
    dut : InitA
end testbench TbA

impl TestA for TbA
    let mem : MemBus = bind dut
    let target : MemTarget passive = bind mem
    run
        dut.rst = 1
        wait 1 cycle
    end run
end impl TestA

testbench TbB
    dut : InitA
end testbench TbB

impl TestB for TbB
    let mem2 : MemBus = bind dut
    let responder : MemTarget passive = bind mem2
    run
        dut.rst = 1
        wait 1 cycle
    end run
end impl TestB
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("more than one instance"),
        "expected the multi-instance rejection: {msg}"
    );
}

/// Locks the dump-ir text for a state-bearing target responder: the
/// `bound to` transactor schema, the state-field list, and the responder
/// body's `TransactorState` reads/writes + loop/branch structure.
#[test]
fn tlm_target_thread_if_dump_ir_snapshot() {
    let prog = lower_src(&fixture("tlm_target_thread_if_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("tlm_target_thread_if_dump_ir", format!("{prog}"));
}

/// Locks the emitted-cpp shape for the target responder actor: the
/// per-instance state struct, the background-coroutine actor (req_ready/
/// rsp_valid handshake, arg capture, body loop-switch, response drive),
/// and the test-scope `target.<field>` reads.
#[test]
fn tlm_target_thread_if_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "tlm_target_thread_if_emitted_cpp",
        emit_fixture_cpp("tlm_target_thread_if_test.harc")
    );
}

/// `bind ... with { method.sig: "port" }` signal remaps survive
/// lowering: the binding line carries the sorted `(channel, signal) â†’
/// port` table. The fixture binds with name `m`, so the
/// `<field>_<channel>_<signal>` convention would produce `m_read_*` â€”
/// every entry remaps to the real `mem_read_*` port, so the table is
/// load-bearing, not an identity no-op.
#[test]
fn bus_bind_remap_dump_ir_snapshot() {
    let prog = lower_src(&fixture("tlm_bind_remap_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let text = format!("{prog}");
    assert!(
        text.contains(" with{poke.addr=mem_poke_addr") && text.contains("read.addr=mem_read_addr"),
        "bind remap table (sorted by key) must ride the binding line:\n{text}"
    );
    insta::assert_snapshot!("bus_bind_remap_dump_ir", text);
}

/// Locks the emitted C++ for the remapped blocking call edges: every
/// req/rsp wire resolves through the `bind ... with` override to
/// `dut->mem_read_*` / `dut->mem_poke_*` â€” the `m_read_*` convention
/// names never appear, proving the remap rewrites the wire emission.
#[test]
fn bus_bind_remap_emitted_cpp_snapshot() {
    let cpp = emit_fixture_cpp("tlm_bind_remap_test.harc");
    assert!(
        cpp.contains("dut->mem_read_req_valid") && !cpp.contains("dut->m_read_req_valid"),
        "remapped wires must override the convention name:\n{cpp}"
    );
    insta::assert_snapshot!("bus_bind_remap_emitted_cpp", cpp);
}

/// Transactor-call seam rule, verifier side: the call edge is pinned
/// to the whole-Assign-RHS position in Run/Check functions and must
/// resolve against the owning testbench's bus bindings.
#[test]
fn verifier_pins_transactor_call_seam() {
    let prog = lower_src(&fixture("tlm_method_blocking_bus_test.harc")).expect("lowers");
    let run_fn = prog.tests[0].run.index();

    // Locate an Assign whose RHS is a TransactorMethod call.
    let find_call = |f: &ir::TbFunction| -> (usize, usize) {
        for (bi, b) in f.blocks.iter().enumerate() {
            for (si, s) in b.stmts.iter().enumerate() {
                if let ir::Stmt::Assign(
                    _,
                    ir::Expr::Call(ir::CallTarget::TransactorMethod { .. }, _),
                ) = s
                {
                    return (bi, si);
                }
            }
        }
        panic!("no TransactorMethod Assign found");
    };

    // 1. Nested in an expression â†’ seam violation.
    let mut broken = prog.clone();
    {
        let f = &mut broken.functions[run_fn];
        let (bi, si) = find_call(f);
        let ir::Stmt::Assign(l, call) = f.blocks[bi].stmts[si].clone() else {
            unreachable!()
        };
        f.blocks[bi].stmts[si] = ir::Stmt::Assign(
            l,
            ir::Expr::Binary(
                ir::BinOp::Add,
                Box::new(call),
                Box::new(ir::Expr::Literal {
                    value: 1,
                    ty: ir::IrType::Unknown,
                }),
            ),
        );
    }
    let errs = verify::verify_program(&broken).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| matches!(e, verify::VerifyError::BadTransactorCall { .. })),
        "{errs:?}"
    );

    // 2. Unresolved binding â†’ seam violation.
    let mut broken = prog.clone();
    {
        let f = &mut broken.functions[run_fn];
        let (bi, si) = find_call(f);
        if let ir::Stmt::Assign(
            _,
            ir::Expr::Call(ir::CallTarget::TransactorMethod { bus_field, .. }, _),
        ) = &mut f.blocks[bi].stmts[si]
        {
            *bus_field = "ghost".to_string();
        }
    }
    let errs = verify::verify_program(&broken).unwrap_err();
    assert!(
        errs.iter().any(|e| matches!(
            e,
            verify::VerifyError::BadTransactorCall { detail, .. } if detail.contains("no bus binding `ghost`")
        )),
        "{errs:?}"
    );

    // 3. Arity drift against the schema â†’ seam violation.
    let mut broken = prog.clone();
    {
        let f = &mut broken.functions[run_fn];
        let (bi, si) = find_call(f);
        if let ir::Stmt::Assign(_, ir::Expr::Call(_, args)) = &mut f.blocks[bi].stmts[si] {
            args.push(ir::Expr::Literal {
                value: 0,
                ty: ir::IrType::Unknown,
            });
        }
    }
    let errs = verify::verify_program(&broken).unwrap_err();
    assert!(
        errs.iter().any(|e| matches!(
            e,
            verify::VerifyError::BadTransactorCall { detail, .. } if detail.contains("arity mismatch")
        )),
        "{errs:?}"
    );

    // 4. Call edge in a non-Run/Check function â†’ seam violation
    //    (pure helpers must stay suspension-free and placement-neutral).
    let mut broken = prog.clone();
    broken.functions[run_fn].kind = ir::FunctionKind::Helper;
    let errs = verify::verify_program(&broken).unwrap_err();
    assert!(
        errs.iter().any(|e| matches!(
            e,
            verify::VerifyError::BadTransactorCall { detail, .. } if detail.contains("Helper")
        )),
        "{errs:?}"
    );
}

/// Placement classifies blocks carrying a transactor call edge as
/// timing-tolerant â€” the boundary the lowering now actually produces.
#[test]
fn placement_classifies_transactor_call_block_timing_tolerant() {
    let prog = lower_src(&fixture("tlm_method_blocking_bus_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let profile = placement::TargetProfile::single_site();
    let table = placement::run(&prog, &profile);
    let run_id = prog.tests[0].run;
    let f = prog.function(run_id);
    let (bi, _) = f
        .blocks
        .iter()
        .enumerate()
        .find(|(_, b)| {
            b.stmts.iter().any(|s| {
                matches!(
                    s,
                    ir::Stmt::Assign(
                        _,
                        ir::Expr::Call(ir::CallTarget::TransactorMethod { .. }, _)
                    )
                )
            })
        })
        .expect("a block carries the call edge");
    let (_, timing) = table.blocks[&(run_id, ir::BlockId(bi as u32))];
    assert_eq!(timing, placement::TimingClass::TimingTolerant);
}

/// Out-of-subset transactor shapes reject with precise messages:
/// event fields (the sequencer-driven form) and >64-bit method params
/// (the tbir value model is u64). Scalar STATE fields, by contrast, now
/// lower (state-field slice) â€” asserted positively here.
#[test]
fn transactor_shape_rejections() {
    let event_src = r#"
transaction Req
    addr : uint<8>
end transaction Req

transactor Ev
    dut : Top

    when active
        req : in event<Req>
    end when
end transactor Ev

testbench EvTb
    dut : Top
    ev  : Ev active
end testbench EvTb

impl EvTest for EvTb
    run
        wait 1 cycle
    end run
end impl EvTest
"#;
    let msg = assert_unsupported(&lower_src(event_src).unwrap_err());
    assert!(msg.contains("directional event field `req`"), "{msg}");
    // The directional SCALAR spelling is a different verdict â€” v1
    // models the event field and flattens the scalar one.
    let scalar_src = event_src.replace("req : in event<Req>", "req : in uint<8>");
    let msg = assert_not_implemented(
        &lower_src(&scalar_src).unwrap_err(),
        lower::V1Status::SilentlyMisLowers,
    );
    assert!(msg.contains("directional scalar field `req`"), "{msg}");

    // A scalar state field now lowers: the transactor carries it on its
    // schema and the testbench records the instance for per-instance
    // state materialization (state-field slice).
    let state_src = event_src.replace("req : in event<Req>", "count : uint<32>");
    let prog = lower_src(&state_src).expect("scalar state field lowers");
    let xs = &prog.transactors[0];
    assert_eq!(xs.state_fields.len(), 1, "state field on schema");
    assert_eq!(xs.state_fields[0].name, "count");
    assert_eq!(
        prog.testbenches[0].unbound_state_actors,
        vec![("ev".to_string(), ir::TransactorId(0))],
        "stateful instance recorded for per-instance materialization",
    );

    // A second ACTIVE stateful instance of the same type now lowers
    // (#494 P1b): the state-receiver ABI serves any number of instances
    // from one shared method body, so both instances are recorded for
    // per-instance state materialization.
    let two_src = state_src.replace(
        "    ev  : Ev active",
        "    ev  : Ev active\n    ev2 : Ev active",
    );
    let prog = lower_src(&two_src).expect("two active stateful instances lower");
    assert_eq!(
        prog.testbenches[0].unbound_state_actors,
        vec![
            ("ev".to_string(), ir::TransactorId(0)),
            ("ev2".to_string(), ir::TransactorId(0)),
        ],
        "both active stateful instances recorded for per-instance materialization",
    );

    // A method value param wider than 128 bits now lowers via v1's
    // `HarcWide<N>` register-array value model (`local_scalar_cty`): the
    // param carries its declared `uint<256>` IrType so the backend
    // declares the wide storage instead of truncating. (See
    // `wide1024_tlm_test` for the end-to-end echo equivalence.)
    let wide_src = r#"
transactor Wx
    dut : Top

    when active
        hookable big(v: uint<256>)
            dut.data = v
        end big
    end when
end transactor Wx

testbench WxTb
    dut : Top
    wx  : Wx active
end testbench WxTb

impl WxTest for WxTb
    run
        wx.dut = dut
        wx.big(0)
    end run
end impl WxTest
"#;
    let prog = lower_src(wide_src).expect("wide (>128b) method param lowers");
    let big = &prog.functions[0];
    assert_eq!(big.name, "Wx_big");
    assert_eq!(
        big.params[0].ty,
        ir::IrType::UInt(Some(256)),
        "wide method param carries its declared width",
    );
}

/// Verifier net: a `TransactorMethod` call edge nested in expression
/// position (i.e. anywhere but the root of `Stmt::TransactorCall`) is
/// a `BadTransactorCall` â€” lowering never produces it, so reaching it
/// means a pass corrupted the IR.
#[test]
fn verifier_rejects_call_edge_in_expression_position() {
    let mut prog = lower_src(XACTOR_SRC).expect("lowers");
    verify::verify_program(&prog).expect("clean before mutation");
    // Rewrite the first TransactorCall into a plain Assign of the
    // call-edge expression.
    let run_id = prog.tests[0].run;
    let run = &mut prog.functions[run_id.index()];
    let mut mutated = false;
    for b in &mut run.blocks {
        for s in &mut b.stmts {
            if let ir::Stmt::TransactorCall { call, .. } = s {
                let dest = ir::LocalId(0);
                *s = ir::Stmt::Assign(dest, call.clone());
                mutated = true;
                break;
            }
        }
        if mutated {
            break;
        }
    }
    assert!(mutated, "fixture carries a TransactorCall");
    let errs = verify::verify_program(&prog).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| matches!(e, verify::VerifyError::BadTransactorCall { .. })),
        "expected BadTransactorCall, got: {errs:?}"
    );
}

/// `lower_coroutine` tags transactor method bodies (they are the
/// Tier-0 FSM candidates): the suspension inside `pulse` becomes a
/// state boundary.
#[test]
fn lower_coroutine_tags_transactor_bodies() {
    let prog = lower_src(XACTOR_SRC).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let meta = lower_coroutine::run(&prog).expect("pass runs");
    let pulse = prog.transactors[0].method("pulse").unwrap().function;
    let states = meta.state_enum.get(&pulse).expect("pulse tagged");
    assert!(states.len() >= 2, "wait creates a resume state: {states:?}");
}

/// A method param (or any local) that shadows the DUT field name is
/// host state â€” `dut.x` through it must NOT silently lower to a DUT
/// access.
///
/// The parenthetical here used to read "v1 surfaces the shadowing as a
/// C++ compile error". It does not. v1 ignores the shadowing and emits
/// `harc_rt::harc_assign(self.dut->en, 1)` â€” a write to the DUT PORT,
/// which compiles and runs (built and run: `dut.en=1`, parameter never
/// touched). That is what moved this arm to `SilentlyMisLowers`, and it
/// is asserted below rather than described.
#[test]
fn local_shadowing_dut_name_does_not_mislower() {
    let src = r#"
transactor Sh
    dut : Top

    when active
        hookable poke(dut: uint<8>)
            dut.en = 1
        end poke
    end when
end transactor Sh

testbench ShTb
    dut : Top
    sh  : Sh active
end testbench ShTb

impl ShTest for ShTb
    run
        sh.dut = dut
        sh.poke(1)
    end run
end impl ShTest
"#;
    let msg = assert_not_implemented(
        &lower_src(src).unwrap_err(),
        lower::V1Status::SilentlyMisLowers,
    );
    assert!(
        msg.contains("neither a DUT port nor a local"),
        "shadowed name must not resolve to the DUT: {msg}"
    );

    // The doc above used to say v1 "surfaces the shadowing as a C++
    // compile error". Measured, it does not: it ignores the shadowing
    // and writes to the DUT handle. Built and run outside the suite â€”
    // `dut.en=1`, with the `uint<8>` parameter never touched.
    let v1 = cpp_tb::emit(&merged_src(src)).expect("v1 emits the shadowed program");
    assert!(
        v1.contains("harc_rt::harc_assign(self.dut->en, 1);"),
        "v1 resolves the shadowed name to the DUT handle: {v1}"
    );

    // The other shape under the same arm, which is the uncompilable
    // one â€” both are why the arm carries the worse of the two.
    let non_place = r#"
testbench NpTb
    dut : Top
    n   : uint<32> default 0
end testbench NpTb

impl NpTest for NpTb
    run
        5 = n
        wait 1 cycle
    end run
end impl NpTest
"#;
    let msg = assert_not_implemented(
        &lower_src(non_place).unwrap_err(),
        lower::V1Status::SilentlyMisLowers,
    );
    assert!(msg.contains("neither a DUT port nor a local"), "{msg}");
    assert!(
        cpp_tb::emit(&merged_src(non_place))
            .expect("v1 emits")
            .contains("5 = _tb.n;"),
        "v1 emits the assignment to a non-place"
    );
}

// â”€â”€ Singleton-blocker batch (ternary, time/wide literals, const/enum,
//    test-scope lets, indexed lanes, testbench methods/fields, width
//    methods): one dump-ir snapshot per newly-registered fixture. â”€â”€â”€â”€

/// Ternary expressions inside CFG-inlined impure helpers, plus the
/// `WaitCyclesSync` terminator (v1's synchronous helper-lambda waits).
#[test]
fn linklist_basic_dump_ir_snapshot() {
    let prog = lower_src(&fixture("linklist_basic_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("linklist_basic_dump_ir", format!("{prog}"));
}

/// Wall-clock waits (`wait 80ns` â†’ `WaitTimePs`) and the `debug` log
/// severity, under the two-clock scheduler.
#[test]
fn async_fifo_dump_ir_snapshot() {
    let prog =
        lower_fixtures(&["async_fifo_test.harc", "async_fifo_domains.harc"]).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("async_fifo_dump_ir", format!("{prog}"));
}

/// 256-bit literals: `WideLiteral` word lists in DutWrite values and
/// `==`/`!=` assert conditions.
#[test]
fn wide_reg_dump_ir_snapshot() {
    let prog = lower_src(&fixture("wide_reg_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("wide_reg_dump_ir", format!("{prog}"));
}

/// Wide-value (>64-bit) method-param ABI: `load_block(key: uint<128>,
/// text_in: uint<128>)` lowers its params as `uint<128>` locals and the
/// body moves them to the 128-bit DUT ports. The `_harc_u128` C++ ABI is
/// asserted end-to-end by the registry equivalence harness.
#[test]
fn aes_cipher_top_dump_ir_snapshot() {
    let prog = lower_src(&fixture("aes_cipher_top_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    // The wide param survives as a 128-bit-typed local.
    let m = prog.transactors[0].method("load_block").unwrap();
    let f = prog.function(m.function);
    assert_eq!(f.params.len(), 2, "two wide params");
    assert_eq!(
        f.params[0].ty,
        ir::IrType::UInt(Some(128)),
        "first param lowers to uint<128>",
    );
    // `LocalId(i)` mirrors the i-th param (TB-IR convention).
    assert_eq!(
        f.locals[0].ty,
        ir::IrType::UInt(Some(128)),
        "param local is wide"
    );
    insta::assert_snapshot!("aes_cipher_top_dump_ir", format!("{prog}"));
}

/// 512-bit message-block literals + `while !dut.done` header re-reads.
#[test]
fn sha256_dump_ir_snapshot() {
    let prog = lower_src(&fixture("sha256_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("sha256_dump_ir", format!("{prog}"));
}

/// Test-scope `let`s hoisted to the head of the run function.
#[test]
fn if_wait_for_in_then_dump_ir_snapshot() {
    let prog = lower_src(&fixture("if_wait_for_in_then_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("if_wait_for_in_then_dump_ir", format!("{prog}"));
}

/// Constant-lane DUT port access (`dut.<port>[i]` reads and writes,
/// `PortRef::lane`) across packed and unpacked port shapes.
#[test]
fn packed_vec_lane_dump_ir_snapshot() {
    let prog = lower_src(&fixture("packed_vec_lane_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("packed_vec_lane_dump_ir", format!("{prog}"));
}

/// Testbench helper methods (`_tb.reset()` / `_tb.bump(n)`) CFG-
/// inlined into two `--test`-selectable tests.
#[test]
fn testbench_basic_dump_ir_snapshot() {
    let prog = lower_src(&fixture("testbench_basic_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("testbench_basic_dump_ir", format!("{prog}"));
}

/// Scalar testbench fields (`expected : uint<32> default 0`):
/// `TbFieldWrite` in run, `TbField` reads in the shared check phase.
#[test]
fn testbench_lifecycle_dump_ir_snapshot() {
    let prog = lower_src(&fixture("testbench_lifecycle_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("testbench_lifecycle_dump_ir", format!("{prog}"));
}

/// A classic-form test-scope `let` promoted for a check-phase read must
/// accept a bool literal default, matching ordinary `_tb` scalar fields.
#[test]
fn check_phase_promoted_bool_let_emits_bool_tb_field() {
    let src = r#"
domain SysDomain
  freq_mhz: 100
end domain SysDomain

test CheckPhaseBoolTest
    let dut : Top
    let seen : bool = false

    clock clk = SysDomain

    run
        seen = true
    end run

    check
        assert seen else fail("seen should persist")
    end check
end test CheckPhaseBoolTest
"#;
    let cpp = emit_cpp_src(src);
    assert!(
        cpp.contains("bool seen = false;"),
        "promoted bool let should emit a bool _tb field:\n{cpp}"
    );
    assert!(
        cpp.contains("_tb.seen = 1;"),
        "run-phase write should lower to a _tb field write:\n{cpp}"
    );
}

/// Transaction-typed testbench fields are shared host record state. Each
/// lowered run/check/helper body sees a synthetic record local for normal
/// `RecordFieldWrite`/`RecordField` lowering, but TBIR C++ declares the
/// object once at test scope so helper calls mutate one persistent value.
#[test]
fn testbench_transaction_field_lowers_shared_record_state() {
    let src = r#"
transaction Txn
    value : uint<8> default 0
    data : Vec<uint<8>, 2>
end transaction Txn

testbench Tb
    dut : Top
    cur : Txn

    function seed(v: uint<8>)
        cur.value = v
        cur.data[0] = v
        cur.data[1] = v + 1
    end seed

    function bump()
        _tb.cur.value = cur.value + 1
        _tb.cur.data[1] = _tb.cur.data[1] + 1
    end bump

    function mirror(t: Txn) -> uint<8>
        return t.data[1]
    end mirror

    function check_mirror()
        assert mirror(cur) == 8 else fail("bare cur data=${cur.data[1]}")
    end check_mirror
end testbench Tb

impl TbRecordFieldTest for Tb
    run
        seed(6)
        bump()
        assert _tb.cur.value == 7 else fail("value=${cur.value}")
        assert mirror(_tb.cur) == 8 else fail("data=${_tb.cur.data[1]}")
        check_mirror()
    end run
end impl TbRecordFieldTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let run = prog.function(prog.tests[0].run);
    let body = run
        .blocks
        .iter()
        .map(|b| format!("{b:?}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        body.contains("RecordInit") && body.contains("RecordFieldWrite"),
        "shared testbench record field should use ordinary record IR:\n{body}"
    );

    let cpp = emit_cpp_src(src);
    assert_eq!(
        cpp.matches("Txn cur{};").count(),
        1,
        "record field should be declared once at test scope:\n{cpp}"
    );
    assert!(
        cpp.contains("cur.value = v;") && cpp.contains("cur.value = (cur.value + 1);"),
        "helpers should mutate the shared record object:\n{cpp}"
    );
    assert!(
        cpp.contains("t = cur;") && cpp.contains("__t2 = t.data[1];"),
        "whole-record and indexed Vec reads should resolve through the shared record:\n{cpp}"
    );
}

#[test]
fn free_helper_does_not_capture_testbench_record_field() {
    let src = r#"
transaction Txn
    value : uint<8> default 0
end transaction Txn

function illegal_capture(dut: Top) -> uint<8>
    return cur.value
end function illegal_capture

testbench Tb
    dut : Top
    cur : Txn
end testbench Tb

impl TbRecordFreeHelperFenceTest for Tb
    run
        assert illegal_capture(dut) == 0 else fail("unexpected capture")
    end run
end impl TbRecordFreeHelperFenceTest
"#;
    let err = lower_src(src).expect_err("free helper must not capture testbench record field");
    // v1 has no rejection here: it passes `cur.value` through as C++
    // member syntax inside a free function that never declared `cur`.
    let msg = assert_not_implemented(&err, lower::V1Status::EmitsUncompilable);
    assert!(
        msg.contains("field access on a non-DUT value ending in `.value`"),
        "{msg}"
    );
}

#[test]
fn nested_free_helper_does_not_call_testbench_method_by_bare_name() {
    let src = r#"
transaction Txn
    value : uint<8> default 0
end transaction Txn

function illegal_call(dut: Top)
    seed()
end function illegal_call

testbench Tb
    dut : Top
    cur : Txn

    function seed()
        cur.value = 1
    end seed

    function call_helper()
        illegal_call(dut)
    end call_helper
end testbench Tb

impl TbNestedHelperFenceTest for Tb
    run
        call_helper()
    end run
end impl TbNestedHelperFenceTest
"#;
    let err = lower_src(src).expect_err("free helper must not see sibling testbench methods");
    let msg = assert_unsupported(&err);
    assert!(msg.contains("helper call `seed(...)`"), "{msg}");
}

#[test]
fn testbench_scalar_field_lowers_bare_captured_state() {
    let src = r#"
testbench Tb
    dut : Top
    count : uint<32> default 0

    function bump()
        count = count + 1
    end bump
end testbench Tb

impl TbScalarFieldTest for Tb
    run
        bump()
        assert count == 1 else fail("count=${count}")
    end run
end impl TbScalarFieldTest
"#;
    let cpp = emit_cpp_src(src);
    assert!(
        cpp.contains("_tb.count = (_tb.count + 1);") && cpp.contains("if (!((_tb.count == 1)))"),
        "bare scalar testbench field should lower through _tb host state:\n{cpp}"
    );
}

#[test]
fn free_helper_does_not_capture_testbench_scalar_field() {
    let src = r#"
function illegal_scalar_capture(dut: Top) -> uint<32>
    return count
end function illegal_scalar_capture

testbench Tb
    dut : Top
    count : uint<32> default 0
end testbench Tb

impl TbScalarFreeHelperFenceTest for Tb
    run
        assert illegal_scalar_capture(dut) == 0 else fail("unexpected capture")
    end run
end impl TbScalarFreeHelperFenceTest
"#;
    let err = lower_src(src).expect_err("free helper must not capture scalar testbench field");
    // v1 emits the bare identifier into a function that never declared
    // it (`return count;`), so there is no working backend to point at.
    let msg = assert_not_implemented(&err, lower::V1Status::EmitsUncompilable);
    assert!(msg.contains("unresolved name `count`"), "{msg}");
}

/// Shared record fields must not disturb hook/callback parameter local
/// ordering. If a parameter has the same source name as the shared field,
/// bare `cur`/`data` resolves to the parameter while explicit `_tb.cur` /
/// `_tb.data` resolves to the shared test-scope record.
#[test]
fn testbench_transaction_field_preserves_hook_parameter_order() {
    let src = r#"
transaction Txn
    value : uint<8> default 0
end transaction Txn

transactor Driver
    dut : Top
    when active
        hookable observe(cur: Txn)
            dut.en = cur.value
        end observe
    end when
end transactor Driver

testbench Tb
    dut : Top
    drv : Driver active
    cur : Txn
end testbench Tb

impl TbRecordHookShadowTest for Tb
    on drv.observe pre
        _tb.cur.value = cur.value + 1
    end on

    run
        drv.dut = dut
        _tb.cur.value = 3
        drv.observe(_tb.cur)
    end run
end impl TbRecordHookShadowTest
"#;
    let cpp = emit_cpp_src(src);
    assert!(
        cpp.contains("Driver_observe_pre = [&](Txn cur_2) -> void"),
        "hook parameter should remain first and be renamed away from shared cur:\n{cpp}"
    );
    assert!(
        cpp.contains("cur.value = (cur_2.value + 1);"),
        "_tb.cur should resolve to the shared record while bare cur resolves to the hook param:\n{cpp}"
    );
}

#[test]
fn testbench_transaction_field_preserves_reg_callback_parameter_order() {
    let src = r#"
transaction Txn
    value : uint<32> default 0
end transaction Txn

transactor H
    dut : Top
    when active
        hookable write(addr: uint<8>, data: uint<32>)
            dut.en = 1
        end write
        hookable read(addr: uint<8>) -> uint<32>
            return addr
        end read
    end when
end transactor H

regblock R via H width 32
    register A @ 0x10 access rw
end regblock R

testbench Tb
    dut : Top
    data : Txn
end testbench Tb

impl TbRecordRegCbShadowTest for Tb
    let h : H active
    let regs : R = bind h

    on regs.A
        _tb.data.value = data
    end on

    run
        h.dut = dut
        regs.record_write(0x10, 9)
    end run
end impl TbRecordRegCbShadowTest
"#;
    let cpp = emit_cpp_src(src);
    assert!(
        cpp.contains("TbRecordRegCbShadowTest_regs_A_cb = [&](uint64_t data_2) -> void"),
        "reg callback parameter should remain first and be renamed away from shared data:\n{cpp}"
    );
    assert!(
        cpp.contains("data.value = data_2;"),
        "_tb.data should resolve to shared record while bare data resolves to callback param:\n{cpp}"
    );
}

/// Width-method intrinsics (`.trunc/.zext/.sext/.resize`) with
/// receiver widths from typed lets, casts, and chained methods.
#[test]
fn width_methods_dump_ir_snapshot() {
    let prog = lower_src(&fixture("width_methods_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("width_methods_dump_ir", format!("{prog}"));
}

#[test]
fn wide_cast_dump_ir_snapshot() {
    let prog = lower_src(&fixture("wide_cast_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("wide_cast_dump_ir", format!("{prog}"));
}

#[test]
fn wide_zext_256_lowers_and_emits_harcwide() {
    let cpp = emit_cpp_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let small : uint<64> = 0xDEADBEEFCAFEF00D
        let first_wide : uint<129> = small.zext<129>()
        let wide : uint<256> = small.zext<256>()
        let ceiling : uint<1024> = wide.zext<1024>()
        assert first_wide == 0xDEADBEEFCAFEF00D else fail("first wide zext")
        assert wide == 0xDEADBEEFCAFEF00D else fail("wide zext")
        assert ceiling.trunc<64>() == small else fail("ceiling zext")
    end run
end impl T"#,
    );
    assert!(
        cpp.contains("harc_rt::HarcWide<5> first_wide")
            && cpp.contains("harc_rt::HarcWide<8> wide")
            && cpp.contains("harc_rt::HarcWide<32> ceiling")
            && cpp.contains("harc_rt::harc_wide_zext<8>")
            && cpp.contains("harc_rt::harc_wide_zext<32>"),
        "expected 129/256/1024-bit zext representations:\n{cpp}"
    );
}

#[test]
fn wide_trunc_130_lowers_and_emits_masked_harcwide() {
    let cpp = emit_cpp_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let source : uint<256> = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF
        let low : uint<130> = source.trunc<130>()
        assert low == 0x3FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF else fail("wide trunc")
    end run
end impl T"#,
    );
    assert!(
        cpp.contains("harc_rt::HarcWide<5> low")
            && cpp.contains("harc_rt::harc_wide_trunc<5>")
            && cpp.contains(", 130)"),
        "expected trunc<130> to mask a HarcWide<5>:\n{cpp}"
    );
}

#[test]
fn chained_wide_sext_uses_harcwide_source_and_destination() {
    let cpp = emit_cpp_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let negative : uint<9> = 0x100
        let sign130 : sint<130> = negative.sext<130>()
        let sign256 : sint<256> = sign130.sext<256>()
        assert sign256.trunc<9>() == 0x100 else fail("wide sext chain")
    end run
end impl T"#,
    );
    assert!(
        cpp.contains("harc_rt::harc_wide_sext<5>")
            && cpp.contains("harc_rt::harc_wide_sext<8>")
            && cpp.contains("sign130, 130, 256"),
        "expected chained wide sign extension:\n{cpp}"
    );
}

#[test]
fn wide_resize_selects_zero_extension_or_truncation() {
    let cpp = emit_cpp_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let small : uint<64> = 0x123456789ABCDEF0
        let wide : uint<256> = small.resize<256>()
        let low : uint<130> = wide.resize<130>()
        assert low.trunc<64>() == small else fail("wide resize")
    end run
end impl T"#,
    );
    assert!(
        cpp.contains("harc_rt::harc_wide_zext<8>")
            && cpp.contains("harc_rt::harc_wide_trunc<5>")
            && cpp.contains(", 130)"),
        "expected wide resize to select zext/trunc helpers:\n{cpp}"
    );
}

#[test]
fn tbir_rejects_width_method_above_language_limit() {
    let err = lower_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let value : uint<1025> = (1 as uint<64>).zext<1025>()
        log(info, "${value}")
    end run
end impl T"#,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("zext<1025>") && msg.contains("1024-bit language limit"),
        "expected language-limit diagnostic, got: {msg}"
    );
}

#[test]
fn verifier_rejects_malformed_width_cast_bounds() {
    let source = r#"test T
    let dut : Top
    run
        let source : uint<64> = 1
        let wide : uint<128> = source.zext<128>()
    end run
end test T"#;

    for invalid in [0, 1025] {
        let mut prog = lower_src(source).expect("lowers");
        let ir::Stmt::Assign(_, ir::Expr::WidthCast { width, .. }) =
            &mut prog.functions[0].blocks[0].stmts[1]
        else {
            panic!("expected width-cast assignment");
        };
        *width = invalid;
        let errs = verify::verify_program(&prog).unwrap_err();
        assert!(errs.iter().any(|e| matches!(
            e,
            verify::VerifyError::BadWidthCast { width, .. } if *width == invalid
        )));
    }

    // A zero-width source is malformed: lowering reports an unusable
    // declared width as `None`, never `Some(0)`.
    let mut prog = lower_src(source).expect("lowers");
    let ir::Stmt::Assign(
        _,
        ir::Expr::WidthCast {
            src_width: Some(src_width),
            ..
        },
    ) = &mut prog.functions[0].blocks[0].stmts[1]
    else {
        panic!("expected width-cast assignment with a known source width");
    };
    *src_width = 0;
    let errs = verify::verify_program(&prog).unwrap_err();
    assert!(errs.iter().any(|e| matches!(
        e,
        verify::VerifyError::BadWidthCast {
            src_width: Some(0),
            ..
        }
    )));
}

/// `src_width` is best-effort *receiver* metadata, not a cast destination:
/// declared widths are not bounded by `MAX_WIDTH_METHOD_BITS`, so a legal
/// narrowing out of an oversized declared type must survive verification
/// rather than trip `BadWidthCast` after a clean `harc check`.
#[test]
fn verifier_accepts_source_width_past_the_width_method_limit() {
    let source = r#"test T
    let dut : Top
    run
        let big : uint<2048> = 0
        let narrow : uint<64> = big.trunc<64>()
    end run
end test T"#;

    let prog = lower_src(source).expect("lowers");
    let ir::Stmt::Assign(
        _,
        ir::Expr::WidthCast {
            src_width: Some(2048),
            width: 64,
            ..
        },
    ) = &prog.functions[0].blocks[0].stmts[1]
    else {
        panic!("expected a 64-bit cast carrying the 2048-bit source width");
    };
    verify::verify_program(&prog).expect("oversized declared source width verifies");
}

/// A zero-width declared type carries no usable source metadata, so
/// lowering records `None` â€” feeding `0` into the sext shift-fill would
/// emit a `64 - 0` shift (UB) and, before that, fail verification on a
/// program `harc check` accepts.
#[test]
fn zero_width_receiver_lowers_to_an_unknown_source_width() {
    let source = r#"test T
    let dut : Top
    run
        let z : uint<0> = 0
        let w = z.sext<64>()
    end run
end test T"#;

    let prog = lower_src(source).expect("lowers");
    let ir::Stmt::Assign(
        _,
        ir::Expr::WidthCast {
            src_width: None, ..
        },
    ) = &prog.functions[0].blocks[0].stmts[1]
    else {
        panic!("expected a width cast with no source width");
    };
    verify::verify_program(&prog).expect("zero-width receiver verifies");
}

/// Dropping `Some(0)` is scoped to the `src_width` *emission* metadata.
/// The width-method direction check still sees the raw inferred width, so
/// a zero-width receiver remains a wrong-direction `.trunc<N>()` with a
/// user-facing diagnostic rather than silently lowering.
#[test]
fn zero_width_receiver_still_fails_the_trunc_direction_check() {
    let err = lower_src(
        r#"test T
    let dut : Top
    run
        let z : uint<0> = 0
        let t = z.trunc<8>()
    end run
end test T"#,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("trunc<8>") && msg.contains("0-bit value"),
        "expected a wrong-direction diagnostic, got: {msg}"
    );
}

/// The wrapping-operator mask reads the same inference helper, and there a
/// zero width is a usable operand width (`max(0, 1)` â†’ a 1-bit mask), not
/// metadata to discard. Lowering must not degrade to the "operand width
/// unknown" hard error.
#[test]
fn zero_width_wrapping_operand_keeps_its_inferred_width() {
    let prog = lower_src(
        r#"test T
    let dut : Top
    run
        let z : uint<0> = 0
        let y = z +% 1
    end run
end test T"#,
    )
    .expect("zero-width wrapping operand lowers");
    let ir::Stmt::Assign(_, ir::Expr::WidthCast { width: 1, .. }) =
        &prog.functions[0].blocks[0].stmts[1]
    else {
        panic!("expected the wrap mask to size to 1 bit");
    };
    verify::verify_program(&prog).expect("verifies");
}

#[test]
fn bit_not_masks_to_fixed_uint_width() {
    let cpp = emit_cpp_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let v : uint<32> = 0xffffffff
        let n = ~v
        log(info, "${n:08x}")
    end run
end impl T"#,
    );
    assert!(
        cpp.contains("~(v)") && cpp.contains("0xFFFFFFFFULL"),
        "expected uint<32> bit-not to be masked to 32 bits; got:\n{cpp}",
    );
}

// â”€â”€ regblock construct (register-level frontdoor subset) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// The regblock subset fixture lowers cleanly: a `regblock` declaration
/// becomes a synthetic mirror record (one scalar field per register,
/// defaulting to its reset value) plus a `RegblockSchema`; the
/// test-scope unbound-transactor `let h : RegHelper active` registers as
/// a transactor instance; and register-level access lowers to mirror
/// `RecordFieldWrite` / reads plus `Helper.write`/`read`
/// `TransactorCall` edges. Snapshotted end-to-end.
#[test]
fn regblock_subset_dump_ir_snapshot() {
    let prog = lower_src(&fixture("regblock_subset_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("regblock_subset_dump_ir", format!("{prog}"));
}

/// Register-level write/read lowering shapes. A RW write emits the
/// mirror `RecordFieldWrite` then the helper `write` call edge; a RW
/// read emits the helper `read` call edge into the destination local
/// then a mirror-predict `RecordFieldWrite`.
#[test]
fn regblock_rw_write_then_read_lowers_to_mirror_plus_call_edge() {
    let src = r#"
transactor H
    dut : Top
    when active
        hookable write(addr: uint<8>, data: uint<32>)
            dut.en = 1
        end write
        hookable read(addr: uint<8>) -> uint<32>
            return addr
        end read
    end when
end transactor H
regblock R via H width 32
    register A @ 0x10 access rw
end regblock R
testbench Tb
    dut : Top
end testbench Tb
impl Test for Tb
    let h : H active
    let regs : R = bind h
    run
        h.dut = dut
        regs.A = 5
        let v = regs.A
    end run
end impl Test
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let run = prog.function(prog.tests[0].run);
    let body = run
        .blocks
        .iter()
        .map(|b| format!("{b:?}"))
        .collect::<Vec<_>>()
        .join("\n");
    // Mirror init at entry, write = mirror + call edge, read = call edge
    // + mirror predict.
    assert!(body.contains("RecordInit"), "mirror init missing:\n{body}");
    assert!(
        body.matches("RecordFieldWrite").count() >= 2,
        "expected a write-side mirror update and a read-side predict:\n{body}"
    );
    assert!(
        body.matches("TransactorMethod").count() >= 2,
        "expected write + read frontdoor call edges:\n{body}"
    );
}

/// RO write suppresses the bus traffic (mirror update only); WO read
/// serves from the mirror (no bus traffic).
#[test]
fn regblock_ro_write_and_wo_read_skip_the_bus() {
    let src = r#"
transactor H
    dut : Top
    when active
        hookable write(addr: uint<8>, data: uint<32>)
            dut.en = 1
        end write
        hookable read(addr: uint<8>) -> uint<32>
            return addr
        end read
    end when
end transactor H
regblock R via H width 32
    register RO @ 0x00 access ro
    register WO @ 0x04 access wo
end regblock R
testbench Tb
    dut : Top
end testbench Tb
impl Test for Tb
    let h : H active
    let regs : R = bind h
    run
        h.dut = dut
        regs.RO = 1
        let w = regs.WO
    end run
end impl Test
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let run = prog.function(prog.tests[0].run);
    let body = run
        .blocks
        .iter()
        .map(|b| format!("{b:?}"))
        .collect::<Vec<_>>()
        .join("\n");
    // RO write + WO read make NO frontdoor call edges (both stay local).
    assert!(
        !body.contains("TransactorMethod"),
        "RO write and WO read must not reach the bus:\n{body}"
    );
}

/// Field-level access (`regs.REG.FIELD`) lowers to a masked read-modify-
/// write on the whole-register mirror cell plus a full-register bus
/// write â€” v1's bit-slice insert. The mirror cell stays whole-register
/// (one `RecordFieldWrite` of the new word); a bus-writing field then
/// fires `H.write(off, mirror.REG)`.
#[test]
fn regblock_field_level_write_lowers_to_masked_rmw_plus_bus_write() {
    let src = r#"
transactor H
    dut : Top
    when active
        hookable write(addr: uint<8>, data: uint<32>)
            dut.en = 1
        end write
        hookable read(addr: uint<8>) -> uint<32>
            return addr
        end read
    end when
end transactor H
regblock R via H width 32
    register A @ 0x00 access rw
        field F : bit @ 0 access rw
        field G : uint<3> @ 4 access rw
    end register A
end regblock R
testbench Tb
    dut : Top
end testbench Tb
impl Test for Tb
    let h : H active
    let regs : R = bind h
    run
        h.dut = dut
        regs.A.F = 1
        regs.A.G = 5
        assert regs.A.G == 5 else fail("g")
    end run
end impl Test
"#;
    let prog = lower_src(src).expect("field-level lowers");
    let dump = format!("{prog}");
    // The masked RMW writes the whole-register mirror field `A`, then the
    // bus write carries the updated whole-register word â€” never the
    // field. A shifted-extract read appears in the assert condition.
    assert!(
        dump.contains("RecordFieldWrite(%regs.A"),
        "expected a whole-register mirror RecordFieldWrite of `A`: {dump}"
    );
    assert!(
        dump.contains("RegRead"),
        "expected a field read to compose over an inline RegRead: {dump}"
    );
}

/// An RO field's write updates the mirror but suppresses the bus write
/// (v1's `ro` semantics); a WO field's read serves from the mirror cell
/// without bus traffic.
#[test]
fn regblock_field_level_ro_wo_policies() {
    let src = r#"
transactor H
    dut : Top
    when active
        hookable write(addr: uint<8>, data: uint<32>)
            dut.en = 1
        end write
        hookable read(addr: uint<8>) -> uint<32>
            return addr
        end read
    end when
end transactor H
regblock R via H width 32
    register A @ 0x00 access rw
        field RO : bit @ 0 access ro
        field WO : bit @ 1 access wo
    end register A
end regblock R
testbench Tb
    dut : Top
end testbench Tb
impl Test for Tb
    let h : H active
    let regs : R = bind h
    run
        h.dut = dut
        regs.A.RO = 1
        let w = regs.A.WO
    end run
end impl Test
"#;
    let prog = lower_src(src).expect("ro/wo fields lower");
    let dump = format!("{prog}");
    // The RO write still updates the mirror (RecordFieldWrite of A).
    assert!(
        dump.contains("RecordFieldWrite(%regs.A"),
        "expected a mirror RecordFieldWrite even for the RO field: {dump}"
    );
    // The WO read must NOT issue a bus read â€” it serves from the mirror
    // cell (a shifted RecordField extract), so no RegRead is emitted.
    assert!(
        !dump.contains("RegRead"),
        "WO field read must serve from the mirror, not the bus: {dump}"
    );
}

/// A register read outside `let`-RHS position (here an assert condition)
/// now lowers to an `Expr::RegRead` â€” v1's inline assignment-expression
/// (`(regs.A = H_read(off))`), which fires exactly one bus read per
/// textual occurrence. The `via` helper's `read` is a plain hookable
/// lambda (not the TLM seam), so it is a legitimate sub-expression value.
#[test]
fn regblock_read_in_assert_lowers_to_regread() {
    let src = r#"
transactor H
    dut : Top
    when active
        hookable write(addr: uint<8>, data: uint<32>)
            dut.en = 1
        end write
        hookable read(addr: uint<8>) -> uint<32>
            return addr
        end read
    end when
end transactor H
regblock R via H width 32
    register A @ 0x00 access rw
end regblock R
testbench Tb
    dut : Top
end testbench Tb
impl Test for Tb
    let h : H active
    let regs : R = bind h
    run
        h.dut = dut
        assert regs.A == 1 else fail("x")
    end run
end impl Test
"#;
    let prog = lower_src(src).expect("read-in-assert lowers");
    verify::verify_program(&prog).expect("verifies");
    // The run function's AssertCheck condition must carry a RegRead
    // that reads the bus and predicts the mirror.
    let run = prog
        .functions
        .iter()
        .find(|f| matches!(f.kind, ir::FunctionKind::Run))
        .expect("run function");
    let has_regread = run.blocks.iter().any(|b| {
        b.stmts.iter().any(|s| {
            if let ir::Stmt::AssertCheck { cond, .. } = s {
                fn contains_regread(e: &ir::Expr) -> bool {
                    match e {
                        ir::Expr::RegRead { reads_bus, .. } => *reads_bus,
                        ir::Expr::Binary(_, a, b) => contains_regread(a) || contains_regread(b),
                        ir::Expr::Unary(_, a) => contains_regread(a),
                        _ => false,
                    }
                }
                contains_regread(cond)
            } else {
                false
            }
        })
    });
    assert!(
        has_regread,
        "expected a bus-reading RegRead in the assert condition"
    );
}

/// The corpus `regblock_basic_test` fixture â€” initiator-side BFM `via`
/// helper PLUS register reads in assert conditions and `${...}` format
/// args (`assert (regs.DMACR & 1) == 1 else fail("...0x${regs.DMACR}")`)
/// â€” now FULLY lowers (this slice). Register reads outside `let`-RHS
/// lower to `Expr::RegRead` (v1's inline assignment-expression), so the
/// fixture's last regblock residual (divergence 12) is closed.
#[test]
fn regblock_basic_corpus_lowers_with_register_read_in_assert() {
    let prog =
        lower_with_stdlib_bus("regblock_basic_test.harc", "BusAxiLite.arch").expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    // Both an assert condition AND a fail-message format arg carry a
    // bus-reading RegRead (eager in the cond, lazy in the fail branch).
    let run = prog
        .functions
        .iter()
        .find(|f| matches!(f.kind, ir::FunctionKind::Run))
        .expect("run function");
    let mut cond_reads = 0usize;
    let mut fail_arg_reads = 0usize;
    fn is_bus_regread(e: &ir::Expr) -> bool {
        match e {
            ir::Expr::RegRead { reads_bus, .. } => *reads_bus,
            ir::Expr::Binary(_, a, b) => is_bus_regread(a) || is_bus_regread(b),
            ir::Expr::Unary(_, a) => is_bus_regread(a),
            _ => false,
        }
    }
    for b in &run.blocks {
        for s in &b.stmts {
            if let ir::Stmt::AssertCheck { cond, on_fail } = s {
                if is_bus_regread(cond) {
                    cond_reads += 1;
                }
                if on_fail.args.iter().any(|a| is_bus_regread(&a.expr)) {
                    fail_arg_reads += 1;
                }
            }
        }
    }
    assert!(
        cond_reads >= 3,
        "expected â‰¥3 assert-cond RegReads, got {cond_reads}"
    );
    assert!(
        fail_arg_reads >= 3,
        "expected â‰¥3 fail-message RegReads, got {fail_arg_reads}"
    );
}

/// The corpus `regblock_access_test` fixture â€” same initiator-side BFM
/// `via` helper, but every register read sits in `let`-RHS position
/// (`let v = regs.MM2S_LEN`) â€” FULLY lowers with this slice: the BFM
/// helper's `hookable write/read` bodies drive the bound AXI-Lite bus
/// channels and the regblock frontdoor's `Helper.write`/`read` call
/// edges resolve. (The end-to-end v1â†”tbir trace equivalence is gated by
/// the registry harness; this asserts lowering succeeds.)
#[test]
fn regblock_access_corpus_lowers_with_initiator_bfm() {
    let prog =
        lower_with_stdlib_bus("regblock_access_test.harc", "BusAxiLite.arch").expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    // The BFM helper is a bound-to initiator transactor with write+read.
    let helper = prog
        .transactors
        .iter()
        .find(|x| x.name == "AxilHelper")
        .expect("AxilHelper transactor lowered");
    assert_eq!(helper.bound_bus.as_deref(), Some("BusAxiLite"));
    assert!(helper.method("write").is_some() && helper.method("read").is_some());
}

/// The bound-to INITIATOR BFM now carries persistent scalar STATE fields
/// (this slice): `AxilHelper bound to BusAxiLite` with a `read` method
/// that caches `last_read`/`read_count` across calls. The schema records
/// the state fields, the method body's bare-name writes lower to
/// `TransactorStateWrite` instance-filled with the bound instance name,
/// and the testbench records the instance in `unbound_state_actors` so
/// emission materializes one per-instance state struct (sha×nwëÆòµë(š+my×vW%öf×F&R×'6W2V6‚6GW&R27FæFÆöæRg&vÖVçBv†÷6P¢òòò7ç2&Rg&vÖVçB×&VÆF—fRÂ6ò6GW&Rw27â6â6öÆÆ–FRv—F‚¢òòò&VÂFV×÷&Âö67W'&Væ6Rw2æBvWB&Ww&—GFVâ–çFòF†Bö67W'&Væ6Rw0¢òòòW‡#£¥FV×÷&Å6Æ÷Fâv—F‚F†RÖV×G’ÂG·7B‡‚—Ö&V6†W2F†P¢òòò÷&F–æ'’FV×÷&ÂvFRæB—2&V¦V7FVB'’æÖRà¢5·FW7EÐ¦fâö6†V6µöÖW76vUö6ææ÷E÷&VEö÷FV×÷&Å÷6Æ÷B‚’°¢ÆWBW'"ÒÆ÷vW%÷7&2€¢"2'FW7F&Væ6‚F ¢GWB¢F÷ ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢76W'B7B†GWBæ’ÓÒVÇ6Rf–Â‚'v2G·7B†GWBæ—Ò"¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"2À¢¢çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fW'"“°¢76W'B€¢×6ræ6öçF–ç2‚&æ÷B–âF†B6†V6²w2VÇ6Rf–Â‚âââ–ÖW76vR"’À¢'¶×6wÒ ¢“° ¢òòF†R7âÖ6öÆÆ—6–öâ—G6VÆc¢ÖW76vRv†÷6R6GW&R—2Æ–à¢òò÷'B&VB×W7B7F’÷'B&VBÂæWfW"ÆF6‚âF†—2—2F†P¢òò6†RF†B6–ÆVçFÇ’VÖ—GFVBö†&5÷3&Vf÷&RF†RÖv0¢òò6ÆV&VBà¢ÆWB&örÒÆ÷vW%÷7&2€¢"2'FW7F&Væ6‚F ¢GWB¢F÷ ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢76W'B7B†GWBæ’ÓÒVÇ6Rf–Â‚&#ÒG¶GWBæ'Ò"¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"2À¢¢æW‡V7B‚&Æ÷vW'2"“°¢ÆWB×6rÒ&örç&÷W'G•ö6†V6·5³Ð¢æÖW76vP¢æ5÷&Vb‚¢æW‡V7B‚'F†R6ÆW6RÆ÷vW&VB"“°¢76W'B€¢×6ræ&w0¢æ—FW"‚¢æÆÂ‡ÆÂÖF6†W2†æW‡"Â—#£¤W‡#£¥FV×÷&Å6Æ÷B²ââÒ’’À¢&ÖW76vR6GW&R×W7BæWfW"Æ–2ÆF6‚6Æ÷C¢³£÷Ò"À¢×6ræ&w0¢“°¢òò(
fæBF†RVÖ—GFVB6Æ÷7W&R&VG2F†R÷'BÂæ÷BF†RÆF6‚à¢ÆWB7ÒVÖ—Eö7÷7&2€¢"2'FW7F&Væ6‚F ¢GWB¢F÷ ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢76W'B7B†GWBæ’ÓÒVÇ6Rf–Â‚&#ÒG¶GWBæ'Ò"¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"2À¢“°¢76W'B€¢7æ6öçF–ç2€¢"2'6–ÕöÆöuöÆ–æR‚$d”Â"Â&#ÒVÆÆB"Â†&5÷'C£¦†&5÷&–çFeöÆÂ††&5÷'C£¦†&5÷&VB†GWBÓæ"’’’"0¢’À¢'¶7Ò ¢“°§Ð ¢òòÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÐ¢òò&F6‚C¢f÷"‚–âÇ&V3âãÇfV6f–VÆCæÂF—66&FVBVWVR÷2ÂæBF†P¢òòF–væ÷7F–72&÷VæBF†VÒà¢òòÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÐ ¢òòòf÷"‚–âÇ&V3âãÇfV6f–VÆCæ—FW&FW2fV3ÅBÂãæ&V6÷&Bf–VÆBâc¢òòòVÖ—G2f÷"†WFòb‚¢&V2æFF–÷fW"F†R7FC£¦'&–²F&—"Æ÷vW'0¢òòò—BFò6÷VçFVBÆö÷÷fW"F†R66†VÖÖ6öç7FçBÆVæwF‚v—F‚F†RÆö÷ ¢òòòf&–&ÆR&÷VæBFòÆf–VÆCå¶•Öâ&V6÷&E÷fV5öf–VÆEö—FW%÷FW7F—2F†P¢òòòWV—fÆVæ6Rf—‡GW&S²F†—2–ç2F†RVÖ—GFVB6†Rà¢5·FW7EÐ¦fâf÷%ö–åö÷&V6÷&E÷fV5öf–VÆEöÆ÷vW'5÷Fõöö6÷VçFVEöÆö÷‚’°¢ÆWB7ÒVÖ—Eö7÷7&2€¢"2'7G'V7B'VæFÆP¢FF¢fV3ÇV–çCÃ3#âÂCà¦VæB7G'V7B'VæFÆP §FW7F&Væ6‚F ¢GWB¢F÷ ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢ÆWB"¢'VæFÆP¢ÆWB7VÒÒ ¢f÷"‚–â"æFF¢7VÒÒ7VÒ²€¢VæBf÷ ¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"2À¢“°¢òòF†R&÷VæB—2F†R66†VÖÆVæwF‚Âæ÷B'VçF–ÖR6—¦R‚–6ÆÂà¢76W'B€¢7æ6öçF–ç2‚#ÂB’"’À¢&†VFW"6÷VçG2FòF†RfV2ÆVæwFƒ¥Æç¶7Ò ¢“°¢76W'B€¢7æ6öçF–ç2‚'‚Ò"æFF²"’bb7æ6öçF–ç2‚'7VÒÒ‡7VÒ²‚’"’À¢&V6‚—FW&F–öâ&–æG2F†RVÆVÖVçBÂF†Vâ'Vç2F†R&öG“¥Æç¶7Ò ¢“°§Ð ¢òòòæW7FVBF‚†æ"ãÇfV6f–VÆCæ’&V6†W2F†R6ÖRÆö÷à¢5·FW7EÐ¦fâf÷%ö–åööæW7FVE÷&V6÷&E÷fV5öf–VÆEöÆ÷vW'2‚’°¢ÆWB7ÒVÖ—Eö7÷7&2€¢"2'7G'V7B–ææW ¢FF¢fV3ÇV–çCÃ3#âÂ3à¦VæB7G'V7B–ææW §7G'V7B÷WFW ¢–ææW"¢–ææW ¦VæB7G'V7B÷WFW  §FW7F&Væ6‚F ¢GWB¢F÷ ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢ÆWBò¢÷WFW ¢ÆWB7VÒÒ ¢f÷"’–âòæ–ææW"æFF¢7VÒÒ7VÒ²¢VæBf÷ ¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"2À¢“°¢76W'B†7æ6öçF–ç2‚#Â2’"’Â'¶7Ò"“°¢76W'B†7æ6öçF–ç2‚'’Òòæ–ææW"æFF²"’Â'¶7Ò"“°§Ð ¢òòòF†RÆö÷f&–&ÆR—24õ’öbF†RVÆVÖVçB(	Bc&–æG2WFòb†À¢òòòv†W&Rw&—FRÆæG2&6²–âF†R6öçF–æW"ÂæBF†R•"†2æð¢òòò'’×&VfW&Væ6RÆö6Ââ&F†W"F†âG&÷7V6‚w&—FR6–ÆVçFÇ’ÂÆ÷vW&–æp¢òòò&V¦V7G2—BÂf÷"f÷"B–âÇG6W×&W7VÇCæ2vVÆÂ2F†RæWrfV2f÷&Òà¢5·FW7EÐ¦fâ÷w&—FU÷Fõööf÷%öÆö÷öVÆVÖVçE÷f&–&ÆUö—5÷&V¦V7FVB‚’°¢ÆWBW'"ÒÆ÷vW%÷7&2€¢"2'7G'V7B'VæFÆP¢FF¢fV3ÇV–çCÃ3#âÂCà¦VæB7G'V7B'VæFÆP §FW7F&Væ6‚F ¢GWB¢F÷ ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢ÆWB"¢'VæFÆP¢f÷"‚–â"æFF¢‚Òp¢VæBf÷ ¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"2À¢¢çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2“°¢76W'B€¢×6ræ6öçF–ç2‚&w&—FRFòf÷&Æö÷w2VÆVÖVçBf&–&ÆR"’À¢'¶×6wÒ ¢“° ¢òòw&—FRæW7FVB–ç6–FRF†R&öG’w26öçG&öÂfÆ÷r—26Vv‡BFöò(	BF†P¢òò66â6÷fW'2WfW'’&Æö6²F†R&öG’÷VæVBÂæ÷B§W7B—G2VçG'’à¢ÆWBW'"ÒÆ÷vW%÷7&2€¢"2'7G'V7B'VæFÆP¢FF¢fV3ÇV–çCÃ3#âÂCà¦VæB7G'V7B'VæFÆP §FW7F&Væ6‚F ¢GWB¢F÷ ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢ÆWB"¢'VæFÆP¢f÷"‚–â"æFF¢–b‚ÓÒ¢‚Òp¢VæB–`¢VæBf÷ ¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"2À¢¢çVçw&öW'"‚“°¢76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2“° ¢òò&VF–ær—BÂæBw&—F–ærâTå$TÄDTBÆö6ÂÂ&÷F‚7F’ÆVvÂà¢ÆWB7ÒVÖ—Eö7÷7&2€¢"2'7G'V7B'VæFÆP¢FF¢fV3ÇV–çCÃ3#âÂCà¦VæB7G'V7B'VæFÆP §FW7F&Væ6‚F ¢GWB¢F÷ ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢ÆWB"¢'VæFÆP¢ÆWB7VÒÒ ¢f÷"‚–â"æFF¢–b‚ÓÒ¢7VÒÒ7VÒ²€¢VæB–`¢VæBf÷ ¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"2À¢“°¢76W'B†7æ6öçF–ç2‚'7VÒÒ‡7VÒ²‚’"’Â'¶7Ò"“°§Ð ¢òòò66Æ"&V6÷&Bf–VÆB—2æ÷B—FW&&ÆR–âV—F†W"&6¶VæC¢cVÖ—G0¢òòòf÷"†WFòb‚¢&V2çb–÷fW"V–çCcE÷FÂv†–6‚†2æð¢òòò&Vv–æöVæFà¢5·FW7EÐ¦fâf÷%ö–åö÷66Æ%÷&V6÷&Eöf–VÆEöFöW5öæ÷E÷ö–çEöE÷c‚’°¢ÆWBW'"ÒÆ÷vW%÷7&2€¢"2'7G'V7B ¢b¢V–çCÃƒà¦VæB7G'V7B  §FW7F&Væ6‚F ¢GWB¢F÷ ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢ÆWB"¢ ¢f÷"‚–â"ç`¢Æör†–æfòÂ'‚"¢VæBf÷ ¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"2À¢¢çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¤VÖ—G5Væ6ö×–Æ&ÆR“°¢76W'B†×6ræ6öçF–ç2‚&÷fW"66Æ"&V6÷&Bf–VÆB"’Â'¶×6wÒ"“°§Ð ¢òòò&&Rç÷‚––â7FFVÖVçB÷6—F–öâF—66&G2—G2fÇVR'WB×W7@¢òòò7F–ÆÂ%Tâ(	BF†R×WFF–öâ—2F†Rö–çBöbw&—F–ær—BF†Bv’âc¢òòòVÖ—G2÷F"çç÷‚“¶²F&—"æ÷r÷2–çFòFV×æ÷F†–ær&VG2à¢5·FW7EÐ¦fâöF—66&FVE÷VWVU÷÷÷7F–ÆÅ÷÷2‚’°¢ÆWB7ÒVÖ—Eö7÷7&2€¢"2'FW7F&Væ6‚F ¢GWB¢F÷ ¢¢VWVSÇV–çCÃƒãà¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢çW6‚ƒ¢çW6‚ƒ"¢ç÷‚¢ÆWBbÒç÷‚¢76W'BbÓÒ"VÇ6Rf–Â‚&F—66&FVB÷×W7BF¶RF†Rg&öçB"¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"2À¢“°¢ÆWB÷2Ò7æÖF6†W2‚%÷F"çç÷‚’"’æ6÷VçB‚“°¢76W'EöW‡÷2Â"Â&&÷F‚÷2&RVÖ—GFVC¥Æç¶7Ò"“°§Ð ¢òòòF†RF—66&FVB÷—2dÔ”Å“¢FW7F&Væ6‚Â66÷&V&ö&BÂ6ö×öæVçBÀ¢òòò&&RF&vWB×&W7öæFW"7FFRÂæB–ç7Fæ6R×VÆ–f–VBF&vWB7FFRÆÀ¢òòò6''’F†R6ÖR'VÆRâf—†–æröæRfÆf÷"æBÆVf–ærF†R&W7B6––æp¢òòò&&–æBF†R÷VBfÇVR"—2F†Rf–ÇW&RÖöFRF†—2–ç2(	BÖ÷7BæVVB¢òòòF–ffW&VçFÇ’×6†VBf—‡GW&RFò&V6‚Â6òF†RfÖ–Ç’—26†V6¶V@¢òòò7G'V7GW&ÆÇ’÷fW"F†RÆ÷vW&–ær6÷W&6W2Âv—F‚F†R66÷&V&ö&B&Ð¢òòòW†W&6—6VBVæB×FòÖVæB&VÆ÷r6òF†R66â6ææ÷B72÷fW"FVB6öFRà¢5·FW7EÐ¦fâWfW'•÷VWVUöfÆf÷%öÆ÷vW'5ööF—66&FVE÷÷‚’°¢ÆWB7&2Ò7FC£¦g3£§&VE÷Fõ÷7G&–ær€¢7FC£§Fƒ£¥Fƒ£¦æWr†Vçb‚$4$tõôÔä”dU5EôD•""’’æ¦ö–â‚'7&2ö—"öÆ÷vW"÷7F×G2ç'2"’À¢¢æW‡V7B‚'&VBF†R7FFVÖVçBÆ÷vW&–ær"“°¢76W'B€¢7&2æ6öçF–ç2‚&F—66&FVB"’À¢&F—66&FVBâââ÷‚–&V¦V7F–öâ—2ÆVgB–âF†R7FFVÖVçBÆ÷vW&–æs²WfW'’À¢fÆf÷"÷2–çFòâVç&VBFV×æ÷r‡6VRF—66&E÷6Æ÷F’ ¢“°¢òòöæR&ÒVæB×FòÖVæC¢66÷&V&ö&BVWVR&V6†VBF‡&÷Vv‚¢òòFW7F&Væ6‚f–VÆBà¢ÆWB7ÒVÖ—Eö7÷7&2€¢"2'66÷&V&ö&B6 ¢6VVâ¢VWVSÇV–çCÃƒãà¦VæB66÷&V&ö&B6  §FW7F&Væ6‚F ¢GWB¢F÷ ¢6"¢6 ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢6"ç6VVâçW6‚ƒ¢6"ç6VVâç÷‚¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"2À¢“°¢76W'B†7æ6öçF–ç2‚"ç6VVâç÷‚’"’Â'¶7Ò"“°§Ð ¢òòòVWVRÖWF†öBF†B—2æ÷BW6†ö÷ö6—¦VöV×G–—2æ÷Bc¢òòòW66R†F6‚ÂæB–âU…$U54”ôâ÷6—F–öâ—B—2æ÷B7V'6WBv ¢òòòV—F†W#¢6—¦VöV×G–Æ÷vW"æB÷†2—G2÷vâ&ÒÂ6òv†@¢òòò&V6†W2F†RfÆÆ&6²—2W6†‡v†–6‚&WGW&ç2fö–B’÷"æÖP¢òòò†&5÷'C£¤†&5VWVVæWfW"FV6Æ&W2âcVÖ—G2V–çCcE÷B¢Ð¢òòòÇ&V7câãÆæÖSâ‚âââ“¶f÷"ÆÂöbF†VÒæBr²²&V¦V7G2ÆÂöbF†VÓ ¢òòð¢òòòÂ6ÆÂÂr²²À¢òòòÂÒÒ×ÂÒÒ×À¢òòòÂçW6‚ƒ2–Â'fö–BfÇVRæ÷B–væ÷&VB2—B÷Vv‡BFò&R"À¢òòòÂæg&öçB‚–òæ6ÆV"‚–òG—òÂ&†2æòÖVÖ&W"æÖVBg&öçF"À¢òòð¢òòò6òF†W’&R–çfÆ–FÂv†–6‚—2v†B6W&FW2F†VÒg&öÒF†R6ÖP¢òòòæÖW2–â5DDTÔTåB÷6—F–öâ(	BF†W&RF†RfÇVR—2F—66&FVBÂ6ð¢òòò6—¦VöV×G–&V6öÖRÆVvÂæòÖ÷c'Vç2æBF†W’¶VWF†P¢òòò7VvvW7F–öâà¢5·FW7EÐ¦fâå÷Væ¶æ÷vå÷VWVUöÖWF†öEöFöW5öæ÷E÷ö–çEöE÷c‚’°¢ÆWBF"ÒÇ7F×C¢g7G'Â°¢f÷&ÖB€¢"2'FW7F&Væ6‚F ¢GWB¢F÷ ¢¢VWVSÇV–çCÃƒãà¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢·7F×GÐ¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"0¢¢Ó°¢òò7FFVÖVçB÷6—F–öã¢&öw&ÒW'&÷"ÂæBF†RÖW76vRæÖW2F†R’à¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚gF"‚'æg&öçB‚’"’’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚&–â7FFVÖVçB÷6—F–öâ"’bb×6ræ6öçF–ç2‚&†2öæÇ’"’À¢'¶×6wÒ ¢“°¢òòW‡&W76–öâ÷6—F–öã¢Ç6ò&öw&ÒW'&÷"Â'’F–ffW&VçB&÷WFRà¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚gF"‚&ÆWBbÒæg&öçB‚’"’’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚&–âW‡&W76–öâ÷6—F–öâ"’bb×6ræ6öçF–ç2‚&†2öæÇ’"’À¢'¶×6wÒ ¢“°¢òòæBcw2÷vâGFV×BÂv†–6‚—2v†BÖ¶W2&÷F‚–çfÆ–Fà¢f÷"7F×B–â²'æg&öçB‚’"Â&ÆWBbÒæg&öçB‚’%Ò°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gF"‡7F×B’’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚&·7F×GÖ¢cVÖ—G3¢¶WÒ"’“°¢76W'B€¢cæ6öçF–ç2‚"çæg&öçB‚“²"’À¢&·7F×GÖ¢cVÖ—G2F†R6ÆÂ†&5VWVV†2æòÖVÖ&W"f÷" ¢“°¢Ð§Ð ¢òòò÷&VÖ÷fW2æB&WGW&ç2F†Rg&öçBVÆVÖVçBÂ6ò—BF¶W2æ÷F†–ær(	@¢òòòæBWfW'’÷'&æ6‚W6VBFò6†V6²öæÇ’F†RÔUD„ôBäÔRæBG&÷ ¢òòòF†R&wVÖVçBÆ—7Bâç÷ƒrÂ’–Æ÷vW&VBæBVÖ—GFVB6ÆVæÇ’VæFW ¢òòòD"Ô•"v†–ÆRcVÖ—GFVB÷F"çVæBç÷ƒrÂ’“¶Âv†–6‚r²²&V¦V7G2v—F€¢òòò&æòÖF6†–ærgVæ7F–öâf÷"6ÆÂFò†&5÷'C£¤†&5VWVSÆÆöærVç6–væV@¢òòò–çCã£§÷†–çBÂ–çB–"âF†RW6†'&æ6†W2æW‡BFòF†VÒ†fRÇv—0¢òòòÖF6†VB´6ÆÄ&s£¤W‡"†&r•ÖW†7FÇ“²F†—2—2F†B6†V6²w2Ö—'&÷"à¢òòð¢òòòä”äRwV&G2Âæ÷BV–v‡BÂ7&÷72DTâ÷ÆæF–æw3¢F†RFVçF€¢òòò†ÆWBbÒ6"çç÷ƒrÂ’–’—26Æ–ÖVBf—'7B'’F†RöÆFW ¢òòò5÷66÷&V&ö&E÷÷&—G’6†V6²Âv†–6‚6—2'66÷&V&ö&@¢òòò6"çç÷‚–F¶W2æò&wVÖVçG2"(	B6ÖRfW&F–7BÂF–ffW&VçBv÷&F–ærÀ¢òòòæB—B—2v‡’F†R66÷&V&ö&BfÆf÷W"V'2&VÆ÷r–â7FFVÖVç@¢òòò÷6—F–öâöæÇ’à¢5·FW7EÐ¦fâ÷VWVU÷÷÷F¶W5öæõö&wVÖVçG2‚’°¢ÆWBF"ÒÇ7F×C¢g7G'Â°¢f÷&ÖB€¢"2'FW7F&Væ6‚F ¢GWB¢F÷ ¢¢VWVSÇV–çCÃƒãà¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢çW6‚ƒ¢·7F×GÐ¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"0¢¢Ó°¢òò7FFVÖVçB÷6—F–öâæBÆWFÕ$…2÷6—F–öâ&R6W&FR'&æ6†W2à¢f÷"7F×B–â²'ç÷ƒrÂ’’"Â&ÆWBb¢V–çCÃƒâÒç÷ƒrÂ’’%Ò°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚gF"‡7F×B’’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚&÷F¶W2æò&wVÖVçG2"’bb×6ræ6öçF–ç2‚&'WB"vW&R76VB"’À¢&·7F×GÖ¢¶×6wÒ ¢“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gF"‡7F×B’’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚&·7F×GÖ¢cVÖ—G3¢¶WÒ"’“°¢76W'B€¢cæ6öçF–ç2‚"çç÷ƒrÂ’’"’À¢&·7F×GÖ¢cVÖ—G2F†R÷fW"ÖÆ–VB6ÆÃ¢·cÒ ¢“°¢Ð¢òòF†R¦W&òÖ&wVÖVçBf÷&Ò7F–ÆÂÆ÷vW'2(	BF†RwV&B×W7Bæ÷B†fP¢òò7vÆÆ÷vVBF†R÷&F–æ'’66Rà¢Æ÷vW%÷7&2‚gF"‚'ç÷‚’"’’æW‡V7B‚&&&R÷7F–ÆÂÆ÷vW'2"“°¢Æ÷vW%÷7&2‚gF"‚&ÆWBb¢V–çCÃƒâÒç÷‚’"’’æW‡V7B‚&&÷VæB÷7F–ÆÂÆ÷vW'2"“° ¢òòF†R÷F†W"VWVRfÆf÷W'2&V6‚F†R6ÖRwV&BF‡&÷Vv‚F†V—"÷và¢òò'&æ6†W2Â6òV6‚—26†V6¶VB&F†W"F†â–æfW'&VBg&öÒF†P¢òòFW7F&Væ6‚öæRà¢ÆWB7FFRÒf—‡GW&R‚'VWVU÷7FFUö†öö¶&ÆU÷FW7Bæ†&2"“°¢ÆWB66W2Ò°¢€¢&&&RF&vWB×7FFR"À¢7FFRç&WÆ6Vâ€¢"VæF–ærçW6‚‡fÇVR’"À¢"VæF–ærçW6‚‡fÇVR•ÆâVæF–ærç÷ƒrÂ’’"À¢À¢’À¢’À¢€¢&–ç7Fæ6R×VÆ–f–VBF&vWB×7FFR"À¢7FFRç&WÆ6Vâ€¢"ÖöFVÂæVçVWVRƒr’"À¢"ÖöFVÂæVçVWVRƒr•ÆâÖöFVÂçVæF–ærç÷ƒrÂ’’"À¢À¢’À¢’À¢€¢'66÷&V&ö&BVWVR"À¢"2'66÷&V&ö&B6 ¢¢VWVSÇV–çCÃ3#ãà¦VæB66÷&V&ö&B6  §FW7F&Væ6‚F ¢GWB¢F÷ ¢6"¢6 ¦VæBFW7F&Væ6‚F  ¦–×ÂFW7Bf÷"F ¢'Và¢6"ççW6‚ƒ2¢6"çç÷ƒrÂ’¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂFW7B"0¢çFõ÷7G&–ær‚’À¢’À¢€¢&6ö×öæVçBVWVR"À¢"2&vVçB6öÆÀ¢W''2¢VWVSÇV–çCÃ3#ãà¢†öö¶&ÆRæ÷FR‡c¢V–çCÃ3#â¢W''2çW6‚‡b¢W''2ç÷ƒrÂ’¢VæBæ÷FP¦VæBvVçB6öÆÀ §FW7B7FW7@¢ÆWBGWB¢F÷ ¢ÆWB2¢6öÆÀ¢'Và¢2ææ÷FRƒ2¢v—B7–6ÆP¢VæB'Và¦VæBFW7B7FW7B"0¢çFõ÷7G&–ær‚’À¢’À¢Ó°¢f÷"‡v†BÂ7&2’–â66W2°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&÷F¶W2æò&wVÖVçG2"’Â'·v†GÓ¢¶×6wÒ"“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·v†GÓ¢¶WÒ"’“°¢76W'B€¢cæ6öçF–ç2‚"ç÷ƒrÂ’’"’À¢'·v†GÓ¢cVÖ—G2F†R÷fW"ÖÆ–VB6ÆÂ ¢“°¢Ð ¢òò(
fæBF†RÆWFÕ$…2'&æ6‚öbV6‚Âv†–6‚—24U$DRwV&Bà¢òò6†V6¶–æröæÇ’F†RFW7F&Væ6‚fÆf÷W"–â&÷F‚÷6—F–öç2ÆVgBF‡&VP¢òòöbF†VÒVç–ææVC¢FVÆWF–ærÆÂF‡&VRÆWFÕ$…26ÆÇ2¶WBF†P¢òòv†öÆR7V—FRw&VVâÂæBÆWB¢¢V–çCÃ3#âÒW''2ç÷ƒrÂ’–vVç@¢òò&6²FòÆ÷vW&–ær6ÆVæÇ’v–ç7BcF†BVÖ—G0¢òòV–çCcE÷B¢Ò6VÆbæW''2ç÷ƒrÂ’“¶à¢ÆWB&÷VæBÒ°¢€¢&&&RF&vWB×7FFR"À¢7FFRç&WÆ6Vâ€¢"VæF–ærçW6‚‡fÇVR’"À¢"VæF–ærçW6‚‡fÇVR•ÆâÆWB¢¢V–çCÃƒâÒVæF–ærç÷ƒrÂ’’"À¢À¢’À¢’À¢€¢&–ç7Fæ6R×VÆ–f–VBF&vWB×7FFR"À¢7FFRç&WÆ6Vâ€¢"ÖöFVÂæVçVWVRƒr’"À¢"ÖöFVÂæVçVWVRƒr•ÆâÆWB¢¢V–çCÃƒâÒÖöFVÂçVæF–ærç÷ƒrÂ’’"À¢À¢’À¢’À¢€¢&6ö×öæVçBVWVR"À¢"2&vVçB6öÆÀ¢W''2¢VWVSÇV–çCÃ3#ãà¢†öö¶&ÆRæ÷FR‡c¢V–çCÃ3#â¢W''2çW6‚‡b¢ÆWB¢¢V–çCÃ3#âÒW''2ç÷ƒrÂ’¢VæBæ÷FP¦VæBvVçB6öÆÀ §FW7B7FW7@¢ÆWBGWB¢F÷ ¢ÆWB2¢6öÆÀ¢'Và¢2ææ÷FRƒ2¢v—B7–6ÆP¢VæB'Và¦VæBFW7B7FW7B"0¢çFõ÷7G&–ær‚’À¢’À¢Ó°¢f÷"‡v†BÂ7&2’–â&÷VæB°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚&÷F¶W2æò&wVÖVçG2"’À¢'·v†GÒ†ÆWBÕ$…2“¢¶×6wÒ ¢“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·v†GÓ¢¶WÒ"’“°¢76W'B€¢cæ6öçF–ç2‚"ç÷ƒrÂ’’"’À¢'·v†GÒ†ÆWBÕ$…2“¢cVÖ—G2F†R÷fW"ÖÆ–VB6ÆÂ ¢“°¢Ð§Ð ¢òòòW6†–âW‡&W76–öâ÷6—F–öâ—2F†R÷F†W"†ÆböbF†BfÆÆ&6²À¢òòòæB—B—2&öw&ÒW'&÷"f÷"D”ddU$TåB&V6öã¢W6†—2&VÀ¢òòò†&5VWVVÖVÖ&W"Â—B§W7B&WGW&ç2fö–BÂ6òcw2V–çCcE÷B¢Ð¢òòò÷F"ç6"ççW6‚ƒ2“¶vWG2'fö–BfÇVRæ÷B–væ÷&VB2—B÷Vv‡BFò&R ¢òòò&F†W"F†â&†2æòÖVÖ&W"æÖVB"âöæR&ÒÂGvòÖV6†æ—6×2ÂGvð¢òòòÖW76vW2à¢5·FW7EÐ¦fâ÷VWVU÷W6…ö–åöW‡&W76–öå÷÷6—F–öåö—5ö÷&öw&ÕöW'&÷"‚’°¢ÆWB7&2Ò"2'66÷&V&ö&B6 ¢¢VWVSÇV–çCÃ3#ãà¦VæB66÷&V&ö&B6  §FW7F&Væ6‚F ¢GWB¢F÷ ¢6"¢6 ¦VæBFW7F&Væ6‚F  ¦–×ÂFW7Bf÷"F ¢'Và¢6"ççW6‚ƒ2¢ÆWB¢¢V–çCÃ3#âÒ6"ççW6‚ƒ2¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂFW7B"3°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‡7&2’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚&W6†&WGW&ç2æòfÇVR"’À¢'F†RÖW76vR×W7BæÖRF†RÖV6†æ—6ÒÂæ÷BF†RÖVÖ&W"Æ—7C¢¶×6wÒ ¢“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‡7&2’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢cæ6öçF–ç2‚'¢Ò÷F"ç6"ççW6‚ƒ2“²"’À¢'c&–æG2F†Rfö–B&W7VÇBÂv†–6‚—2v†BFöW2æ÷B6ö×–ÆS¢·cÒ ¢“° ¢òòf÷W"öbF†Rf—fRW‡&W76–öâÆæF–æw2&V6‚F†BÖW76vRâF†P¢òòDU5D$Tä4‚Ö÷væVBöæRFöW2æ÷C¢Æ÷vW%÷F%÷VWVU÷VW'•ö6ÆÆ'Vç0¢òò—G2&w2æ—5öV×G’‚–&—G’6†V6²$Tdõ$RF†RÖWF†öBÖF6‚Â6ð¢òòVæBçW6‚ƒ2–—2&VgW6VBf÷"—G2&wVÖVçG2&F†W"F†âf÷"—G0¢òò&WGW&âG—Râ6ÖR–çfÆ–FfW&F–7BÂF–ffW&VçBÖW76vR(	B–ææV@¢òò6òF†R'GvòÖV6†æ—6×2ÂGvòÖW76vW2"6Æ–Ò—2æ÷B&VB0¢òò6÷fW&–ærÆÂf—fRà¢ÆWBF"Ò"2'FW7F&Væ6‚F ¢GWB¢F÷ ¢VæB¢VWVSÇV–çCÃ3#ãà¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢VæBçW6‚ƒ2¢ÆWB¢¢V–çCÃ3#âÒVæBçW6‚ƒ2¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"3°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‡F"’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚'F¶W2æò&wVÖVçG2"’À¢'F†RFW7F&Væ6‚ÆæF–ær—26Æ–ÖVB'’—G2&—G’6†V6²f—'7C¢¶×6wÒ ¢“° ¢òòF†R÷F†W"F‡&VRÆæF–æw2ÂV6‚&V6†VBöâ—G2÷vââæWWG&Æ—6–æp¢òòF†R6ö×öæVçBò&&R×F&vWB×7FFRò–ç7Fæ6R×F&vWB×7FFR6ÆÇ0¢òòöæRBF–ÖRÆVgBF†Rv†öÆR7V—FRw&VVâ(	B&ÖV7W&VBBÆÂf—fP¢òòÆæF–æw2"v2G'VRöbF†R&ö&–æræBfÇ6RöbF†R&Vw&W76–öà¢òòFW7G2Âv†–6‚—2F†R6ÖRvF†R÷wV&G2†Bà¢ÆWB7FFRÒf—‡GW&R‚'VWVU÷7FFUö†öö¶&ÆU÷FW7Bæ†&2"“°¢ÆWB66W2Ò°¢€¢&6ö×öæVçBVWVR"À¢"2&vVçB6öÆÀ¢W''2¢VWVSÇV–çCÃ3#ãà¢†öö¶&ÆRæ÷FR‡c¢V–çCÃ3#â¢W''2çW6‚‡b¢ÆWB¢¢V–çCÃ3#âÒW''2æg&öçB‚¢VæBæ÷FP¦VæBvVçB6öÆÀ §FW7B7FW7@¢ÆWBGWB¢F÷ ¢ÆWB2¢6öÆÀ¢'Và¢2ææ÷FRƒ2¢v—B7–6ÆP¢VæB'Và¦VæBFW7B7FW7B"0¢çFõ÷7G&–ær‚’À¢’À¢€¢&&&RF&vWB×7FFR"À¢7FFRç&WÆ6Vâ€¢"VæF–ærçW6‚‡fÇVR’"À¢"VæF–ærçW6‚‡fÇVR•ÆâÆWB¢¢V–çCÃƒâÒVæF–æræg&öçB‚’"À¢À¢’À¢’À¢€¢&–ç7Fæ6R×VÆ–f–VBF&vWB×7FFR"À¢7FFRç&WÆ6Vâ€¢"ÖöFVÂæVçVWVRƒr’"À¢"ÖöFVÂæVçVWVRƒr•ÆâÆWB¢¢V–çCÃƒâÒÖöFVÂçVæF–æræg&öçB‚’"À¢À¢’À¢’À¢Ó°¢f÷"‡v†BÂ7&2’–â66W2°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚&–âW‡&W76–öâ÷6—F–öâ"’bb×6ræ6öçF–ç2‚&†2öæÇ’"’À¢'·v†GÓ¢¶×6wÒ ¢“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·v†GÓ¢¶WÒ"’“°¢76W'B€¢cæ6öçF–ç2‚"æg&öçB‚“²"’À¢'·v†GÓ¢cVÖ—G2F†R6ÆÂ†&5VWVV†2æòÖVÖ&W"f÷" ¢“°¢Ð§Ð ¢òòòFVfVÇFöâVWVRf–VÆC¢cVÖ—G2—B–çFòF†RÖVÖ&W ¢òòò–æ—F–Æ—¦W"††&5VWVSÇV–çCcE÷CâÒ¶’æB†&5VWVV†2æð¢òòò7V6‚6öç7G'V7F÷"à¢5·FW7EÐ¦fâ÷VWVUöf–VÆEöFVfVÇEöFöW5öæ÷E÷ö–çEöE÷c‚’°¢ÆWBW'"ÒÆ÷vW%÷7&2€¢"2'FW7F&Væ6‚F ¢GWB¢F÷ ¢¢VWVSÇV–çCÃƒãâFVfVÇB ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"2À¢¢çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¤VÖ—G5Væ6ö×–Æ&ÆR“°¢76W'B€¢×6ræ6öçF–ç2‚&FVfVÇBöâFW7F&Væ6‚VWVRf–VÆB"’À¢'¶×6wÒ ¢“°§Ð ¢òòòæÖW2F†B&W6öÇfRFòæ÷F†–ærÂæB6Æ÷G2F†BvW&RæWfW"FV6Æ&VBÀ¢òòò&R76VBF‡&÷Vv‚fW&&F–Ò'’c(	BâVæFV6Æ&VB–FVçF–f–W"–âF†P¢òòòVÖ—GFVB2²²Âv†–6‚æWfW"6ö×–ÆW2&Vv&FÆW72öbv†W&R—BÆæG2à¢5·FW7EÐ¦fâVç&W6öÇfVEöæÖW5öFõöæ÷E÷ö–çEöE÷c‚’°¢òòÆWB†v—F‚æòG—R—2äõB–âF†—2Æ—7C¢cVÖ—G2öæÇ’6öÖÖVç@¢òòf÷"F†RFV6Æ&F–öâÂ6òv†WF†W"—G2÷WGWB6ö×–ÆW2FWVæG2öà¢òòv†WF†W"F†RæÖR—2ÆFW"W6VB(	BæBF†R&V¦V7F–öâf—&W2BF†P¢òòFV6Æ&F–öâÂ&Vf÷&RF†B—2¶æ÷vâà¢ÆWB66W3¢²‚g7G"Âg7G"“²%ÒÒ°¢‚&ÆWB‚Òæ÷7V6‡F†–ær"Â'Vç&W6öÇfVBæÖRæ÷7V6‡F†–æv"’À¢€¢&æ÷7V6‡F†–ærÒ"À¢&76–væÖVçBFòVæ¶æ÷vâæÖRæ÷7V6‡F†–æv"À¢’À¢Ó°¢f÷"‡7F×BÂvçB’–â66W2°¢ÆWBW'"ÒÆ÷vW%÷7&2‚ff÷&ÖB€¢"2'FW7F&Væ6‚F ¢GWB¢F÷ ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢·7F×GÐ¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"0¢’¢çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¤VÖ—G5Væ6ö×–Æ&ÆR“°¢76W'B†×6ræ6öçF–ç2‡vçB’Â&·7F×GÖ¢¶×6wÒ"“°¢Ð§Ð ¢òòò–æFW†–ær66Æ"&V6÷&Bf–VÆBÂöâV—F†W"6–FRöbâ76–væÖVçC¢c¢òòòVÖ—G2F†R7V'67&—BfW&&F–Òv–ç7BV–çCcE÷FÖVÖ&W"à¢5·FW7EÐ¦fâ–æFW†–æuö÷66Æ%÷&V6÷&Eöf–VÆEöFöW5öæ÷E÷ö–çEöE÷c‚’°¢f÷"7F×B–â²&ÆWBBÒ"çe³Ò"Â&"çe³ÒÒ2%Ò°¢ÆWBW'"ÒÆ÷vW%÷7&2‚ff÷&ÖB€¢"2'7G'V7B ¢b¢V–çCÃƒà¦VæB7G'V7B  §FW7F&Væ6‚F ¢GWB¢F÷ ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢ÆWB"¢ ¢·7F×GÐ¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"0¢’¢çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¤VÖ—G5Væ6ö×–Æ&ÆR“°¢76W'B€¢×6ræ6öçF–ç2‚&–æFW†–ærF†R66Æ"&V6÷&Bf–VÆB"çf"’À¢'¶×6wÒ ¢“°¢Ð§Ð ¢òòòv†öÆRÖfV6$TB¶VW2—G2ÒÖ6öFVvVâc7VvvW7F–öâÂ&V6W6Rv†@¢òòòcFöW2v—F‚öæRFWVæG2öâv†W&R—BÆæG3¢76W'B"æFFÓÒ"æFF ¢òòòVÖ—G2"æFFÓÒ"æFFÂv†–6‚6ö×–ÆW2æBv÷&·2†7FC£¦'&–†0¢òòò÷W&F÷#ÓÖ’âöæR6—FRÂ6WfW&Â÷WF6öÖW2(	B6òF†R†öæW7BÆ&VÂ—0¢òòòF†RöæRF†B—2G'VR6öÖWv†W&RÂæBF†RFWF–ÂÆVG2v—F‚F†Rf—€¢òòòF†Bv÷&·2WfW'—v†W&Rà¢5·FW7EÐ¦fâ÷v†öÆU÷fV5÷&VEö¶VW5ö—G5÷c÷7VvvW7F–öâ‚’°¢ÆWBW'"ÒÆ÷vW%÷7&2€¢"2'7G'V7B'VæFÆP¢FF¢fV3ÇV–çCÃ3#âÂCà¦VæB7G'V7B'VæFÆP §FW7F&Væ6‚F ¢GWB¢F÷ ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢ÆWB"¢'VæFÆP¢76W'B"æFFÓÒ"æFFVÇ6Rf–Â‚&æ÷R"¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"2À¢¢çVçw&öW'"‚“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fW'"“°¢76W'B€¢×6ræ6öçF–ç2‚'v†öÆRÖfV6&VBöb&V6÷&Bf–VÆB"’bb×6ræ6öçF–ç2‚&VÆVÖVçB×v—6R"’À¢'¶×6wÒ ¢“°§Ð ¢òòòF†RÆö÷ÖVÆVÖVçBw&—FR&V¦V7F–öâ6÷fW'2WfW'’7FFVÖVçBF†BæÖW2¢òòòÆö6ÂDU5D”äD”ôâÂæ÷B§W7B76–væâÆ÷vW%ö76–væ&÷WFW0¢òòò‚Ò6"çç÷‚––çFò66÷&V&ö&D÷£¥VWVU÷²FW7BÖæ@¢òòò‚Ò†7BæÒ‚âââ––çFòG&ç67F÷$6ÆÂ²FW7BÖÂ6ò6†V6²F†@¢òòòöæÇ’Æöö¶VBf÷"76–væv÷VÆB72W†7FÇ’F†÷6Rw&—FW2F‡&÷Vv‚(	@¢òòòF†R6öçF–æW"æWfW"WFFVBÂ6–ÆVçFÇ’à¢5·FW7EÐ¦fâöÆö÷öVÆVÖVçE÷w&—FU÷F‡&÷Vv…öö6ÆÅöFW7F–æF–öåö—5÷&V¦V7FVB‚’°¢ÆWBW'"ÒÆ÷vW%÷7&2€¢"2'7G'V7B'VæFÆP¢FF¢fV3ÇV–çCÃ3#âÂCà¦VæB7G'V7B'VæFÆP §66÷&V&ö&B6 ¢6VVâ¢VWVSÇV–çCÃ3#ãà¦VæB66÷&V&ö&B6  §FW7F&Væ6‚F ¢GWB¢F÷ ¢6"¢6 ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢ÆWB"¢'VæFÆP¢6"ç6VVâçW6‚ƒ¢f÷"‚–â"æFF¢‚Ò6"ç6VVâç÷‚¢VæBf÷ ¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"2À¢¢çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2“°¢76W'B€¢×6ræ6öçF–ç2‚&w&—FRFòf÷&Æö÷w2VÆVÖVçBf&–&ÆR"’À¢'¶×6wÒ ¢“°§Ð ¢òòòf÷"B–âÇG6W×&W7VÇCæFVÆ–&W&FVÇ’FöW2äõBvWBF†Rw&—FP¢òòò&V¦V7F–öââ—G2'’Ö6÷’Æö÷f&–&ÆR&VFFW2F†—27vVWÂæ@¢òòòf÷"B–âG†ç2(
bBæFG"Ò(
bVæBf÷&—2â–F–öÒ&÷F‚&6¶VæG0¢òòò66WBFöF’(	BGW&æ–ær—B–çFòâW'&÷"v÷VÆB'&V²v÷&¶–æp¢òòò&öw&×2Âv†–6‚—2æ÷Bv†BvÖ6Æ÷6–ær6†ævRÖ’Fòà¢5·FW7EÐ¦fâ÷G6WöÆö÷öVÆVÖVçE÷w&—FU÷7F–ÆÅöÆ÷vW'2‚’°¢ÆWB7ÒVÖ—Eö7÷7&2€¢"2'G&ç67F–öâG†à¢FG"¢V–çCÃ3#âFVfVÇB ¦VæBG&ç67F–öâG†à §G6W'W'7B‚’ÓâE6WÅG†ãà¢ÆWBB¢G†à¢––VÆB@¦VæBG6W'W'7@ §FW7F&Væ6‚F ¢GWB¢F÷ ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢f÷"B–â'W'7B‚¢BæFG"ÒP¢VæBf÷ ¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"2À¢“°¢76W'B†7æ6öçF–ç2‚&FG"ÒR"’Â'¶7Ò"“°§Ð ¢òòòæöâÖÆVbVÆVÖVçB6VÆV7F÷"†BæVçG&–W5¶µÒæFF’—26æ6†÷GFVB–çFð¢òòòFV×$Tdõ$RF†RÆö÷âcw2&ævRÖf÷"WfÇVFW2F†R6öçF–æW ¢òòòW‡&W76–öâöæ6S²6VÆV7F÷"ÆVgB–ç6–FRF†RW"Ö—FW&F–öâ&–æBv÷VÆ@¢òòòvÆ²F–ffW&VçB&÷rV6‚F–ÖRF†R&öG’6†ævVB¶à¢5·FW7EÐ¦fâ÷fV5öÆö÷÷6æ6†÷G5ö—G5ö6öçF–æW%÷6VÆV7F÷"‚’°¢ÆWB7ÒVÖ—Eö7÷7&2€¢"2'7G'V7B&÷p¢FF¢fV3ÇV–çCÃ3#âÂ3à¦VæB7G'V7B&÷p§7G'V7BF&À¢VçG&–W2¢fV3Å&÷rÂCà¦VæB7G'V7BF&À §FW7F&Væ6‚F ¢GWB¢F÷ ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢ÆWBB¢F&À¢ÆWB²Ò ¢ÆWB7VÒÒ ¢f÷"‚–âBæVçG&–W5¶µÒæFF¢7VÒÒ7VÒ²€¢²Ò²²¢VæBf÷ ¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"2À¢“°¢76W'B€¢7æ6öçF–ç2‚'BæVçG&–W5¶µÒ"’À¢'F†R6VÆV7F÷"×W7B&R6æ6†÷GFVBÂæ÷B&R×&VBV6‚—FW&F–öã¥Æç¶7Ò ¢“°¢76W'B€¢7æ6öçF–ç2‚'BæVçG&–W5µõ÷B"’bb7æ6öçF–ç2‚&²Ò†²²’"’À¢'F†R&öG’7F–ÆÂGfæ6W2¶²F†RÆö÷§W7B7F÷2föÆÆ÷v–ær—C¥Æç¶7Ò ¢“°§Ð ¢òòò÷'B&VB–âF†R6VÆV7F÷"†ö—7G2–çFòGWE&VF†VBöbF†P¢òòòÆö÷âÆVgB–ç6–FRF†R&–æB—B&V6†VBF†RfW&–f–W"0¢òòò÷'D–äF—6ÆÆ÷vVE÷6—F–öæ(	Bâ–çFW&æÂÖ'Vr6†ææVÂÂ&–çFVBFòF†P¢òòòW6W"2&r•"à¢5·FW7EÐ¦fâ÷÷'Eö–åö÷fV5öÆö÷÷6VÆV7F÷%ö†ö—7G2‚’°¢ÆWB7ÒVÖ—Eö7÷7&2€¢"2'7G'V7B&÷p¢FF¢fV3ÇV–çCÃ3#âÂ3à¦VæB7G'V7B&÷p§7G'V7BF&À¢VçG&–W2¢fV3Å&÷rÂCà¦VæB7G'V7BF&À §FW7F&Væ6‚F ¢GWB¢F÷ ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢ÆWBB¢F&À¢ÆWB7VÒÒ ¢f÷"‚–âBæVçG&–W5¶GWBæÒæFF¢7VÒÒ7VÒ²€¢VæBf÷ ¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"2À¢“°¢76W'B€¢7æ6öçF–ç2‚&†&5÷'C£¦†&5÷&VB†GWBÓæ’"’À¢'F†R÷'B&VB—2VÖ—GFVBöæ6RÂ†VBöbF†RÆö÷¥Æç¶7Ò ¢“°¢76W'B€¢7æ6öçF–ç2‚&VçG&–W5¶†&5÷'C£¦†&5÷&VB"’À¢&æBæ÷B–ç6–FRF†RW"Ö—FW&F–öâ&–æC¥Æç¶7Ò ¢“°§Ð ¢òòòâVæ–æ—F–Æ—¦VBÆWB†v—F‚æòG—R¶VW2—G2ÒÖ6öFVvVâc ¢òòò7VvvW7F–öââcVÖ—G2öæÇ’6öÖÖVçBf÷"F†RFV6Æ&F–öâÂ6òv†WF†W ¢òòò—G2÷WGWB6ö×–ÆW2FWVæG2öâv†WF†W"F†RæÖR—2ÆFW"U4TB(	Bæ@¢òòòF†R&V¦V7F–öâf—&W2BF†RFV6Æ&F–öâÂ&Vf÷&RF†B—2¶æ÷vâà¢5·FW7EÐ¦fâå÷VçG—VE÷Væ–æ—F–Æ—¦VEöÆWEö¶VW5ö—G5÷c÷7VvvW7F–öâ‚’°¢ÆWBW'"ÒÆ÷vW%÷7&2€¢"2'FW7F&Væ6‚F ¢GWB¢F÷ ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢ÆWB€¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"2À¢¢çVçw&öW'"‚“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fW'"“°¢76W'B€¢×6ræ6öçF–ç2‚'Væ–æ—F–Æ—¦VBÆWB†v—F†÷WB66Æ"G—R"’À¢'¶×6wÒ ¢“°§Ð ¢òòÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÐ¢òò&F6‚S¢6öç7FçBÖföÆFVBf–VÆBFVfVÇG2ÂæBF†RF—&V7F–öæÂÖf–VÆ@¢òòfÖ–Ç’à¢òòÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÐ ¢òòòf–VÆBFVfVÇFföÆG2F‡&÷Vv‚F†Rf–ÆRw26öç7FçBF&ÆRÂöâWfW'¢òòòFV6Æ&F–öâF†B†2öæRâcVÖ—G2F†RFVfVÇBw24õU$4RDU…B–çFòF†P¢òòò2²²ÖVÖ&W"–æ—F–Æ—¦W#¢Æ—FW&Â†Òv’æB6öç7FæÖR†Ò¶¢òòòv÷&²F†W&RÂ'WBå’÷F†W"W‡&W76–öâ6–ÆVçFÇ’FVw&FW2FòÒÂ6ò¢òòòFVfVÇB²f–VÆB7F'G2B&F†W"F†â"à¢5·FW7EÐ¦fâöf–VÆEöFVfVÇEöföÆG5÷F‡&÷Vv…÷F†Uö6öç7FçE÷F&ÆR‚’°¢f÷"†FV6ÂÂf–VÆB’–â°¢òò6ö×öæVçB†Vçb’f–VÆBà¢€¢&VçbUÆââ¢V–çCÃƒâFVfVÇB·ÕÆæVæBVçbUÆåÆçFW7F&Væ6‚F%ÆâGWB¢F÷ÆâF÷¢UÆæVæBFW7F&Væ6‚F""À¢&â"À¢’À¢òò66÷&V&ö&Bf–VÆBà¢€¢'66÷&V&ö&BUÆââ¢V–çCÃ3#âFVfVÇB·ÕÆæVæB66÷&V&ö&BUÆåÆçFW7F&Væ6‚F%ÆâGWB¢F÷ÆâF÷¢UÆæVæBFW7F&Væ6‚F""À¢&â"À¢’À¢Ò°¢f÷"–æ—B–â²#r"Â$²"Â$²²""Â#²"Â$²¢"ÒR%Ò°¢ÆWB7ÒVÖ—Eö7÷7&2‚ff÷&ÖB€¢&6öç7B²ÒuÆåÆç·ÕÆæ–×ÂBf÷"F%Æâ'VåÆâv—B7–6ÆUÆâVæB'VåÆæVæB–×ÂB"À¢FV6Âç&WÆ6R‚'·Ò"Â–æ—B¢’“°¢ÆWBvçBÒÖF6‚–æ—B°¢#r"Â$²"Óâ#Òs²"À¢$²²""Óâ#Ò“²"À¢#²"Óâ#Ò#²"À¢òÓâ#Ò“²"À¢Ó°¢76W'B€¢7æ6öçF–ç2‚ff÷&ÖB‚'¶f–VÆGÒ·vçGÒ"’’À¢&FVfVÇB¶–æ—GÖ×W7BföÆBFò·vçGÖ¥Æç¶7Ò ¢“°¢Ð¢Ð§Ð ¢òòòF†R6ÖRöâG&ç67F÷"7FFRf–VÆBÂv†–6‚Föö²6W&FRF‚à¢5·FW7EÐ¦fâ÷G&ç67F÷%÷7FFUöf–VÆEöFVfVÇEöföÆG2‚’°¢ÆWB7ÒVÖ—Eö7÷7&2€¢"2&6öç7B²Ò §G&ç67F÷"‡@¢GWB¢F÷ ¢6÷VçB¢V–çCÃƒâFVfVÇB²² ¢v†Vâ7F—fP¢†öö¶&ÆRvò‚¢v—B7–6ÆP¢VæBvð¢VæBv†Và¦VæBG&ç67F÷"‡@ §FW7F&Væ6‚F ¢GWB¢F÷ ¢‡B¢‡B7F—fP¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢‡BæGWBÒGW@¢‡Bævò‚¢VæB'Và¦VæB–×ÂB"2À¢“°¢76W'B†7æ6öçF–ç2‚&6÷VçBÒ²"’Â'¶7Ò"“°§Ð ¢òòòFVfVÇBF†B—2æ÷B6öç7FçBBÆÂ—2æ÷BcW66R†F6ƒ¢c¢òòòVÖ—G2ÒæB'Vç2Â6òF†Rf–VÆB6–ÆVçFÇ’†öÆG2F†Rw&öærfÇVRà¢òòòâ”ÄÄTtÂ6öç7FçBWfÇVF–öâ—2F–ffW&VçB6Æ72v–â(	B—BvWG0¢òòòF†RÆ÷vW$W'&÷#£¤–çfÆ–F6öç7FFV6Æ&F–öâv÷VÆBvWBf÷"F†P¢òòò6ÖRW‡&W76–öâÂ–æ6ÇVF–ærF†Rf–VÆBw2÷vâ&ævR6†V6²à¢5·FW7EÐ¦fâö&Eöf–VÆEöFVfVÇEö—5ö6Æ76–f–VEö'•÷v‡•ö—Eö—5ö&B‚’°¢ÆWB7&2ÒÆ–æ—C¢g7G'Â°¢f÷&ÖB€¢"2&6öç7B²Òp ¦VçbP¢â¢V–çCÃƒâFVfVÇB¶–æ—GÐ¦VæBVçbP §FW7F&Væ6‚F ¢GWB¢F÷ ¢F÷¢P¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"0¢¢Ó° ¢òòæ÷B6öç7FçBBÆÂ(i"&6¶VæBÖv&W÷'BÂæBäõBö–çFW"@¢òòcÂv†–6‚66WG2—BæBVÖ—G2Òà¢ÆWBW'"ÒÆ÷vW%÷7&2‚g7&2‡"2"'‚""2’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2“°¢76W'B€¢×6ræ6öçF–ç2‚&æöâÖ6öç7FçBFVfVÇBöâ6ö×öæVçBf–VÆBRææ"’À¢'¶×6wÒ ¢“° ¢òò6öç7FçBÂ'WB–ÆÆVvÂ(	BF†R6ÖRF‡&VR6†W26öç7F ¢òòFV6Æ&F–öâ&V¦V7G2Â&W÷'FVBF†R6ÖRv’à¢f÷"†–æ—BÂvçB’–â°¢‚#ò"Â&F—f—6–öâ'’¦W&ò"’À¢‚"Ó"Â&æVvF—fRfÇVW26ææ÷B–æ—F–Æ—¦RâVç6–væVB6öç7FçB"’À¢‚#3"Â&FöW2æ÷Bf—BV–çCÃƒæ†Ö‚#SR’"’À¢Ò°¢ÆWBW'"ÒÆ÷vW%÷7&2‚g7&2†–æ—B’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fW'"“°¢76W'B†×6ræ6öçF–ç2‡vçB’Â&FVfVÇB¶–æ—GÖ¢¶×6wÒ"“°¢76W'B€¢×6ræ6öçF–ç2‚&6ö×öæVçBf–VÆBRææ"’À¢'F†RF–væ÷7F–2æÖW2F†Rf–VÆC¢¶×6wÒ ¢“°¢Ð§Ð ¢òòòF†RföÆB&V6†W2FW7F&Væ6‚f–VÆG2FöòÂ6òFVfVÇB¶ÖVç2F†R6ÖP¢òòòF†–æröâFW7F&Væ6‚f–VÆB2öââVçff–VÆB–âF†R6ÖR6÷W&6Rà¢5·FW7EÐ¦fâ÷FW7F&Væ6…öf–VÆEöFVfVÇEöföÆG5öÆ–¶Uöö6ö×öæVçEööæR‚’°¢ÆWB7ÒVÖ—Eö7÷7&2€¢"2&6öç7B²Òp §FW7F&Væ6‚F ¢GWB¢F÷ ¢â¢V–çCÃƒâFVfVÇB²²¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂB"2À¢“°¢76W'B†7æ6öçF–ç2‚&âÒƒ²"’Â'¶7Ò"“°§Ð  ¢òòÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÐ¢òò&F6‚c¢&V6÷&B×G—VBf–VÆG2öââVæ&÷VæBG&ç67F÷"ÂæBF†P¢òò6öææV7FVæGö–çBF–væ÷7F–72à¢òòÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÒÐ ¢òòò&V6÷&B×G—VBf–VÆBöââTä$õTäBG&ç67F÷&&÷WFW2F‡&÷Vv‚F†P¢òòò6ÖRÆ÷vW%÷7FFUöf–VÆFF†R&÷VæB×FòF‚Ç&VG’W6VBâcVÖ—G2¢òòò&VÂ7G'V7BÖVÖ&W"æB—Bv÷&·2Â6òF†—2v2&VÂvà¢òòð¢òòò&V6†–ær—BW‡÷6VBÆFVçBVÖ—GFW"'Vs ¢òòò7F×C£¥G&ç67F÷%7FFU&V6÷&Df–VÆEw&—FV–çFW'öÆFVB—G2–ç7Fæ6V ¢òòò&rÂæBG&ç67F÷"w2÷vâÖWF†öB&öG’6'&–W2âTÕE’–ç7Fæ6Rf÷ ¢òòò6VÆb×&VfW&Væ6R(	B6òF†Rw&—FRVÖ—GFVBæ7W"çFrÒS¶Âv—F‚¢òòòÆVF–ærF÷BÂv†–6‚—2æ÷B2²²à¢5·FW7EÐ¦fâ÷&V6÷&Eöf–VÆEööåöå÷Væ&÷VæE÷G&ç67F÷%öÆ÷vW'2‚’°¢ÆWB7ÒVÖ—Eö7÷7&2€¢"2'7G'V7B&V@¢Fr¢V–çCÃƒâFVfVÇB ¦VæB7G'V7B&V@ §G&ç67F÷"‡@¢GWB¢F÷ ¢7W"¢&V@ ¢v†Vâ7F—fP¢†öö¶&ÆRvò‚¢7W"çFrÒP¢v—B7–6ÆP¢VæBvð¢VæBv†Và¦VæBG&ç67F÷"‡@ §FW7F&Væ6‚F ¢GWB¢F÷ ¢‡B¢‡B7F—fP¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢‡BæGWBÒGW@¢‡Bævò‚¢76W'B‡Bæ7W"çFrÓÒRVÇ6Rf–Â‚'Fr"¢VæB'Và¦VæB–×ÂB"2À¢“°¢76W'B†7æ6öçF–ç2‚$&VB7W'·Ó²"’Â'F†RÖVÖ&W"—2FV6Æ&VC¥Æç¶7Ò"“°¢76W'B€¢7æ6öçF–ç2‚'6VÆe÷7FFRæ7W"çFrÒS²"’À¢'F†R6VÆb×w&—FR&W6öÇfW2—G2&V6V—fW"(	B&rV×G’–ç7Fæ6RVÖ—GFVBÀ¢æ7W"çFrÒS¶¥Æç¶7Ò ¢“°¢76W'B€¢7æ6öçF–ç2‚"æ7W"â"’À¢&æòÆVF–ærÖF÷BÖVÖ&W"66W727W'f—fW3¥Æç¶7Ò ¢“°¢76W'B†7æ6öçF–ç2‚'‡Bæ7W"çFrÓÒR"’Â'F†RFW7B&VG2—B&6³¥Æç¶7Ò"“°§Ð ¢òòòÖÆf÷&ÖVB6öææV7FVæGö–çB¶VW2—G2ÒÖ6öFVvVâc7VvvW7F–öâÀ¢òòòæBF†R&V6öâ—2F†Rv†öÆRö–çBöbF†R6Æ727—7FVÓ¢v†BcFöW0¢òòòv—F‚&BVFvRFWVæG2öâv†W&RF†RVFvR4•E2Âæ÷Böâ†÷r—B—0¢òòòÖÆf÷&ÖVBà¢òòð¢òòòÒ–ââ”å5DåD”DTBVçbÂcVÖ—G2F†RF‚fW&&F–Ò–çFò¢òòòW6…ö&6¶÷"&ævRÖf÷"ÂæBF†R&W7VÇBW7VÆÇ’FöW2æ÷@¢òòò6ö×–ÆRà¢òòòÒ4”ätÄRÕ4TtÔTåBVæGö–çB&W6öÇfW2v–ç7BF†R÷væW"w2÷và¢òòò†öö¶&ÆR÷"÷WBWfVçFæBv÷&·2(	BU÷F¶R…÷F"çF÷Â÷B–à¢òòòÒ–ââTä”å5DåD”DTBVçbÂcVÖ—G2æòv—&–ærBÆÂÂ6òWfW'¢òòòÖÆf÷&ÖVBVFvRF†W&R—2–çf—6–&ÆRæBc6–×Ç’7V66VVG2âF&— ¢òòò&W6öÇfW26öææV7Ff÷"WfW'’Vçb–âF†RÖW&vVBf–ÆRÂ6ò—B6VW0¢òòòVFvW2cæWfW"&V6†W2à¢òòð¢òòòöæR6—FRÂF‡&VR÷WF6öÖW2âæò6–ævÆRc7FGW6—2†öæW7BÂ6òF†P¢òòò7VvvW7F–öâ7F—2(	B—B—2G'VR6öÖWv†W&Rà¢5·FW7EÐ¦fâöÖÆf÷&ÖVEö6öææV7EöVæGö–çEö¶VW5ö—G5÷c÷7VvvW7F–öâ‚’°¢ÆWB7&2ÒÆVFvS¢g7G"Â–ç7C¢g7G'Â°¢f÷&ÖB€¢"2'G&ç67F÷"7&0¢ö'6W'fVB¢÷WBWfVçCÇV–çCÃƒãà¢â¢V–çCÃ3#âFVfVÇB  ¢†öö¶&ÆRV&Æ—6‚‡c¢V–çCÃƒâ¢VÖ—Bö'6W'fVB‡b¢VæBV&Æ—6€¦VæBG&ç67F÷"7&0 §66÷&V&ö&B6–æ°¢6VVâ¢V–çCÃ3#âFVfVÇB ¢†öö¶&ÆRF¶R‡c¢V–çCÃƒâ¢6VVâÒ6VVâ²¢VæBF¶P¦VæB66÷&V&ö&B6–æ° ¦VçbP¢7&2¢7&276—fP¢6–æ²¢6–æ° ¢6öææV7@¢¶VFvWÐ¢VæB6öææV7@¦VæBVçbP §FW7F&Væ6‚F ¢GWB¢F÷ §¶–ç7GÐ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢v—B"7–6ÆW0¢VæB'Và¦VæB–×ÂB"0¢¢Ó° ¢òòF†R6öçG&öÃ¢F†RvVÆÂÖf÷&ÖVBVFvRÆ÷vW'2à¢VÖ—Eö7÷7&2‚g7&2‚'7&2æö'6W'fVBÓâ6–æ²çF¶R"Â"F÷¢R"’“° ¢f÷"†VFvRÂvçB’–â°¢‚'7&2æö'6W'fVBÓâ6–æ²"Â'6–æ²6–æ¶v—F†÷WBÖWF†öB"’À¢‚'7&2Óâ6–æ²çF¶R"Â'6÷W&6R7&6v—F†÷WBâWfVçBf–VÆB"’À¢€¢'7&2æâÓâ6–æ²çF¶R"À¢'6÷W&6R7&2ææF†B—2æ÷Bâ÷WBWfVçF÷'B"À¢’À¢€¢&æ÷7V6‚æö'6W'fVBÓâ6–æ²çF¶R"À¢'F‚6VvÖVçBæ÷7V6†F†B—2æ÷B7V"Ö6ö×öæVçB"À¢’À¢€¢'7&2æö'6W'fVBÓâæ÷7V6‚çF¶R"À¢'F‚6VvÖVçBæ÷7V6†F†B—2æ÷B7V"Ö6ö×öæVçB"À¢’À¢€¢'7&2æö'6W'fVBÓâ6–æ²çF¶R"À¢""Âòò6öçG&öÃ²6¶—VB'’F†RV×G’vçF ¢’À¢Ò°¢–bvçBæ—5öV×G’‚’°¢6öçF–çVS°¢Ð¢ÆWBW'"ÒÆ÷vW%÷7&2‚g7&2†VFvRÂ"F÷¢R"’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fW'"“°¢76W'B†×6ræ6öçF–ç2‡vçB’Â&¶VFvWÖ¢¶×6wÒ"“° ¢òòF†R4ÔRVFvR–ââTä”å5DåD”DTBVçc¢cVÖ—G2æòv—&–æp¢òòF†W&RBÆÂÂ6ò—B7V66VVG2v†W&RF&—"7F–ÆÂ&V¦V7G2âF†—0¢òò—2F†RÆæF–ærF†BÖ¶W2ç’æ÷D–×ÆVÖVçFVF6Æ–ÒöâF†W6P¢òò6—FW2fÇ6Rà¢ÆWBW'"ÒÆ÷vW%÷7&2‚g7&2†VFvRÂ""’’çVçw&öW'"‚“°¢76W'E÷Vç7W÷'FVB‚fW'"“°¢Ð§Ð ¢òòòF†R6–æ²4”täEU$R6†V6·26''’&V¦V7G6ÂæBF†—2FW7B†2†@¢òòòGvòw&öærfW&F–7G2w&—GFVâ–çFò—Bà¢òòð¢òòò—Bf—'7B6–BF†W’&¶VWF†R7VvvW7F–öâFöòÂf÷"F†R6ÖR&V6öâ"0¢òòòF†RVæGö–çB×6†R&×2(	B76W'FVBÂæWfW"ÖV7W&VBÂæB—BöæÇ’WfW ¢òòò'V–ÇBF†R”å5DåD”DTB66RâF†Vâ—B6–B–çfÆ–FÂv†–6‚—2F†P¢òòò÷÷6—FR÷fW"Ö6÷'&V7F–öã¢âVçbæ÷F†–ær–ç7FçF–FW2VÖ—G2æð¢òòòv—&–ærÂæBD„B&öw&Òc6ö×–ÆW2æB'Vç2Fò6ö×ÆWF–öâÂ6ò&¢òòò&öw&ÒW'&÷"VæFW"WfW'’&6¶VæB"—2fÇ6Rà¢òòð¢òòòÖV7W&VBÂ&÷F‚†ÇfW2â–ç7FçF–FVBÂc&—6W2—G2÷vâW'&÷# ¢òòò&6öææV7C¢†öö¶&ÆR6–æ²6–æ²çGvö×W7BF¶RW†7FÇ’öæR–Æö@¢òòò&wVÖVçBÂv÷B""æB&6öææV7C¢†öö¶&ÆR6–æ²6–æ²ç&WF×W7B&WGW&à¢òòòfö–B"(	BF†RVçbf–VÆB†W&R—26–æ¶ÂæBâV&Æ–W"fW'6–öâöbF†—0¢òòòFö2w&÷FR6&âVæ–ç7FçF–FVBÂc'Vç2F†P¢òòò&öw&Òâv÷'7B×VæFW"Ö&Ò÷fW"F†÷6RGvò—2&V¦V7G6(	BcFöW2æ÷@¢òòò–×ÆVÖVçBF†R6öç7G'V7BÂæB6––ær6ò—2†öæW7Bv—F†÷WB6Æ–Ö–æp¢òòòF†R&öw&Ò—2ÖÆf÷&ÖVBWfW'—v†W&Rà¢5·FW7EÐ¦fâö&Eö6öææV7E÷6–æµ÷6–væGW&Uö—5öæ÷Eö–×ÆVÖVçFVEö'•÷cöV—F†W"‚’°¢ÆWB7&2ÒÇ6–æ³¢g7G"ÂÖWF†öC¢g7G'Â°¢f÷&ÖB€¢"2'G&ç67F÷"7&0¢ö'6W'fVB¢÷WBWfVçCÇV–çCÃƒãà¢†öö¶&ÆRV&Æ—6‚‡c¢V–çCÃƒâ¢VÖ—Bö'6W'fVB‡b¢VæBV&Æ—6€¦VæBG&ç67F÷"7&0 §66÷&V&ö&B6–æ°¢6VVâ¢V–çCÃ3#âFVfVÇB §¶ÖWF†öGÐ¦VæB66÷&V&ö&B6–æ° ¦VçbP¢7&2¢7&276—fP¢6–æ²¢6–æ° ¢6öææV7@¢7&2æö'6W'fVBÓâ6–æ²ç·6–æ·Ð¢VæB6öææV7@¦VæBVçbP §FW7F&Væ6‚F ¢GWB¢F÷ ¢F÷¢P¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢v—B"7–6ÆW0¢VæB'Và¦VæB–×ÂB"0¢¢Ó° ¢f÷"‡6–æ²ÂÖWF†öBÂvçB’–â°¢€¢'Gvò"À¢"†öö¶&ÆRGvò†¢V–çCÃƒâÂ#¢V–çCÃƒâ•Æâ6VVâÒ6VVâ²ÆâVæBGvò"À¢'v—F‚"&ÖWFW'2"À¢’À¢€¢'&WB"À¢"†öö¶&ÆR&WB‡c¢V–çCÃƒâ’ÓâV–çCÃƒåÆâ&WGW&âeÆâVæB&WB"À¢'F†B&WGW&ç2fÇVR"À¢’À¢Ò°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g7&2‡6–æ²ÂÖWF†öB’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥&V¦V7G2À¢“°¢76W'B†×6ræ6öçF–ç2‡vçB’Â'¶×6wÒ"“° ¢òòc&VgW6W2F†R–ç7FçF–FVBf÷&Ò(	BF†R&÷rF†BV&ç0¢òò&V¦V7G6à¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2‡6–æ²ÂÖWF†öB’’¢æW‡V7EöW'"‚'c&VgW6W2â–ç7FçF–FVB&B6–æ²6–væGW&R"“°¢òò–âF†Rv†öÆR6ÆW6RÂæ÷B§W7BF†R&Vf—ƒ¢F†R&Òw2FWF–À¢òòV÷FW2cw2v÷&F–ærÂæB&Vf—‚ÖF6‚v÷VÆBæ÷Bæ÷F–6R—@¢òòG&–gF–ærà¢ÆWBcÒf÷&ÖB‚'·cÒ"“°¢76W'B€¢cæ6öçF–ç2‚&6öææV7C¢†öö¶&ÆR6–æ²6–æ²â"¢bb‡cæ6öçF–ç2‚&×W7BF¶RW†7FÇ’öæR–ÆöB&wVÖVçBÂv÷B""¢ÇÂcæ6öçF–ç2‚&×W7B&WGW&âfö–B"’’À¢'·cÒ ¢“° ¢òòæBF†R&÷rF†B'VÆW2–çfÆ–F÷WC¢v—F‚æ÷F†–æp¢òò–ç7FçF–F–ærF†RVçbÂcVÖ—G2æòv—&–æræBF†R&öw&Ò—0¢òòf–æRâD"Ô•"7F–ÆÂ&VgW6W2—BÂv†–6‚—2&VÂ7G&–7FæW72v ¢òò(	B'WBæ÷B6Æ–ÒF†Bæò&6¶VæB'Vç2—Bà¢ÆWBVæ–ç7FçF–FVBÒ7&2‡6–æ²ÂÖWF†öB’ç&WÆ6Vâ‚"F÷¢UÆâ"Â""Â“°¢76W'B€¢Væ–ç7FçF–FVBæ6öçF–ç2‚'F÷¢R"’À¢'F†R–ç7FçF–F–öâ×W7B7GVÆÇ’&RvöæR ¢“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gVæ–ç7FçF–FVB’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'c'Vç2âVæ–ç7FçF–FVB&BVFvS¢¶WÒ"’“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚gVæ–ç7FçF–FVB’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥&V¦V7G2À¢“°¢76W'B†×6ræ6öçF–ç2‡vçB’Â'¶×6wÒ"“°¢Ð§Ð ¢òòò&V6÷&B7FFR—2ÆVvÂ–â$õD‚G&ç67F÷"FV6Æ&F–öâ÷6—F–öç2(	@¢òòò&÷fRv†Vâ7F—fVæB–ç6–FR—Bâc6ö×–ÆW2V—F†W"Â6ò6Æ÷6–æp¢òòòöæÇ’F†R÷WFW"öæRv÷VÆBÆVfR†ÆbfVGW&Rà¢5·FW7EÐ¦fâ÷&V6÷&Eöf–VÆEöÆ÷vW'5ö–åö&÷F…÷G&ç67F÷%÷÷6—F–öç2‚’°¢f÷"FV6Â–â°¢òò&÷fRv†Vâ7F—fVà¢"7W"¢&VEÆåÆâv†Vâ7F—fUÆâ"À¢òò–ç6–FR—Bà¢%Æâv†Vâ7F—fUÆâ7W"¢&VEÆâ"À¢Ò°¢ÆWB7ÒVÖ—Eö7÷7&2‚ff÷&ÖB€¢"2'7G'V7B&V@¢Fr¢V–çCÃƒâFVfVÇB ¦VæB7G'V7B&V@ §G&ç67F÷"‡@¢GWB¢F÷ §¶FV6ÇÒ†öö¶&ÆRvò‚¢7W"çFrÒP¢v—B7–6ÆP¢VæBvð¢VæBv†Và¦VæBG&ç67F÷"‡@ §FW7F&Væ6‚F ¢GWB¢F÷ ¢‡B¢‡B7F—fP¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢‡BæGWBÒGW@¢‡Bævò‚¢76W'B‡Bæ7W"çFrÓÒRVÇ6Rf–Â‚'Fr"¢VæB'Và¦VæB–×ÂB"0¢’“°¢76W'B†7æ6öçF–ç2‚$&VB7W'·Ó²"’Â'¶7Ò"“°¢76W'B†7æ6öçF–ç2‚'6VÆe÷7FFRæ7W"çFrÒS²"’Â'¶7Ò"“°¢Ð§Ð ¢òòòF†R&V6÷&B'&æ6‚Ö¶W2F†R6ÖRGWÆ–6FRÖæÖR6†V6²F†R66Æ ¢òòò'&æ6‚FöW2âv—F†÷WB—BÂ7W"¢V–çCÃ3#æÇW27W"¢&VFVÖ—GFV@¢òòòEtò7W&ÖVÖ&W'2–çFòF†R7FFR7G'V7Bà¢5·FW7EÐ¦fâöGWÆ–6FU÷G&ç67F÷%÷7FFUöf–VÆEö—5÷&V¦V7FVB‚’°¢ÆWBW'"ÒÆ÷vW%÷7&2€¢"2'7G'V7B&V@¢Fr¢V–çCÃƒâFVfVÇB ¦VæB7G'V7B&V@ §G&ç67F÷"‡@¢GWB¢F÷ ¢7W"¢V–çCÃ3#âFVfVÇB ¢7W"¢&V@ ¢v†Vâ7F—fP¢†öö¶&ÆRvò‚¢v—B7–6ÆP¢VæBvð¢VæBv†Và¦VæBG&ç67F÷"‡@ §FW7F&Væ6‚F ¢GWB¢F÷ ¢‡B¢‡B7F—fP¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢‡BæGWBÒGW@¢‡Bævò‚¢VæB'Và¦VæB–×ÂB"2À¢¢çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fW'"“°¢76W'B€¢×6ræ6öçF–ç2‚&FV6Æ&W27FFRf–VÆB7W&Ö÷&RF†âöæ6R"’À¢'¶×6wÒ ¢“°§Ð ¢òòò&V6÷&B×G—VBf–VÆBöâG&ç67F÷"&V6†VBF‡&÷Vv‚âVçf6öÖW0¢òòòF‡&÷Vv‚F†R6ö×öæVçBÖf–VÆBÖ6†–æW'’â—B—2W'6—7FVçB'’×fÇVP¢òòò7FFRÂæ÷B6V6öæBEUB†æFÆRÂæB×W7B&VÖ–âW6&ÆRg&öÒF†P¢òòòG&ç67F÷"ÖWF†öB&öG’à¢5·FW7EÐ¦fâåöVçeö†VÆE÷G&ç67F÷%÷&V6÷&Eöf–VÆEöÆ÷vW'2‚’°¢ÆWB7&2Ò"2'7G'V7B&V@¢Fr¢V–çCÃƒâFVfVÇB ¦VæB7G'V7B&V@ §G&ç67F÷"‡@¢GWB¢F÷ ¢7W"¢&V@ ¢v†Vâ7F—fP¢†öö¶&ÆRvò‚¢7W"çFrÒP¢VæBvð¢VæBv†Và¦VæBG&ç67F÷"‡@ ¦VçbP¢‡B¢‡B7F—fP¦VæBVçbP §FW7F&Væ6‚F ¢GWB¢F÷ ¢F÷¢P¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢v—B"7–6ÆW0¢VæB'Và¦VæB–×ÂB"3°¢ÆWB7ÒVÖ—Eö7÷7&2‡7&2“°¢76W'B€¢7æ6öçF–ç2‚$&VB7W'·Ó²"’À¢'&V6÷&BÖVÖ&W"—2VÖ—GFVC¥Æç¶7Ò ¢“°¢76W'B€¢7æ6öçF–ç2‚'6VÆbæ7W"çFrÒS²"’À¢&ÖWF†öBw&—FR¶VW2F†R&V6÷&B&V6V—fW#¥Æç¶7Ò ¢“°§Ð ¢òòòvF6†FövöâG&ç67F÷"—2äõBcW66R†F6‚ÂæBF†R&V6öà¢òòòöæÇ’6†÷w2W–b–÷R6†V6²v†WF†W"F†RVÖ—GFVB6öFRWfW"%Tå2âc¢òòòVÖ—G26ö×ÆWFRÅCå÷vF6†FövÆÖ&F(	B&R÷÷7B†öö²fV7F÷'2ÂF†P¢òòòÖ…ö–FÆV6†V6²v–ç7BöÆ7Eö–åö7–6ÆVööÆ7Eö÷WEö7–6ÆVÂF†Rd”À¢òòòÆ–æRÂF†RW'&÷"'V×(	BæBF†VâæWfW"6ÆÇ2—Bà¢òòð¢òòòF†R6öçG&öÂF†BÖ¶W2F†—2c'Vr&F†W"F†âvÆö&ÂFW6–vã¢à¢òòòtTåBvF6†FörDôU2vWB6ÆÂ6—FR†&öGV6W%÷vF6†För…÷F"ç&öB– ¢òòò–ç6–FRW&–öF–26Æ÷7W&RÂ–âvF6†Föu÷V–W66U÷FW7F’âG&ç67F÷ ¢òòòvF6†FörvWG2æöæRÂ6ò—B6ö×–ÆW2æB6–ÆVçFÇ’æWfW"f—&W2à¢òòð¢òòòÆÂf—fRvF6†För6—FW26''’—B(	BVæ&÷VæBÂ&÷VæB×FòF&vWBÂæ@¢òòò–æ—F–F÷"×6–FRââV&Æ–W"72&V6Æ76–f–VBöæÇ’F†RGvòVæ&÷Væ@¢òòòöæW2öâF†R&VÆ–VbF†BF†R÷F†W'2æVVFVB6–&Æ–ær'W2f–ÆRFð¢òòò&V6ƒ²F†W’Fòæ÷B†'W2(
bVæB'W66—G2–æÆ–æR&W6–FR&÷VæB×Fð¢òòòG&ç67F÷"–âFÖöVæv–æU÷FÆÕ÷F&vWE÷FW7F’ÂæB6–ævÆRÖf–ÆR&ö&W0¢òòòöb&÷F‚&÷VæBfÆf÷'26†÷rF†R6ÖRFVf–æVBÖæWfW"Ö6ÆÆVBÆÖ&Fà¢5·FW7EÐ¦fâ÷G&ç67F÷%÷vF6†FöuöFöW5öæ÷E÷ö–çEöE÷c‚’°¢ÆWB7&2ÒÇvE÷÷3¢g7G'Â°¢ÆWB†÷WFW"Â–ææW"’ÒÖF6‚vE÷÷2°¢&÷WFW""Óâ…tEô$Äô4²Â""’À¢òÓâ‚""ÂtEô$Äô4²’À¢Ó°¢f÷&ÖB€¢"2'G&ç67F÷"‡@¢GWB¢F÷ ¢â¢V–çCÃ3#âFVfVÇB §¶÷WFW'Ð¢v†Vâ7F—fP§¶–ææW'Ò†öö¶&ÆRvò‚¢âÒâ²¢v—B7–6ÆP¢VæBvð¢VæBv†Và¦VæBG&ç67F÷"‡@ §FW7F&Væ6‚F ¢GWB¢F÷ ¢‡B¢‡B7F—fP¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢‡BæGWBÒGW@¢‡Bævò‚¢VæB'Và¦VæB–×ÂB"0¢¢Ó°¢6öç7BtEô$Äô4³¢g7G"Ò"vF6†FöuÆâW&–öBR7–6ÆW5ÆâÖ…ö–FÆR7–6ÆW5ÆâÆör†–æfòÂÂ'vFöuÂ"•ÆâVæBvF6†FöuÆâ#° ¢f÷"÷2–â²&÷WFW""Â'v†Vâ%Ò°¢ÆWBW'"ÒÆ÷vW%÷7&2‚g7&2‡÷2’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2“°¢76W'B†×6ræ6öçF–ç2‚'G&ç67F÷"‡FvF6†Föw2"’Â'·÷7Ó¢¶×6wÒ"“°¢76W'B€¢×6ræ6öçF–ç2‚&æWfW"66†VGVÆW2—B"’À¢'F†RF–væ÷7F–2×W7B6’t…’c—2æ÷Bv’÷WC¢¶×6wÒ ¢“°¢Ð§Ð ¢òòòF†R6öçG&öÂÂ–ææVB6òF†R6Æ–Ò&÷fR7F—2G'VRâ—B×W7B6†V6°¢òòò¢§cw2¢¢÷WGWBÂæ÷BF&—"w3¢F†R6Æ–Ò—2&÷WBv†B7÷F&VÖ—G2à¢òòòæB—B×W7B6÷VçB&VÂ4ÄÂÆ–æW2(	BvF6†FörF†B—2æWfW ¢òòò66†VGVÆVB7F–ÆÂVÖ—G2—G2÷&Vö÷÷7FfV7F÷"FV6Æ&F–öç2æBGvð¢òòò–çFW&æÂ†öö²Æö÷2Â6ò&&R&æÖRV'2"6÷VçB—26F—6f–VB'¢òòòW†7FÇ’F†RFVB6†RF†—2FW7BW†—7G2FòF—7F–æwV—6‚g&öÒà¢5·FW7EÐ¦fâåövVçE÷vF6†Föuö—5÷66†VGVÆVE÷VæFW%÷c‚’°¢ÆWBÖW&vVBÒÖW&vVE÷7&2‚ff—‡GW&R‚'vF6†Föu÷V–W66U÷FW7Bæ†&2"’“°¢ÆWB7Ò7÷F#£¦VÖ—B‚fÖW&vVB’æW‡V7B‚'cVÖ—G2"“° ¢ÆWB—5ö6ÆÂÒÆÃ¢bg7G'Â°¢Âæ6öçF–ç2‚%&öGV6W%÷vF6†För‚"¢bbÂæ6öçF–ç2‚&WFò&öGV6W%÷vF6†För"¢bbÂæ6öçF–ç2‚%&öGV6W%÷vF6†Föu÷&R"¢bbÂæ6öçF–ç2‚%&öGV6W%÷vF6†Föu÷÷7B"¢Ó°¢ÆWB6ÆÇ3¢fV3Âg7G#âÒ7æÆ–æW2‚’æf–ÇFW"†—5ö6ÆÂ’æ6öÆÆV7B‚“°¢76W'EöW€¢6ÆÇ2æÆVâ‚’À¢À¢&âvVçBvF6†För—24ÄÄTBW†7FÇ’öæ6RÂg&öÒ—G2W&–öF–26Æ÷7W&S²À¢v÷B¶6ÆÇ3£÷Ò ¢“° ¢òòæBF†RG&ç67F÷"fÆf÷"ÂF‡&÷Vv‚F†R6ÖRVÖ—GFW"Â†2æöæR(	@¢òòv†–6‚—2F†Rv†öÆR&6—2f÷"F†R&V6Æ76–f–6F–öâ&÷fRà¢ÆWBÖW&vVBÒÖW&vVE÷7&2€¢"2'G&ç67F÷"‡@¢GWB¢F÷ ¢â¢V–çCÃ3#âFVfVÇB  ¢vF6†Föp¢W&–öBR7–6ÆW0¢Ö…ö–FÆR7–6ÆW0¢Æör†–æfòÂ'vFör"¢VæBvF6†Föp ¢v†Vâ7F—fP¢†öö¶&ÆRvò‚¢âÒâ²¢v—B7–6ÆP¢VæBvð¢VæBv†Và¦VæBG&ç67F÷"‡@ §FW7F&Væ6‚F ¢GWB¢F÷ ¢‡B¢‡B7F—fP¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢‡BæGWBÒGW@¢‡Bævò‚¢VæB'Và¦VæB–×ÂB"2À¢“°¢ÆWB7Ò7÷F#£¦VÖ—B‚fÖW&vVB’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢7æ6öçF–ç2‚&WFò‡E÷vF6†För"’À¢'cFVf–æW2F†RG&ç67F÷"vF6†För&öG“¥Æç¶7Ò ¢“°¢ÆWB6ÆÇ3¢fV3Âg7G#âÒ7 ¢æÆ–æW2‚¢æf–ÇFW"‡ÆÇÂ°¢Âæ6öçF–ç2‚%‡E÷vF6†För‚"¢bbÂæ6öçF–ç2‚&WFò‡E÷vF6†För"¢bbÂæ6öçF–ç2‚%‡E÷vF6†Föu÷&R"¢bbÂæ6öçF–ç2‚%‡E÷vF6†Föu÷÷7B"¢Ò¢æ6öÆÆV7B‚“°¢76W'B€¢6ÆÇ2æ—5öV×G’‚’À¢.(
fæBæWfW"6ÆÇ2—B(	B–bF†—2WfW"f—&W2ÂF†R&V6Æ76–f–6F–öâ—27FÆRÀ¢æBF†RF–væ÷7F–2×W7Bvò&6²FòVç7W÷'FVF²v÷B¶6ÆÇ3£÷Ò ¢“°§Ð ¢òòò6öææV7F&Æö6²öâG&ç67F÷"—2æ÷BcW66R†F6‚V—F†W"À¢òòòæBF†RWf–FVæ6R—24ôåE$ôÂD”db&F†W"F†âw&W¢cw2VÖ—GFV@¢òòò2²²—2'—FRÖ–FVçF–6Âv—F‚æBv—F†÷WBF†R&Æö6²à¢òòð¢òòòæ÷FRv†BF†BFöW2äõB&W7Böââc†2fW&&F–ÒfÆÆ&6²f÷"à¢òòòVç&W6öÇf&ÆRVçbVFvR(	B—BVÖ—G2F†RF‚2w&—GFVâæBÆWG2F†P¢òòò2²²6ö×–ÆW"6ö×Æ–â(	B6ò&&6¶VæBF†B&W6öÇfVBç—F†–ærv÷VÆ@¢òòò†fRW'&÷&VB"—2fÇ6RÂæBF†R'—FRÖ–FVçF—G’—2æ÷BW‡Æ–æVB'¢òòòF†RVFvR&V–æræöç6Vç6RâF†R÷6—F—fRæ6†÷"&VÆ÷r—2v†BÖ¶W2F†P¢òòò6ö×&—6öâÖVâ6öÖWF†–æs¢F†R4ÔRVFvR6†R–ç6–FRâVçfFöW0¢òòò6†ævRcw2÷WGWBâF†RVÖ—GFW"v—&W2F†—2VFvRv†Vâ—B÷vç2—BÂæ@¢òòòG&÷2—BöâG&ç67F÷"à¢5·FW7EÐ¦fâ÷G&ç67F÷%ö6öææV7Eö&Æö6µöFöW5öæ÷E÷ö–çEöE÷c‚’°¢ÆWB7&2ÒÆ6öæåö÷WFW#¢g7G"Â6öæåö–ææW#¢g7G'Â°¢f÷&ÖB€¢"2'G&ç67F÷"‡@¢GWB¢F÷ ¢â¢V–çCÃ3#âFVfVÇB §¶6öæåö÷WFW'Ð¢v†Vâ7F—fP§¶6öæåö–ææW'Ò†öö¶&ÆRvò‚¢âÒâ²¢v—B7–6ÆP¢VæBvð¢VæBv†Và¦VæBG&ç67F÷"‡@ §FW7F&Væ6‚F ¢GWB¢F÷ ¢‡B¢‡B7F—fP¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢‡BæGWBÒGW@¢‡Bævò‚¢VæB'Và¦VæB–×ÂB"0¢¢Ó°¢6öç7BõUDU#¢g7G"Ò"6öææV7EÆâæ"Óâ2æEÆâVæB6öææV7EÆâ#°¢6öç7B”ääU#¢g7G"Ò"6öææV7EÆâæ"Óâ2æEÆâVæB6öææV7EÆâ#° ¢òòÆÂf—fR6—FW3¢Væ&÷VæB†&÷F‚FV6Æ&F–öâ÷6—F–öç2’Â&÷VæB×Fð¢òòF&vWBÂæB–æ—F–F÷"×6–FRâ6Æ–Ö–ær&f—fR6—FW2"v†–ÆP¢òòW†W&6—6–ærGvò—2F†R÷fW&6Æ–ÒF†—27vVW¶VW2†f–ærFòVæFòà¢f÷"†÷WFW"Â–ææW"’–â²„õUDU"Â""’Â‚""Â”ääU"•Ò°¢ÆWBW'"ÒÆ÷vW%÷7&2‚g7&2†÷WFW"Â–ææW"’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2“°¢76W'B†×6ræ6öçF–ç2‚&6öææV7B&Æö6·2"’Â'¶×6wÒ"“°¢76W'B†×6ræ6öçF–ç2‚&VÖ—G2äõD„”ärf÷"—B"’Â'¶×6wÒ"“°¢Ð¢f÷"†Æ&VÂÂ&öw&Ò’–â°¢‚&&÷VæB×FòF&vWB"Â$õTäEõD$tUEô4ôääT5B’À¢‚&–æ—F–F÷"×6–FR"Â$õTäEô”ä•D”Dõ%ô4ôääT5B’À¢Ò°¢ÆWBW'"ÒÆ÷vW%÷7&2‡&öw&Ò’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2“°¢76W'B†×6ræ6öçF–ç2‚&6öææV7B&Æö6·2"’Â'¶Æ&VÇÓ¢¶×6wÒ"“°¢òòF†RVçfGf–6R—2FVBVæBf÷"&÷VæBFöG&ç67F÷"(	@¢òò&÷F‚&6¶VæG2&V¦V7BöæR2âVçb7V"Öf–VÆBà¢76W'B€¢×6ræ6öçF–ç2‚'v—&RF†RVæGö–çG2g&öÒâVçf"’À¢'¶Æ&VÇÓ¢F†R7VvvW7FVBv÷&¶&÷VæB×W7B&R&V6†&ÆS¢¶×6wÒ ¢“°¢Ð ¢òòF†R6öçG&öÂÂ–â$õD‚FV6Æ&F–öâ÷6—F–öç2(	BF†W’&RF—7F–æ7Bc¢òòF‡2†–æ6ÇVFUö7F—fV’Â6òöæRFöW2æ÷B6÷fW"F†R÷F†W"à¢ÆWBv—F†÷WBÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2‚""Â""’’’æW‡V7B‚'cVÖ—G2"“°¢f÷"†Æ&VÂÂ÷WFW"Â–ææW"’–â²‚&÷WFW""ÂõUDU"Â""’Â‚'v†Vâ7F—fR"Â""Â”ääU"•Ò°¢ÆWBv—F‚Ò7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2†÷WFW"Â–ææW"’’’æW‡V7B‚'cVÖ—G2"“°¢76W'EöW€¢v—F‚Âv—F†÷WBÀ¢'¶Æ&VÇÓ¢cVÖ—G2–FVçF–6Â2²²v—F‚æBv—F†÷WBG&ç67F÷"6öææV7F ¢“°¢Ð¢òò÷6—F—fRæ6†÷#¢v—F†÷WBF†—2F†RWVÆ—G’&÷fRFVvVæW&FW2Fò¢òòFWFöÆöw’F†RF’c7F÷2VÖ—GF–ærF†RG&ç67F÷"BÆÂà¢76W'B€¢v—F†÷WBæ6öçF–ç2‚'7G'V7B‡B"’bbv—F†÷WBæ6öçF–ç2‚%‡Eövò"’À¢'F†R6öçG&öÂ6ö×&W2&VÂ÷WGWBÂæ÷BGvòV×G’7G&–æw3¥Æç·v—F†÷WGÒ ¢“° ¢òò(
fæBF†RæVvF—fRæ6†÷#¢F†R6ÖRVFvR–ââVçfDôU26†ævP¢òòcw2÷WGWBÂ6ò'—FRÖ–FVçF—G’—2&÷W'G’öbF†RG&ç67F÷ ¢òòF‚Âæ÷BöbF†RVFvRà¢ÆWBVçe÷7&2ÒÆ6öæã¢g7G'Â°¢f÷&ÖB€¢"2'G&ç67F÷"7&0¢ö'6W'fVB¢÷WBWfVçCÇV–çCÃƒãà¢†öö¶&ÆRV&Æ—6‚‡c¢V–çCÃƒâ¢VÖ—Bö'6W'fVB‡b¢VæBV&Æ—6€¦VæBG&ç67F÷"7&0 §66÷&V&ö&B6–æ°¢6VVâ¢V–çCÃ3#âFVfVÇB ¢†öö¶&ÆRF¶R‡c¢V–çCÃƒâ¢6VVâÒ6VVâ²¢VæBF¶P¦VæB66÷&V&ö&B6–æ° ¦VçbP¢7&2¢7&276—fP¢6–æ²¢6–æ°§¶6öæçÖVæBVçbP §FW7F&Væ6‚F ¢GWB¢F÷ ¢F÷¢P¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢v—B"7–6ÆW0¢VæB'Và¦VæB–×ÂB"0¢¢Ó°¢ÆWBVçe÷v—F‚Ð¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fVçe÷7&2‚"6öææV7EÆâ7&2æö'6W'fVBÓâ6–æ²çF¶UÆâVæB6öææV7EÆâ"’’¢æW‡V7B‚'cVÖ—G2"“°¢ÆWBVçe÷v—F†÷WBÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fVçe÷7&2‚""’’’æW‡V7B‚'cVÖ—G2"“°¢76W'EöæR€¢Vçe÷v—F‚ÂVçe÷v—F†÷WBÀ¢&âVçf6öææV7BVFvRDôU26†ævRcw2÷WGWB(	BF†B6öçG&7B—2v†BÀ¢Ö¶W2F†RG&ç67F÷"'—FRÖ–FVçF—G’ÖVæ–ævgVÂ ¢“°§Ð ¦6öç7B$õTäEõD$tUEô4ôääT5C¢g7G"Ò"2&'W2ÖVÔ'W0¢FÆÕöÖWF†öB&VB†FG#¢V–çCÃ3#â’ÓâV–çCÃ3#ã¢&Æö6¶–æs°¦VæB'W2ÖVÔ'W0 §G&ç67F÷"ÖVÕF&vWB&÷VæBFòÖVÔ'W0¢â¢V–çCÃ3#âFVfVÇB  ¢6öææV7@¢æ"Óâ2æ@¢VæB6öææV7@ ¢F‡&VB'W2ç&VB†FG"¢âÒâ²¢&WGW&âp¢VæBF‡&V@¦VæBG&ç67F÷"ÖVÕF&vW@ §FW7F&Væ6‚F ¢GWB¢F÷ ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢ÆWBÖVÒ¢ÖVÔ'W2Ò&–æBGW@¢ÆWBF&vWB¢ÖVÕF&vWB76—fRÒ&–æBÖVÐ¢'Và¢v—B"7–6ÆW0¢VæB'Và¦VæB–×ÂB"3° ¦6öç7B$õTäEô”ä•D”Dõ%ô4ôääT5C¢g7G"Ò"2&'W2't'W0¢†æG6†¶Uö6†ææVÂs¢6VæB¶–æC¢fÆ–E÷&VG¢FF¢V–çCÃƒà¢VæB†æG6†¶Uö6†ææVÂp¦VæB'W2't'W0 §G&ç67F÷"'tG'b&÷VæBFò't'W0¢â¢V–çCÃ3#âFVfVÇB  ¢6öææV7@¢æ"Óâ2æ@¢VæB6öææV7@ ¢†öö¶&ÆRvò‚¢âÒâ²¢v—B7–6ÆP¢VæBvð¦VæBG&ç67F÷"'tG'` §FW7F&Væ6‚F ¢GWB¢F÷ ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢ÆWB"¢'t'W2Ò&–æBGW@¢ÆWBG'b¢'tG'b7F—fRÒ&–æB ¢'Và¢G'bævò‚¢VæB'Và¦VæB–×ÂB"3° ¢òòòF‡&VB'W2ãÆÓâ‚âââ–&W7öæFW"öââTä$õTäBG&ç67F÷"—0¢òòòF—66&FVB'’c(	B—G22²²—2'—FRÖ–FVçF–6Âv—F‚æBv—F†÷WBF†P¢òòò—FVÒâF†RäTtD•dRæ6†÷"—2v†BÖ¶W2F†B7FFVÖVçB&÷WBF†P¢òòòVæ&÷VæBF‚&F†W"F†â&÷WBF&vWBF‡&VG2–âvVæW&Ã¢F†R6ÖP¢òòò—FVÒöâ&÷VæBFöG&ç67F÷"6†ævW2cw2÷WGWB7V'7FçF–ÆÇ’À¢òòò6òF†RVÖ—GFW"6W'fW2F&vWBF‡&VG2v†W&R—B÷vç2F†VÒà¢5·FW7EÐ¦fâ÷F&vWE÷F‡&VEööåöå÷Væ&÷VæE÷G&ç67F÷%ö—5öF—66&FVEö'•÷c‚’°¢ÆWB7&2ÒÇF‡&VC¢g7G'Â°¢f÷&ÖB€¢"2'G&ç67F÷"‡@¢GWB¢F÷ ¢â¢V–çCÃ3#âFVfVÇB §·F‡&VGÐ¢v†Vâ7F—fP¢†öö¶&ÆRvò‚¢âÒâ²¢v—B7–6ÆP¢VæBvð¢VæBv†Và¦VæBG&ç67F÷"‡@ §FW7F&Væ6‚F ¢GWB¢F÷ ¢‡B¢‡B7F—fP¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢‡BæGWBÒGW@¢‡Bævò‚¢VæB'Và¦VæB–×ÂB"0¢¢Ó°¢6öç7BD…$TC¢g7G"Ò"F‡&VB'W2ç&VB†FG"•ÆââÒâ²Æâ&WGW&âuÆâVæBF‡&VEÆâ#° ¢ÆWBW'"ÒÆ÷vW%÷7&2‚g7&2…D…$TB’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2“°¢76W'B†×6ræ6öçF–ç2‚%DÄÒF&vWBF‡&VG2"’Â'¶×6wÒ"“° ¢òò6öçG&öÂÂv—F‚÷6—F—fRæ6†÷"6òWVÆ—G’6ææ÷B72f7V÷W6Ç’à¢ÆWBv—F‚Ò7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2…D…$TB’’’æW‡V7B‚'cVÖ—G2"“°¢ÆWBv—F†÷WBÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2‚""’’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢v—F†÷WBæ6öçF–ç2‚'7G'V7B‡B"’bbv—F†÷WBæ6öçF–ç2‚%‡Eövò"’À¢'F†R6öçG&öÂ6ö×&W2&VÂ÷WGWC¥Æç·v—F†÷WGÒ ¢“°¢76W'EöW‡v—F‚Âv—F†÷WBÂ'cF—66&G2F†RF‡&VBöââVæ&÷VæBG&ç67F÷""“° ¢òòæVvF—fRæ6†÷#¢F†R4ÔR—FVÒöâ&÷VæB×FòG&ç67F÷"DôU0¢òò6†ævRcw2÷WGWBâv—F†÷WBF†—2ÂF†RWVÆ—G’&÷fRv÷VÆB&P¢òò6öç6—7FVçBv—F‚cæ÷B–×ÆVÖVçF–ærF&vWBF‡&VG2BÆÂà¢ÆWB&÷VæBÒÆ—FVÓ¢g7G'Â°¢f÷&ÖB€¢"2&'W2ÖVÔ'W0¢FÆÕöÖWF†öB&VB†FG#¢V–çCÃ3#â’ÓâV–çCÃ3#ã¢&Æö6¶–æs°¦VæB'W2ÖVÔ'W0 §G&ç67F÷"ÖVÕF&vWB&÷VæBFòÖVÔ'W0¢â¢V–çCÃ3#âFVfVÇB §¶—FV×Ð¦VæBG&ç67F÷"ÖVÕF&vW@ §FW7F&Væ6‚F ¢GWB¢F÷ ¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢ÆWBÖVÒ¢ÖVÔ'W2Ò&–æBGW@¢ÆWBF&vWB¢ÖVÕF&vWB76—fRÒ&–æBÖVÐ¢'Và¢v—B"7–6ÆW0¢VæB'Và¦VæB–×ÂB"0¢¢Ó°¢ÆWBÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f&÷VæB…D…$TB’’’æW‡V7B‚'cVÖ—G2"“°¢ÆWB"Ò7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f&÷VæB€¢"†öö¶&ÆRvò‚•ÆââÒâ²ÆâVæBvõÆâ"À¢’’¢æW‡V7B‚'cVÖ—G2"“°¢76W'EöæR€¢Â"À¢'cDôU26W'fRF&vWBF‡&VG2öâ&÷VæB×FòG&ç67F÷"(	BF†B6öçG&7B—2À¢v†BÖ¶W2F†RVæ&÷VæB'—FRÖ–FVçF—G’ÖVæ–ævgVÂ ¢“°§Ð  ¢òòòâFG&Ö–ç7Fæ6RÆ&6Sæò6—¦Væ÷rdôÄE2F‡&÷Vv‚F†Rf–ÆP¢òòò6öç7FçBF&ÆRÂ6ò$4R²ƒÖVç2v†B—B6—2âF†—2—2öæRö`¢òòòF†RÆ6W2D"Ô•"—2†VBöbc&F†W"F†â6F6†–ærW¢cw2÷và¢òòòföÆB66WG2ç’W‡&W76–öâæB––VÆG2¤U$òÂVÖ—GF–ær&Vv—7FW ¢òòòw&—FRv–ç7B&6RâF†RFW7F&Væ6‚ö¶W2F†Rw&öærFG&W72æ@¢òòò&W÷'G2æ÷F†–ærà¢òòð¢òòò&V6W6RF†RGvò&6¶VæG2F—6w&VR†W&R(	BöæRöbF†VÒ—2w&öær(	B¢òòò&öw&ÒW6–ærF†—2—2FVÆ–&W&FVÇ’'6VçBg&öÐ¢òòòFW7G2÷F&—%öWV—eöf—‡GW&W2çG‡FÂæBcw2&V†f–÷W"—2–ææVB&VÆ÷p¢òòò6ògWGW&R6†ævRFò—B—26Vv‡B&F†W"F†â77VÖVBà¢5·FW7EÐ¦fâö6öç7EöFG&Öö&6Uö÷%÷6—¦UöföÆG2‚’°¢òò6öçG&öÃ¢F†RVæ×WFFVB6†RÆ÷vW'2Â6òF†R76W'F–öç2&VÆ÷r&P¢òò&V6†–ærF†RFG&Ö&Ò&F†W"F†âG&—–ærâV&Æ–W"vFRà¢ÆWB¦W&òÒVÖ—Eö7÷7&2‚fFG&Ö÷7&2‚$ƒ6—¦Rƒ3"’“° ¢òòF†RföÆBv—fW2'—FRÖ–FVçF–6Â÷WGWBFòF†RÆ—FW&Â—B6ö×WFW>(
`¢ÆWBÆ—FW&ÂÒVÖ—Eö7÷7&2‚fFG&Ö÷7&2‚$ƒc6—¦Rƒ3"’“°¢f÷"7VÆÆ–ær–â°¢$ƒS²ƒ6—¦Rƒ3"À¢$"6—¦Rƒ3"À¢$"6—¦R2"À¢$ƒc6—¦R2"À¢Ò°¢ÆWB7&2Òf÷&ÖB€¢&6öç7B"ÒƒcÆæ6öç7B2Òƒ3ÆåÆç·Ò"À¢FG&Ö÷7&2‡7VÆÆ–ær¢“°¢76W'EöW€¢VÖ—Eö7÷7&2‚g7&2’À¢Æ—FW&ÂÀ¢&·7VÆÆ–æwÖ×W7BÆ÷vW"W†7FÇ’Æ–¶Rƒc6—¦Rƒ3 ¢“°¢Ð¢òò(
fæBF†R&6RdÅTR—2vVçV–æVÇ’W6VBÂ6òF†BWVÆ—G’—2F†P¢òòföÆBv÷&¶–ær&F†W"F†âF†R&6R&V–ærG&÷VBà¢76W'EöæR‡¦W&òÂÆ—FW&ÂÂ&F–ffW&VçB&6R×W7B6†ævRF†R÷WGWB"“° ¢òòF†RföÆFVB&6R&V6†W2F†RVÖ—GFVBDE$U52Â&R×6†–gFVC¢F†R4¢òò&Vv—7FW"6—G2Böfg6WBƒ‚Â6ò&6RƒcWG2—BBƒs‚Ò#à¢76W'B€¢Æ—FW&Âæ6öçF–ç2‚$†–Ä†VÇW%÷w&—FRƒ#Â"’À¢'F†RföÆFVB&6R—2&R×6†–gFVB–çFòF†R&Vv—7FW"FG&W73¥Æç¶Æ—FW&ÇÒ ¢“° ¢òò6—¦VF†BföÆG27F–ÆÂfVVG2F†R÷fW&Æ6†V6²Â6òF†R6†V6°¢òòF†BW†—7G2Fò6F6‚&BÖ†2æ÷B&VVâV–WFÇ’F—6&ÆVBà¢òò2Òƒƒ'Vç2F†Rf—'7B–ç7Fæ6Rw2v–æF÷r÷fW"F†R6V6öæBw2&6P¢òòBƒ3(	B6—¦RföÆF–ærFòv÷VÆB6öÆÆ6RF†Rv–æF÷ræBÆW@¢òòF†—2F‡&÷Vv‚6–ÆVçFÇ’Âv†–6‚—2W†7FÇ’v†BcFöW2à¢ÆWBW'"ÒÆ÷vW%÷7&2‚ff÷&ÖB€¢&6öç7B2ÒƒƒÆåÆç·Ò"À¢FG&Ö÷7&2‚$ƒ6—¦R2"¢’¢çVçw&öW'"‚“°¢76W'B€¢f÷&ÖB‚'¶W'#£÷Ò"’æ6öçF–ç2‚&÷fW&Æ"’À¢&föÆFVB6—¦R×W7B7F–ÆÂ÷fW&ÆÖ6†V6³¢¶W'#£÷Ò ¢“° ¢òòcw26–FRÂ–ææVBöâF†RVÖ—GFVBFG&W72&F†W"F†âv†öÆRf–ÆW0¢òò(	BæBƒ&RF†R6ÖRfÇVR7VÆÆVBGvòv—2ÂæBF†P¢òò6Æ–Ò—2&÷WBF†RfÇVRà¢ÆWBcöFG%ööbÒÆ–ç7C¢g7G"Â&S¢g7G'Â°¢ÆWB7Ð¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚ff÷&ÖB‚'·&W×·Ò"ÂFG&Ö÷7&2†–ç7B’’’’æW‡V7B‚'cVÖ—G2"“°¢ÆWBBÒ7 ¢æf–æB‚$†–Ä†VÇW%÷w&—FR††VÇW"Â‚"¢çVçw&ö÷%öVÇ6R‡ÇÂæ–2‚&æòFG&Öw&—FR–âc÷WGWC¥Æç¶7Ò"’“°¢ÆWB&W7BÒf7¶B²$†–Ä†VÇW%÷w&—FR††VÇW"Â‚"æÆVâ‚’âåÓ°¢&W7E²âç&W7Bæf–æB‚r’r’æW‡V7B‚'F†RFG&W72W‡&W76–öâ6Æ÷6W2"•ÒçFõ÷7G&–ær‚¢Ó°¢òò&÷F‚7VÆÆ–æw2D"Ô•"æ÷rföÆG2FòƒcÆæBBVæFW"c(
`¢f÷"†–ç7BÂ&R’–â°¢‚$ƒS²ƒ6—¦Rƒ3"Â""’À¢‚$"6—¦Rƒ3"Â&6öç7B"ÒƒcÆåÆâ"’À¢Ò°¢76W'EöW€¢cöFG%ööb†–ç7BÂ&R’ç&WÆ6R‚#ƒ"Â#"’À¢#²ƒ‚"À¢'cföÆG2¶–ç7GÖFò ¢“°¢Ð¢òò(
gv†W&RF†R&6RF†W’6ö×WFRv÷VÆB†fRWBF†Rw&—FRBƒs‚à¢òòF†B6öçG&7B—2v†BÖ¶W2cw2föÆB6–ÆVçB'Vr&F†W"F†â¢òò†&ÖÆW727VÆÆ–ærF–ffW&Væ6Rà¢76W'EöW‡cöFG%ööb‚$ƒc6—¦Rƒ3"Â""’Â#ƒc²ƒ‚"“°§Ð ¢òòòF†RföÆG2&÷fR66WB6öç7FçBW‡&W76–öç2Âæ÷B&&—G&'’öæW2(	@¢òòòæBF†RF‡&VRv—2öæR6âf–ÂFò&RW6&ÆR&RF‡&VRF–ffW&Vç@¢òòòW'&÷"¶–æG2Â&V6W6RF†W’&RF‡&VRF–ffW&VçB&ö&ÆV×2à¢5·FW7EÐ¦fâå÷VæföÆF&ÆUö÷%öæVvF—fUöFG&Öö&6Uö—5÷7F–ÆÅ÷&V¦V7FVB‚’°¢òò6öçG&öÃ¢F†RVæ×WFFVB6†RÆ÷vW'2à¢VÖ—Eö7÷7&2‚fFG&Ö÷7&2‚$ƒ6—¦Rƒ3"’“° ¢òòæ÷B6öç7FçBBÆÃ¢cVÖ—G2&6RÂ6òF†—2×W7Bæ÷Bö–çB@¢òò—Bà¢ÆWBW'"ÒÆ÷vW%÷7&2‚fFG&Ö÷7&2‚$GWBæ6÷VçEö÷WB6—¦Rƒ3"’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2“°¢76W'B†×6ræ6öçF–ç2‚&æöâÖ6öç7FçBÆ&6Sæ"’Â'¶×6wÒ"“° ¢òòföÆF&ÆR'WBæVvF—fR(	B&öw&ÒW'&÷"Âæ÷B&6¶VæBvà¢ÆWBW'"ÒÆ÷vW%÷7&2‚ff÷&ÖB€¢&6öç7B"ÒÒ…ÆåÆç·Ò"À¢FG&Ö÷7&2‚$"6—¦Rƒ3"¢’¢çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fW'"“°¢76W'B†×6ræ6öçF–ç2‚&×W7Bæ÷B&RæVvF—fR"’Â'¶×6wÒ"“° ¢òò&6R²6—¦R×W7B7F’–ç6–FRF†RcBÖ&—BFG&W7276Râ&÷F€¢òò÷W&æG2föÆBæ÷rÂ6òF†V—"7VÒ6âÆVfR—C²âVæ6†V6¶VBF@¢òòv÷VÆBæ–2–âFV'VræBu$–â&VÆV6RÂ6–ÆVçFÇ’FVfVF–ærF†P¢òò÷fW&Æ6†V6²F†R7VÒW†—7G2f÷"à¢ÆWBW'"ÒÆ÷vW%÷7&2‚ff÷&ÖB€¢&6öç7B"¢V–çCÃcCâÒ„dddddddddddddcÆåÆç·Ò"À¢FG&Ö÷7&2‚$"6—¦Rƒ"¢’¢çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fW'"“°¢76W'B€¢×6ræ6öçF–ç2‚'v–æF÷r‚"’À¢'F†Rt”äDõrFB—2F†RöæR6†V6¶VC¢¶×6wÒ ¢“°¢76W'B†×6ræ6öçF–ç2‚&÷fW&fÆ÷w2cBÖ&—BFG&W72"’Â'¶×6wÒ"“° ¢òò6ÖRf÷"&6R²&Vv—7FW"öfg6WB(	BF†RFBF†B7GVÆÇ’&V6†W0¢òòF†RVÖ—GFVB'W2FG&W72â7VÆÆVBv—F‚äò6—¦VÂ6òF†Rv–æF÷p¢òò6†V6²&÷fR6¶—2F†—2–ç7Fæ6RæB6ææ÷B7FæB–âf÷"F†RöæP¢òò&V–ærW†W&6—6VC²F†R76W'F–öâæÖW2F†R&Vv—7FW"Fò¶VWF†RGvð¢òò'BÂ6–æ6R&÷F‚ÖW76vW2VæBF†R6ÖRv’à¢ÆWBW'"ÒÆ÷vW%÷7&2‚ff÷&ÖB€¢&6öç7B"¢V–çCÃcCâÒ„dddddddddddddddeÆåÆç·Ò"À¢FG&Ö÷7&2‚$""¢’¢çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fW'"“°¢76W'B€¢×6ræ6öçF–ç2‚'&Vv—7FW"4öfg6WBƒ‚"’À¢'F†R$Tt•5DU"&R×6†–gBFB—2F†RöæR6†V6¶VC¢¶×6wÒ ¢“°¢76W'B†×6ræ6öçF–ç2‚&÷fW&fÆ÷w2cBÖ&—BFG&W72"’Â'¶×6wÒ"“°§Ð ¦fâFG&Ö÷7&2†–ç7C¢g7G"’Óâ7G&–ær°¢DE$ÔõD"ç&WÆ6R‚$ƒ6—¦Rƒ3"Â–ç7B§Ð ¦6öç7BDE$ÔõD#¢g7G"Ò"2&'W2'W4†”Æ—FP¢†æG6†¶Uö6†ææVÂs¢6VæB¶–æC¢fÆ–E÷&VG¢FG#¢V–çCÃƒà¢VæB†æG6†¶Uö6†ææVÂp¢†æG6†¶Uö6†ææVÂs¢6VæB¶–æC¢fÆ–E÷&VG¢FF¢V–çCÃ3#à¢7G&#¢V–çCÃCà¢VæB†æG6†¶Uö6†ææVÂp¢†æG6†¶Uö6†ææVÂ#¢6VæB¶–æC¢fÆ–E÷&VG¢FG#¢V–çCÃƒà¢VæB†æG6†¶Uö6†ææVÂ ¢†æG6†¶Uö6†ææVÂ#¢&V6V—fR¶–æC¢fÆ–E÷&VG¢FF¢V–çCÃ3#à¢VæB†æG6†¶Uö6†ææVÂ ¦VæB'W2'W4†”Æ—FP §G&ç67F÷"†–Ä†VÇW"&÷VæBFò'W4†”Æ—FP¢v†Vâ7F—fP¢†öö¶&ÆRw&—FR†FG#¢V–çCÃƒâÂFF¢V–çCÃ3#â¢'W2æræFG"ÒFG ¢'W2ærçfÆ–BÒ¢'W2çrç6VæB†FFÂR¢'W2ærçfÆ–BÒ ¢v—B7–6ÆP¢VæBw&—FP ¢†öö¶&ÆR&VB†FG#¢V–çCÃƒâ’ÓâV–çCÃ3#à¢'W2æ"æFG"ÒFG ¢'W2æ"çfÆ–BÒ¢ÆWB"Ò'W2ç"ç&V7b‚¢'W2æ"çfÆ–BÒ ¢v—B7–6ÆP¢&WGW&â"æFF¢VæB&V@¢VæBv†Và¦VæBG&ç67F÷"†–Ä†VÇW  §&Vv&Æö6²FÖ6†âf–†–Ä†VÇW"v–GF‚3 ¢òòòDÔ5"(	B&—B—2%2‡'Vâ÷7F÷’à¢&Vv—7FW"DÔ5"ƒ66W72'p¢f–VÆB%2¢&—B&W6WB66W72'p¢VæB&Vv—7FW"DÔ5  ¢òòò4(	BgVÆÂ×v÷&B6÷W&6RFG&W72à¢&Vv—7FW"4ƒ‚66W72'p¦VæB&Vv&Æö6²FÖ6†à ¦FG&Ö6ö2f–†–Ä†VÇW ¢òòòÔÓ%26†ææVÂB&6Rƒ(	B&Vv—7FW"FG&W76W2Æ–vâv—F‚F†P¢òòò†”Æ—FU&Vw2ÔÓ%2&Vv—7FW"Æ–÷WBâ6—¦Rƒ3FV6Æ&W2F†P¢òòòv–æF÷r6òF†R6öFVvVâ6â7FF–6ÆÇ’'VÆR÷WB÷fW&Æv—F€¢òòòF†R3&ÖÒ–ç7Fæ6R&VÆ÷rà¢–ç7Fæ6RÖÓ'2¢FÖ6†âƒ6—¦Rƒ3 ¢òòò3$ÔÒ6†ææVÂB&6Rƒ3(	BDÔ5"Bƒ3Â4BƒC€¢òòòƒƒ3²ƒ‚’(	BÇ6òÖF6†W2F†R†”Æ—FU&Vw2Æ–÷WBà¢–ç7Fæ6R3&ÖÒ¢FÖ6†âƒ36—¦Rƒ3 ¦VæBFG&Ö6ö0 §FW7F&Væ6‚F ¢GWB¢†”Æ—FU&Vw0¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢ÆWB†–Â¢'W4†”Æ—FRÒ&–æBGW@¢ÆWB†VÇW"¢†–Ä†VÇW"7F—fRÒ&–æB†–À¢ÆWB6†—¢6ö2Ò&–æB†VÇW ¢'Và¢6†—æÖÓ'2å4Òƒ#3@¢v—B"7–6ÆW0¢VæB'Và¦VæB–×Â@¢"3° ¢òòòF†Rf÷W"F†–æw26÷fW'ö–çB6öç7FçB6â$RÂæBF†Rf÷W"F–ffW&Vç@¢òòòç7vW'2cv—fW2v†Vâ—B—2æ÷B6öç7FçBà¢òòð¢òòòöæR†VÇW"föÆFVBfV6ÆæR–æF–6W2Â&—B×6Æ–6R&÷VæG2À¢òòòçG'Væ3Äãâ‚–ÖfÖ–Ç’v–GF‡2æB2V–çCÄãæ67Bv–GF‡2ÂæBvfP¢òòòÆÂf÷W"F†R6ÖR&VgW6Â(	B–æ6ÇVF–ærF†R6ÖRDU…BÂ6ð¢òòòçG'Væ3Äãâ‚–v2&W÷'FVB2&æöâÖ6öç7FçB–æFW‚÷6Æ–6R&÷VæB"à¢òòòÖV7W&VBW"&öÆR'’×WFF–ær6÷eöW‡%÷F&vWG5÷FW7Fæ@¢òòò6¶VE÷fV5öÆæU÷FW7F†f—‡GW&W2$õD‚&6¶VæG272’öæR&÷VæBB¢òòòF–ÖRÂæB6ö×–Æ–ærcw2VÖ—76–öâv–ç7B7GV"eF÷ ¢òòð¢òòòÂ&öÆRÂcöââVç&W6öÇf&ÆRæÖRÂfW&F–7BÀ¢òòòÂÒÒ×ÂÒÒ×ÂÒÒ×À¢òòòÂfV6ÆæR–æFW‚ÂGWBÓæÆæUö–Eö÷WE´TôeÖ(	B6ö×–ÆW2Â–æFW†W2BÓÂ6–ÆVçFÇ”Ö—4Æ÷vW'6À¢òòòÂ&—B×6Æ–6R&÷VæBÂ†&5ö&—G2‡bÂ‡V–çC3%÷B’„Tôb’Â–(	B6ö×–ÆW2Â6Æ–6W2BC#“C“cs#“RÂ6–ÆVçFÇ”Ö—4Æ÷vW'6À¢òòòÂv–GF‚ÖÖWF†öBv–GF‚Â&VgW6W3¢'&WV—&W26öç7FçB–çFVvW"v–GF‚"Â&V¦V7G6À¢òòòÂ67Bv–GF‚Â‡V–çCcE÷B’‚âââ–(	B6ö×–ÆW2Âv–GF‚–væ÷&VBVçF—&VÇ’Â6–ÆVçFÇ”Ö—4Æ÷vW'6À¢òòð¢òòòTôf—2F†RÆöBÖ&V&–ær–çWBâæÆöæRv÷VÆB†fR6–@¢òòòVÖ—G5Væ6ö×–Æ&ÆV‚"târv2æ÷BFV6Æ&VB–âF†—266÷R"ÂÖV7W&VB’À¢òòòæB6òv÷VÆB7FFW'&‚&67Bg&öÒtd”ÄR¢rFòwV–çC3%÷BrÆ÷6W0¢òòò&V6—6–öâ"’(	B'WBc7FW2F†R„$2–FVçF–f–W"–çFò2²²v—F†÷W@¢òòòÆöö¶–ærB—BÂ6òæÖRF†BÇ6òæÖW2Ö7&ò6ö×–ÆW26ÆVâæ@¢òòò6×ÆW2&÷VæBæö&öG’w&÷FRâF†R&Òw27FGW2—2F†Rv÷'7BF†–ærc¢òòòFöW2ç—v†W&RVæFW"—Bà¢5·FW7EÐ¦fâö6÷fW'ö–çEö6öç7FçEö—5ö6Æ76–f–VEö'•÷v†Eö—E÷7FæG5öf÷"‚’°¢ÆWB6÷bÒf—‡GW&R‚&6÷eöW‡%÷F&vWG5÷FW7Bæ†&2"“°¢6öç7B5¢g7G"Ò&6÷fW"GWBæ6÷VçEö÷WE³3£ÕÆâ#°¢76W'B†6÷bæ6öçF–ç2„5’Â&6÷bf—‡GW&R6†R6†ævVB"“°¢ÆWB6Æ–6RÒÇC¢g7G'Â6÷bç&WÆ6Vâ„5Âff÷&ÖB‚&6÷fW"·GÕÆâ"’Â“° ¢ÆWBÆæW2Òf—‡GW&R‚'6¶VE÷fV5öÆæU÷FW7Bæ†&2"“°¢6öç7BÄäS¢g7G"Ò&6÷fW"GWBæÆæUö–Eö÷WE³Ò#°¢76W'B†ÆæW2æ6öçF–ç2„ÄäR’Â&ÆæRf—‡GW&R6†R6†ævVB"“°¢ÆWBÆæRÒÇC¢g7G'ÂÆæW2ç&WÆ6Vâ„ÄäRÂff÷&ÖB‚&6÷fW"·GÒ"’Â“° ¢òòF†RGvò&öÆW2cÖ—2×6×ÆW3¢—BVÖ—G2F†RæÖRfW&&F–Òà¢f÷"‡v†BÂ7&2ÂcöæVVFÆR’–â°¢€¢&&—B×6Æ–6R&÷VæB"À¢6Æ–6R‚&GWBæ6÷VçEö÷WE´Tôc£Ò"’À¢"‡V–çC3%÷B’„Tôb’"À¢’À¢€¢&fV6ÆæR–æFW‚"À¢ÆæR‚&GWBæÆæUö–Eö÷WE´TôeÒ"’À¢&ÆæUö–Eö÷WE´TôeÒ"À¢’À¢Ò°¢ÆWBW'"ÒÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2“°¢76W'B€¢×6ræ6öçF–ç2‡v†B’À¢'·v†GÓ¢F†R&öÆR×W7BæÖR—G6VÆc¢¶×6wÒ ¢“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·v†GÓ¢¶WÒ"’“°¢76W'B€¢cæ6öçF–ç2‡cöæVVFÆR’À¢'·v†GÓ¢c7FW2F†RæÖR–çFò2²³¢W‡V7FVB·cöæVVFÆWÖ ¢“°¢Ð ¢òòF†R&öÆRc&VgW6W2à¢ÆWBv–GF‚Ò6Æ–6R‚&GWBæ6÷VçEö÷WE³3£ÒçG'Væ3Äãâ‚’"“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fÆ÷vW%÷7&2‚gv–GF‚’çVçw&öW'"‚’ÂÆ÷vW#£¥c7FGW3£¥&V¦V7G2“°¢76W'B†×6ræ6öçF–ç2‚'v–GF‚ÖÖWF†öBv–GF‚"’Â'¶×6wÒ"“°¢ÆWBRÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gv–GF‚’’æW‡V7EöW'"‚'c&VgW6W2æöâÖ6öç7Bv–GF‚"“°¢76W'B€¢RçFõ÷7G&–ær‚’æ6öçF–ç2‚'&WV—&W26öç7FçB–çFVvW"v–GF‚"’À¢'cw2÷vâ&VgW6Â—2v†BÖ¶W2F†—2&V¦V7G6¢¶WÒ ¢“° ¢òòF†R&öÆRcæWfW"WfVâÆöö·2Bà¢ÆWB67BÒ6Æ–6R‚"†GWBæ6÷VçEö÷WB2V–çCÄãâ’"“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚f67B’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚&67Bv–GF‚"’Â'¶×6wÒ"“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f67B’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢cæ6öçF–ç2‚"‡V–çC3%÷B’„â’"’bbcæ6öçF–ç2‚'V–çCÄãâ"’À¢'cG&÷2F†Rv–GF‚&F†W"F†â&W6öÇf–ær—C¢·cÒ ¢“° ¢òòGvòÖ÷&R6†W2ÂGvòÖ÷&RfW&F–7G2Âg&öÒF†R6ÖR†VÇW"à¢ÆWB'VçF–ÖRÒ6Æ–6R‚&GWBæ6÷VçEö÷WE¶GWBæVã£Ò"“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚g'VçF–ÖR’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&æ÷B6ö×–ÆR×F–ÖR6öç7FçB"’Â'¶×6wÒ"“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g'VçF–ÖR’’æW‡V7B‚'cVÖ—G2G–æÖ–2&÷VæB"“°¢76W'B€¢cæ6öçF–ç2‚"‡V–çC3%÷B’††&5÷'C£¦†&5÷&VB†GWBÓæVâ’’"’À¢'cw2&÷VæBf&–W2W"7–6ÆRÂv†–6‚—2v‡’F†R7VvvW7F–öâ—2†öæW7C¢·cÒ ¢“° ¢ÆWB‡VvRÒ6Æ–6R‚&GWBæ6÷VçEö÷WE³“““““““““““““““““““““““£Ò"“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚f‡VvR’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚&÷fW"×v–FRÆ—FW&Â"’Â'¶×6wÒ"“° ¢ÆWB6—¦VBÒ6Æ–6R‚&GWBæ6÷VçEö÷WE³BvC3£Ò"“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚g6—¦VB’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚'6—¦VBÆ—FW&Â"’Â'¶×6wÒ"“° ¢òòäU5DTBÂ&÷F‚¶–æG2âF†RvÆ²F†Bf–æG2æÖR÷"Æ—FW&À¢òò–ç6–FR´²²ã£Ö—2v†BÖ¶W2ç’öbF†R&÷fRv÷&²öâ¢òò6ö×÷VæB&÷VæBÂæBæ÷F†–ærW†W&6—6VB—C¢7GV&&–ærF†R&V7W'6–öà¢òò÷WB‚'f—6—BF†RF÷æöFRöæÇ’"’ÆVgBF†Rv†öÆR7V—FRw&VVâà¢ÆWBæW7FVBÒ6Æ–6R‚&GWBæ6÷VçEö÷WE³²Tôc£Ò"“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚fæW7FVB’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B€¢×6ræ6öçF–ç2‚&Tôf"’À¢&'W&–VBæÖR×W7B7F–ÆÂ&Rf÷VæC¢¶×6wÒ ¢“° ¢òò(
fæBv†Vâ&÷VæB†öÆG2$õD‚¶–æG2öbVæföÆF&ÆRÆ—FW&ÂÂF†P¢òòv÷'6RfW&F–7B†2Fòv–â&Vv&FÆW72öb÷W&æB÷&FW"â&WGW&æ–æp¢òòF†Rf—'7B&RÖ÷&FW"†—BÖFR³BvC2²““ž(
c£Ö&öÖ—6P¢òòÒÖ6öFVvVâcv†–ÆR³““ž(
b²BvC3£Ö&VgW6VBFòÂf÷"&öw&Ð¢òòcG'Væ6FW2V—F†W"v’à¢f÷"B–â°¢&GWBæ6÷VçEö÷WE³BvC2²“““““““““““““““““““““““£Ò"À¢&GWBæ6÷VçEö÷WE³““““““““““““““““““““““’²BvC3£Ò"À¢Ò°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g6Æ–6R‡B’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚&÷fW"×v–FRÆ—FW&Â"’Â&·GÖ¢¶×6wÒ"“°¢Ð¢òò(
fæBF†R&V6öâF†BöæR¶VW2F†R7VvvW7F–öã¢cföÆG2—Bà¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g6—¦VB’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢cæ6öçF–ç2‚"‡V–çC3%÷B’ƒ2’"’À¢'cföÆG2BvC6Fò2Â6òÒÖ6öFVvVâc&VÆÇ’—2v’÷WC¢·cÒ ¢“°§Ð ¢òòò6÷fW'ö–çB&÷VæB—26öç7FçBU…$U54”ôâÂæ÷B§W7BÆ—FW&Â÷"¢òòòæÖRâcVÖ—G2³²#£Ö2†&5ö&—G2‡bÂ‡V–çC3%÷B’ƒ²"’Â–À¢òòòv†–6‚ÖVç2W†7FÇ’³3£ÖÂ6ò&VgW6–ær—Bv27V'6WBvv—F€¢òòòæ÷F†–ær&V†–æB—BâföÆF–ærF‡&÷Vv‚föÆEö6öç7F(	BF†R6ÖRWfÇVF÷ ¢òòò6öç7FFV6Æ&F–öç2W6R(	B6Æ÷6W2—BÂæBF†RFW7B—2WVÆ—G’v—F€¢òòòF†RÆ—FW&Â7VÆÆ–ær&F†W"F†â6†R76W'F–öâà¢5·FW7EÐ¦fâö6öç7FçEöW‡&W76–öåö6÷fW'ö–çEö&÷VæEöföÆG2‚’°¢ÆWB6÷bÒf—‡GW&R‚&6÷eöW‡%÷F&vWG5÷FW7Bæ†&2"“°¢6öç7B5¢g7G"Ò&6÷fW"GWBæ6÷VçEö÷WE³3£ÕÆâ#°¢ÆWB6Æ–6RÐ¢Ç&Vf—ƒ¢g7G"ÂC¢g7G'Âf÷&ÖB‚'·&Vf—‡×·Ò"Â6÷bç&WÆ6Vâ„5Âff÷&ÖB‚&6÷fW"·GÕÆâ"’Â’“°¢ÆWBÆ—FW&ÂÒVÖ—Eö7÷7&2‚f6÷b“°¢f÷"‡v†BÂ&Vf—‚ÂB’–â°¢‚&&—F†ÖWF–2"Â""Â&GWBæ6÷VçEö÷WE³²#£Ò"’À¢€¢&6öç7B–ââW‡&W76–öâ"À¢&6öç7B²ÒÆåÆâ"À¢&GWBæ6÷VçEö÷WE´²²#£Ò"À¢’À¢‚&6†–gB"Â""Â&GWBæ6÷VçEö÷WE²ƒÃÂ"’Ò£Ò"’À¢‚&†W‚Æ—FW&Â"Â""Â&GWBæ6÷VçEö÷WE³ƒ3£Ò"’À¢Ò°¢76W'EöW€¢VÖ—Eö7÷7&2‚g6Æ–6R‡&Vf—‚ÂB’’À¢Æ—FW&ÂÀ¢'·v†GÒ×W7BÆ÷vW"W†7FÇ’Æ–¶RF†RÆ—FW&Â³3£Ö ¢“°¢Ð¢òòF†RÆæRÖ–æFW‚&öÆRföÆG2F‡&÷Vv‚F†R6ÖR†VÇW"à¢ÆWBÆæW2Òf—‡GW&R‚'6¶VE÷fV5öÆæU÷FW7Bæ†&2"“°¢6öç7BÄäS¢g7G"Ò&6÷fW"GWBæÆæUö–Eö÷WE³Ò#°¢76W'EöW€¢VÖ—Eö7÷7&2‚fÆæW2ç&WÆ6Vâ„ÄäRÂ&6÷fW"GWBæÆæUö–Eö÷WE³"Ò%Ò"Â’’À¢VÖ—Eö7÷7&2‚fÆæW2’À¢&föÆFVBÆæR–æFW‚×W7BÆ÷vW"W†7FÇ’Æ–¶RF†RÆ—FW&Â ¢“°§Ð ¢òòòF†R'V–ÇBÖ–â&VF–6FW2öâE$å45Dõ"&V6V—fW"âc&W6öÇfW2F†VÐ¢òòòF†W&R2vVÆÂ2öâ6ö×öæVçB(	B&W6öÇfUö6ö×öæVçEö–FÆU÷&VF–6FV ¢òòòvÆ·26VÆbçG&ç67F÷'6F‡&÷Vv‚7–çF…ö6ö×öæVçEög&öÕ÷G&ç67F÷&À¢òòòæB&÷F‚&6¶VæG27F×öÆ7Eö–åö7–6ÆVööÆ7Eö÷WEö7–6ÆVöà¢òòòG&ç67F÷"7FFR7G'V7G2(	B6òfÆB–çfÆ–Fv26ÆÆ–ærGvVÇfP¢òòòv÷&¶–ær&öw&×2ƒBæÖW2‚76W'Bò&&R7FFVÖVçBòÆWF’W'&÷'2à¢òòð¢òòòF†R6'fRÖ÷WBF†Bf—†VB—B6†—VBv—F‚æòFW7C¢bbfÇ6Vöâ—@¢òòòÆVgBF†Rv†öÆR7V—FRw&VVâà¢5·FW7EÐ¦fâö'V–ÇEö–å÷&VF–6FUööåö÷G&ç67F÷%ö—5öövöæ÷EöåöW'&÷"‚’°¢ÆWB7&2ÒÇ7F×C¢g7G'Â°¢f÷&ÖB€¢"2&FöÖ–â7—4FöÖ–à¢g&WöÖ‡£¢ ¦VæBFöÖ–â7—4FöÖ–à §G&ç67F÷"G'`¢GWB¢F÷ ¢v†Vâ7F—fP¢†öö¶&ÆRvò‚¢v—B7–6ÆP¢VæBvð¢VæBv†Và¦VæBG&ç67F÷"G'` §FW7F&Væ6‚F ¢GWB¢F÷ ¢B¢G'b7F—fP¦VæBFW7F&Væ6‚F  ¦–×ÂEBf÷"F ¢6Æö6²6Æ²Ò7—4FöÖ–à¢'Và¢BæGWBÒGW@¢·7F×GÐ¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂEB"0¢¢Ó°¢f÷"æÖR–â²&–FÆR"Â&–FÆUö–â"Â&–FÆUö÷WB"Â'V–W66VB%Ò°¢f÷"7F×B–â°¢f÷&ÖB‚&76W'BBç¶æÖWÒƒ"’VÇ6Rf–Â…Â&õÂ"’"’À¢f÷&ÖB‚&Bç¶æÖWÒƒ"’"’À¢f÷&ÖB‚&ÆWBbÒBç¶æÖWÒƒ"’"’À¢Ò°¢ÆWB2Ò7&2‚g7F×B“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚g2’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚ff÷&ÖB‚&Bç¶æÖWÖöâG&ç67F÷""’’À¢&·7F×GÖ¢¶×6wÒ ¢“°¢òòF†R†ÆbF†BÖ¶W2F†R7VvvW7F–öâ†öæW7C¢cVÖ—G2F†P¢òò†V'F&VBv–ç7BF†RG&ç67F÷"w2÷vâ7F×2à¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g2’’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚&·7F×GÖ¢¶WÒ"’“°¢òò–FÆUö÷WF&VG2öæÇ’F†R÷WB7F×²F†R÷F†W"F‡&VR&V@¢òòF†R–â7F×†–FÆVæBV–W66VF&VB&÷F‚’à¢ÆWB7F×Ò–bæÖRÓÒ&–FÆUö÷WB"°¢%÷F"æBåöÆ7Eö÷WEö7–6ÆR ¢ÒVÇ6R°¢%÷F"æBåöÆ7Eö–åö7–6ÆR ¢Ó°¢76W'B€¢cæ6öçF–ç2‡7F×’À¢&·7F×GÖ¢c&W6öÇfW2F†R&VF–6FRöâF†RG&ç67F÷"†·7F×Ö’ ¢“°¢Ð¢Ð¢òò(
fæBæÖRæ÷F†–ær–×ÆVÖVçG2—27F–ÆÂF†R&öw&ÒW'&÷"F†P¢òò7W'&÷VæF–ær&Òv2w&—GFVâf÷"à¢ÆWB2Ò7&2‚&76W'BBææ÷7V6‚ƒ"’VÇ6Rf–Â…Â&õÂ"’"“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚g2’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&†2æòÖWF†öBæ÷7V6†"’Â'¶×6wÒ"“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g2’’æW‡V7B‚'cVÖ—G2—B"“°¢76W'B€¢cæ6öçF–ç2‚%÷F"æBææ÷7V6‚ƒ"’"’À¢'cVÖ—G26ÆÂG'f†2æòÖVÖ&W"f÷#¢·cÒ ¢“°§Ð ¢òòòWfW'’v–GF‚ÖÖWF†öBæB67BfW&F–7B–âF†R6÷fW&w&÷WF‚Âv–ç7@¢òòòv†Bc7GVÆÇ’FöW2(	B##B6VÆÇ2Âæ÷BF†R†æFgVÂöbW†×ÆW2F†@¢òòòÆWBf÷W"&÷VæG2öbFVfV7G2F‡&÷Vv‚à¢òòð¢òòòF†R'VÆR—2öæRÖF—&V7F–öæÂæB6†VFò7FFS¢–bc$TeU4U2¢òòò&öw&ÒÂD"Ô•"×W7Bæ÷Bç7vW"Vç7W÷'FVFÂ&V6W6RF†B&VæFW'0¢òòò'&R×'Vâv—F‚ÒÖ6öFVvVâc"æBc—2æ÷BF†W&Râ–bcTÔ•E2öæRÀ¢òòòD"Ô•"×W7Bæ÷Bç7vW"–çfÆ–F÷"æ÷D–×ÆVÖVçFVFÂ&V6W6R&÷F€¢òòòFVç’âW66R†F6‚F†BW†—7G2à¢òòð¢òòò6†V6¶–ær—B'’W†×ÆR—2v†Bf–ÆVB&WVFVFÇ’âV6‚öbF†W6Rv0¢òòòf÷VæB'’VçVÖW&F–öâæBv÷VÆB†fR&VVâ6Vv‡B†W&S ¢òòð¢òòò¢ç&W6—¦SÃCâ‚–öââ‚Ö&—BfÇVR(	B–çfÆ–FÂ&÷F‚&6¶VæG0¢òòò6ö×–ÆR—B‡F†Rw&öærÖF—&V7F–öâ'VÆRFöW2æ÷BÇ’Fò&W6—¦V¢òòò¢ç¦W‡CÃ#â‚–(	BVç7W÷'FVFÂc&VgW6W2—BBF†R#BÖ&—@¢òòòÆæwVvRÆ–Ö—@¢òòò¢†2V–çCÃ3â’çG'Væ3Ã#â‚–(	Bæ÷D–×ÆVÖVçFVFÂc'V–ÆG2—@¢òòò¢³#£ÒçG'Væ3ÃcSâ‚–(	Bæ÷D–×ÆVÖVçFVF&ÆÖ–ær67BF†B—0¢òòòæ÷B–âF†R&öw&Ð¢òòò¢6öç7B„’Ò²´„“£Òç¦W‡CÃsâ‚–(	B–çfÆ–Fv†W&RF†P¢òòòÆ—FW&Â7VÆÆ–æröbF†R6ÖR&öw&Ò—26÷'&V7BÂ&V6W6RF†P¢òòòv–GF‚–æfW&Væ6RföÆFVBæBcw2FöW2æ÷@¢5·FW7EÐ¦fâ6÷fW&w&÷W÷v–GF…÷fW&F–7G5öw&VU÷v—F…÷cö7&÷75÷F†Uöw&–B‚’°¢ÆWB6÷bÒf—‡GW&R‚&6÷eöW‡%÷F&vWG5÷FW7Bæ†&2"“°¢6öç7B5¢g7G"Ò&6÷fW"GWBæ6÷VçEö÷WE³3£ÕÆâ#°¢76W'B†6÷bæ6öçF–ç2„5’Â&f—‡GW&R6†R6†ævVB"“°¢òò†ÆbÆ—FW&Â×7VÆÆVBÂ†Æbw&—GFVâv—F‚6öç7F(	B&V6W6RF†P¢òòv–GF‚”ädU$Tä4RF–ffW'2&WGvVVâF†RGvòæBÆ—FW&ÂÖöæÇ’w&–@¢òòÖ—76W2—Bâf÷VæB'’×WFF–öâöâF†—2fW'’FW7C¢&W7F÷&–ærF†P¢òòföÆF–ær–æfW&Væ6RÆVgBâÆÂÖÆ—FW&Âw&–Bw&VVâv†–ÆP¢òò´„“£Òç¦W‡CÃsâ‚–vVçB&6²Fò–çfÆ–Föâ&öw&Òc'Vç2à¢6öç7B$TÅTDS¢g7G"Ò&6öç7B„’ÒÆæ6öç7BÄòÒÆæ6öç7Bs‚Ò…Ææ6öç7B³#‚Ò#…ÆåÆâ#°¢ÆWB&V6V—fW'2Ò°¢&GWBæ6÷VçEö÷WE³3£Ò"À¢&GWBæ6÷VçEö÷WE³s£Ò"À¢&GWBæ6÷VçEö÷WE³£Ò"À¢"†GWBæ6÷VçEö÷WB2V–çCÃ#ƒâ’"À¢"†GWBæ6÷VçEö÷WB2V–çCÃ#â’"À¢"†GWBæ6÷VçEö÷WB2V–çCÃ3â’"À¢&GWBæ6÷VçEö÷WB"À¢&GWBæ6÷VçEö÷WE´„“¤ÄõÒ"À¢&GWBæ6÷VçEö÷WEµs‚Ò£Ò"À¢&GWBæ6÷VçEö÷WE³²#£Ò"À¢"†GWBæ6÷VçEö÷WB2V–çCÄ³#ƒâ’"À¢Ó°¢ÆWB×WB&C¢fV3Å7G&–æsâÒfV3£¦æWr‚“°¢ÆWB×WB6VÆÇ2ÒW6—¦S°¢f÷"&V7b–â&V6V—fW'2°¢f÷"ÖWF†öB–â²'G'Væ2"Â'¦W‡B"Â'6W‡B"Â'&W6—¦R%Ò°¢f÷"v–GF‚–â³'S3"ÂBÂcBÂcRÂ#‚Â#Â3Â#Ò°¢ÆWBF&vWBÒf÷&ÖB‚'·&V7gÒç¶ÖWF†öGÓÇ·v–GF‡Óâ‚’"“°¢ÆWB7&2Òf÷&ÖB€¢'µ$TÅTDW×·Ò"À¢6÷bç&WÆ6Vâ„5Âff÷&ÖB‚&6÷fW"·F&vWGÕÆâ"’Â¢“°¢ÆWBF"ÒÆ÷vW%÷7&2‚g7&2“°¢ÆWBcöö²Ò7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’æ—5öö²‚“°¢6VÆÇ2³Ò°¢ÆWBw&öærÒÖF6‚‚gF"Âcöö²’°¢òòc&VgW6W3²D"Ô•"×W7Bæ÷B6VæBF†RW6W"Fò—Bà¢„W'"†Æ÷vW#£¤Æ÷vW$W'&÷#£¥Vç7W÷'FVB²ââÒ’ÂfÇ6R’Óâ°¢6öÖR‚&Vç7W÷'FVF'WBc&VgW6W2—B"¢Ð¢òòc'V–ÆG2—C²D"Ô•"×W7Bæ÷BFVç’F†R†F6‚à¢„W'"†Æ÷vW#£¤Æ÷vW$W'&÷#£¤–çfÆ–B…ò’’ÂG'VR’Óâ°¢6öÖR‚&–çfÆ–F'WBc'V–ÆG2—B"¢Ð¢„W'"†Æ÷vW#£¤Æ÷vW$W'&÷#£¤æ÷D–×ÆVÖVçFVB²ââÒ’ÂG'VR’Óâ°¢6öÖR‚&æ÷D–×ÆVÖVçFVF'WBc'V–ÆG2—B"¢Ð¢òÓâæöæRÀ¢Ó°¢–bÆWB6öÖR‡v‡’’Òw&öær°¢&BçW6‚†f÷&ÖB‚'·F&vWGÓ¢·v‡—Ò"’“°¢Ð¢Ð¢Ð¢Ð¢76W'EöW†6VÆÇ2Â3S"Â'F†Rw&–B6†ævVB6†R"“°¢76W'B€¢&Bæ—5öV×G’‚’À¢'·Òöb¶6VÆÇ7Ò6VÆÇ2F—6w&VRv—F‚c¥Æç·Ò"À¢&BæÆVâ‚’À¢&Bæ¦ö–â‚%Æâ"¢“°§Ð ¢òòòF†Rv–GF‚ÖÖWF†öBD•$T5D”ôâ'VÆRÂ&÷fRcB&—G2æBF‡&÷Vv‚æW7FV@¢òòò&V6V—fW"(	BF†RGvòÆ6W2—BF–Bæ÷B&V6‚ÂÇW2F†RöæRÖWF†öB—@¢òòò×W7Bæ÷BÇ’Fòà¢òòð¢òòòF†R6†V6²W6VBFò6—B&VÆ÷rF†RãcF&VgW6ÂÂ6ò—Bv2Vç&V6†&ÆP¢òòòf÷"W†7FÇ’F†Rv–GF‡2F†BæVVB—C²æB6÷fW%ö–æfW%öW‡%÷v–GF† ¢òòò76VBæöæVf÷"æW7FVB&V6V—fW"Â6ò³3£ÒçG'Væ3Ã#ƒâ‚–v0¢òòò–çfÆ–FÆöæRæBVç7W÷'FVFF†RÖöÖVçBç—F†–ærw&VB—Bà¢òòòæV—F†W"f—‚v2–ææVB'’ç—F†–æs¢&WfW'F–ærV—F†W"ÆVgBF†Rv†öÆP¢òòò7V—FRw&VVâà¢òòð¢òòò&W6—¦V—2äõB7V&¦V7BFòF†R'VÆRâF†R7V26—26ð¢òòò‚&ç&W6—¦SÄãâ‚–&VÖ–ç2F—&V7F–öâÖvæ÷7F–2"’Âcw2÷vâ6†V6²&VG0¢òòò'¦W‡B"Â'6W‡B&ÂæBD"Ô•"w2vVæW&ÂW‡&W76–öâÆ÷vW&–ærW†6ÇVFW0¢òòò—B(	BF‡&VR7FFVÖVçG2öbF†R'VÆRÂæöæRöbF†VÒ&VB&Vf÷&R—Bv0¢òòò–çfVçFVBÂv†–6‚ÖFRGWBæ6÷VçEö÷WE³s£Òç&W6—¦SÃCâ‚–'&öw&Ð¢òòòW'&÷""F†B&÷F‚&6¶VæG2†B&VVâ6ö×–Æ–ærFò–FVçF–6Â2²²à¢5·FW7EÐ¦fâF†U÷v–GF…öF—&V7F–öå÷'VÆU÷&V6†W5÷v–FUöæEöæW7FVE÷&V6V—fW'2‚’°¢ÆWB6÷bÒf—‡GW&R‚&6÷eöW‡%÷F&vWG5÷FW7Bæ†&2"“°¢6öç7B5¢g7G"Ò&6÷fW"GWBæ6÷VçEö÷WE³3£ÕÆâ#°¢ÆWB6Æ–6RÒÇC¢g7G'Â6÷bç&WÆ6Vâ„5Âff÷&ÖB‚&6÷fW"·GÕÆâ"’Â“° ¢òò&÷fRcBÂv†W&RF†R6†V6²6÷VÆBæ÷B&Wf–÷W6Ç’'Vââc&VgW6W0¢òòV6‚öbF†W6RÂv†–6‚—2v†BÖ¶W2–çfÆ–F&–v‡B&F†W"F†â¢òòÒÖ6öFVvVâc7VvvW7F–öâà¢f÷"B–â°¢&GWBæ6÷VçEö÷WE³£Òç¦W‡CÃsâ‚’"À¢&GWBæ6÷VçEö÷WE³£Òç6W‡CÃsâ‚’"À¢"†GWBæ6÷VçEö÷WB2V–çCÃ#ƒâ’ç¦W‡CÃâ‚’"À¢Ò°¢ÆWB7&2Ò6Æ–6R‡B“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚'v–GF‚×W7B&R(šRF†R6÷W&6Rv–GF‚"’À¢&·GÖ¢¶×6wÒ ¢“°¢ÆWBRÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’æW‡V7EöW'"‚'c&VgW6W2—BFöò"“°¢76W'B€¢RçFõ÷7G&–ær‚’æ6öçF–ç2‚'v–GF‚×W7B&R(šRF†R6÷W&6Rv–GF‚"’À¢&·GÖ¢cw26VçFVæ6R—2F†RöæRD"Ô•"&–çG3¢¶WÒ ¢“°¢Ð ¢òòF‡&÷Vv‚äU5DTBv–GF‚ÖWF†öC¢F†R–ææW"&öw&Ò—2&VgW6V@¢òòv†WF†W"÷"æ÷B6öÖWF†–ærw&2—Bà¢ÆWB&&RÒ6Æ–6R‚&GWBæ6÷VçEö÷WE³3£ÒçG'Væ3Ã#ƒâ‚’"“°¢ÆWBw&VBÒ6Æ–6R‚&GWBæ6÷VçEö÷WE³3£ÒçG'Væ3Ã#ƒâ‚’ç¦W‡CÃ#ƒâ‚’"“°¢ÆWB&&Uö×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚f&&R’çVçw&öW'"‚’“°¢ÆWBw&VEö×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚gw&VB’çVçw&öW'"‚’“°¢76W'EöW€¢&&Uö×6rÂw&VEö×6rÀ¢&w&W"×W7Bæ÷B6†ævRF†R–ææW"&öw&Òw2fW&F–7B ¢“° ¢òò&W6—¦Væ'&÷w2÷"v–FVç22—BÆ–¶W2ÂBç’v–GF‚à¢f÷"B–â°¢&GWBæ6÷VçEö÷WE³s£Òç&W6—¦SÃCâ‚’"À¢&GWBæ6÷VçEö÷WE³s£Òç&W6—¦SÃ3#â‚’"À¢&GWBæ6÷VçEö÷WE³3£ÒçG'Væ3Ã#â‚’ç&W6—¦SÃâ‚’"À¢Ò°¢VÖ—Eö7÷7&2‚g6Æ–6R‡B’“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g6Æ–6R‡B’’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚&·GÖ¢c'V–ÆG2—BFöó¢¶WÒ"’“°¢Ð ¢òòWVÂv–GF‡3¢G'Væ6—2æòÖ÷æB&VgW6VBÂF†Rv–FVæ–ær— ¢òò—266WFVBâ&÷F‚&6¶VæG2w&VRà¢ÆWBW÷G'Væ2Ò6Æ–6R‚&GWBæ6÷VçEö÷WE³s£ÒçG'Væ3Ãƒâ‚’"“°¢76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚fW÷G'Væ2’çVçw&öW'"‚’“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fW÷G'Væ2’’æW‡V7EöW'"‚'c&VgW6W2æòÖ÷G'Væ2"“°¢VÖ—Eö7÷7&2‚g6Æ–6R‚&GWBæ6÷VçEö÷WE³s£Òç¦W‡CÃƒâ‚’"’“° ¢òòv–FR&V6V—fW'2æBv–FRv–GF‡2Æ–¶R¶VWF†R7VvvW7F–öã¢c¢òò'V–ÆG2ÆÂöbF†VÒVæFW"×7FCÖvçR²³#Âv†–6‚—2v†@¢òò7&2öÖ–âç'676W2FòF†RVÖ—GFVBFW7F&Væ6‚âà¢òòVÖ—G5Væ6ö×–Æ&ÆV7Æ—BB#‚W6VBFòÆ—fR†W&RæB–à¢òò6÷fW%ö67E÷v–GF†ÂÖV7W&VBv—F‚×7FCÖ2²³#Âv†W&P¢òò†&5v–FVw2—5ö–çFVw&Å÷fvFR&V¦V7G2õö–çC#†à¢f÷"B–â°¢"†GWBæ6÷VçEö÷WB2V–çCÃ3â’çG'Væ3Ã#â‚’"À¢&GWBæ6÷VçEö÷WE³3£Òç¦W‡CÃ#â‚’"À¢&GWBæ6÷VçEö÷WE³3£Òç6W‡CÃ#â‚’"À¢òò6Æ–6RÖFW&—fVB6÷W&6Rv–GF‚&÷fR#‚Âv†–6‚F†Rã#† ¢òòÖ6†–æW'’&W÷'FVB2&ö&ÆVÒv—F‚'F†R&V6V—fW"w2÷và¢òò67B"v†VâF†W&R—2æò67BBÆÂà¢&GWBæ6÷VçEö÷WE³#£ÒçG'Væ3ÃcSâ‚’"À¢Ò°¢76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚g6Æ–6R‡B’’çVçw&öW'"‚’“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g6Æ–6R‡B’’’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚&·GÖ¢c'V–ÆG2—C¢¶WÒ"’“°¢Ð ¢òòF†RF—&V7F–öâ6†V6²×W7Bf—&RW†7FÇ’v†W&Rcw2FöW2Âv†–6€¢òòÖVç2–æfW'&–ær&V6V—fW"v–GF‚öæÇ’g&öÒÄ•DU$Å2(	BcW6W0¢òòWfÅö6öç7E÷v–GF†æB6òFöW2D"Ô•"w2vVæW&ÂF‚âföÆF–ærF†P¢òò&÷VæBÖFRF†W6R–çfÆ–Fv†–ÆRF†R–FVçF–6Â&öw&Ò6ö×–ÆV@¢òòVæFW"cÂFV6–FVBW&VÇ’'’v†WF†W"F†R&÷VæB†BæÖRà¢f÷"‡&RÂB’–â°¢‚&6öç7Bs‚Ò…ÆåÆâ"Â&GWBæ6÷VçEö÷WEµs‚Ò£Òç¦W‡CÃCâ‚’"’À¢‚""Â&GWBæ6÷VçEö÷WE³²#£ÒçG'Væ3ÃCâ‚’"’À¢Ò°¢ÆWB7&2Òf÷&ÖB‚'·&W×·Ò"Â6Æ–6R‡B’“°¢VÖ—Eö7÷7&2‚g7&2“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚&·GÖ¢c'V–ÆG2—C¢¶WÒ"’“°¢Ð¢òòföÆFVB&÷VæBv—F‚v–GF‚$õdRcB—27F–ÆÂ&VgW6VBÂ'WBf÷ ¢òòF†RcBÖ&—BÖöFVÂ&F†W"F†âf÷"F—&V7F–öâ(	BVç7W÷'FVFÂæ÷@¢òòF†R–çfÆ–FF†RföÆF–ær–æfW&Væ6RW6VBFò&öGV6Rà¢ÆWBföÆFVE÷v–FRÒf÷&ÖB€¢&6öç7B„’ÒÆåÆç·Ò"À¢6Æ–6R‚&GWBæ6÷VçEö÷WE´„“£Òç¦W‡CÃsâ‚’"¢“°¢76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚fföÆFVE÷v–FR’çVçw&öW'"‚’“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fföÆFVE÷v–FR’’æW‡V7B‚'c'V–ÆG2—B"“°¢òò(
gv†–ÆRF†RÆ—FW&Â7VÆÆ–æröbF†R6ÖR&öw&Ò7F—2–çfÆ–F ¢òòVæFW"&÷F‚&6¶VæG2à¢76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚g6Æ–6R‚&GWBæ6÷VçEö÷WE³£Òç¦W‡CÃsâ‚’"’’çVçw&öW'"‚’“°§Ð ¢òòòF†R†öö²×&ÖWFW"wV&BöâF†Rf÷W"6öç7FçB$ôÄU2ÂæBF†P¢òòòæVvF—fRÖföÆBfW&F–7B(	B&÷F‚FFVB–â&W7öç6RFò&Wf–Wræ@¢òòòæV—F†W"–ææVB'’ç—F†–ærÂv†–6‚&Wf–Wr&÷VæBF‡&VR6Vv‡B'¢òòòFVÆWF–ærV6‚æBvF6†–ærCƒ"FW7G27F’w&VVââF†B—2F†RF†—&@¢òòòF–ÖRöâF†—2'&æ6‚wV&B†26†—VBVæÖV7W&VBÂ–â6öÖÖ—@¢òòòw&—GFVâFòç7vW"&Wf–WrF†B6–BW†7FÇ’F†Bà¢òòð¢òòò†öö²&ÖWFW"&VG2f–ÆR×66÷R6öç7FöbF†R6ÖRæÖRâv†@¢òòòcFöW2v—F‚W"Ö6ÆÂ&÷VæBF†VâFWVæG2öâF†R$ôÄRÂæBfÆ@¢òòòVç7W÷'FVFf÷"ÆÂf÷W"v2fÇ6RW66R†F6‚BGvòöbF†VÓ ¢òòð¢òòòÂ6÷fW'ö–çBÂcÂfW&F–7BÀ¢òòòÂÒÒ×ÂÒÒ×ÂÒÒ×À¢òòòÂ6ÖBçF–6·5¶³£ÖÂ†&5ö&—G2†6ÖBçF–6·2Â‡V–çC3%÷B’†²’Â–(	B6÷'&V7BÂVç7W÷'FVFÀ¢òòòÂ6ÖBçF–6·2çG'Væ3Æ³â‚–Â&VgW6W3¢'&WV—&W26öç7FçB–çFVvW"v–GF‚"Â&V¦V7G6À¢òòòÂ6ÖBçF–6·2ç¦W‡CÆ³â‚–Â6ÖR&VgW6ÂÂ&V¦V7G6À¢òòòÂ†6ÖBçF–6·22V–çCÆ³â–Â‡V–çCcE÷B’†6ÖBçF–6·2–(	Bv–GF‚G&÷VBÂ6–ÆVçFÇ”Ö—4Æ÷vW'6À¢5·FW7EÐ¦fâö†ööµ÷&ÖWFW%ö&÷VæEö—5ö6Æ76–f–VEö'•÷&öÆUöÆ–¶Uöç•ö÷F†W"‚’°¢ÆWB†öö¶VBÒf—‡GW&R‚&6÷fW&w&÷Wö†ööµ÷&Õ÷FW7Bæ†&2"¢ç&WÆ6Vâ€¢"†öö¶&ÆR'Våöf÷"†6ÖC¢'Vä6ÖB’"À¢"†öö¶&ÆR'Våöf÷"†6ÖC¢'Vä6ÖBÂ³¢V–çCÃƒâ’"À¢À¢¢ç&WÆ6Vâ€¢&6÷fW&w&÷W'Vä6ÖD6÷b†G'bç'Våöf÷"†6ÖB’÷7B’"À¢&6÷fW&w&÷W'Vä6ÖD6÷b†G'bç'Våöf÷"†6ÖBÂ²’÷7B’"À¢À¢¢ç&WÆ6Vâ€¢"G'bç'Våöf÷"†6ÖB’"À¢"G'bç'Våöf÷"†6ÖBÂ2’"À¢À¢“°¢76W'B€¢†öö¶VBæ6öçF–ç2‚&6ÖC¢'Vä6ÖBÂ³¢V–çCÃƒâ"’bb†öö¶VBæ6öçF–ç2‚''Våöf÷"†6ÖBÂ2’"’À¢'F†R66Æ"†öö²&ÖWFW"×W7B7GVÆÇ’&RFFVB ¢“°¢ÆWBö–çBÒÇC¢g7G'Â°¢†öö¶VBç&WÆ6Vâ€¢&7÷F–6·2¢6÷fW"6ÖBçF–6·2"À¢ff÷&ÖB‚&7÷F–6·2¢6÷fW"·GÒ"’À¢À¢¢Ó° ¢òòF†R&öÆRcvWG2&–v‡B¶VW2F†R7VvvW7F–öî(
`¢ÆWB6Æ–6RÒö–çB‚&6ÖBçF–6·5¶³£Ò"“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚g6Æ–6R’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&&—B×6Æ–6R&÷VæB¶"’Â'¶×6wÒ"“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g6Æ–6R’’æW‡V7B‚'cVÖ—G2F†RW"Ö6ÆÂ&÷VæB"“°¢76W'B€¢cæ6öçF–ç2‚&†&5ö&—G2†6ÖBçF–6·2Â‡V–çC3%÷B’†²’"’À¢'cVÖ—G2F†R†öö²$uTÔTåB2F†R&÷VæC¢·cÒ ¢“°¢òò(
fæB—B—2&VÂ†¦&BÂæ÷B‡—÷F†WF–6Ã¢âVç&VÆFVB6öç7@¢òòöbF†R6ÖRæÖR×W7Bæ÷B6GW&R—Bà¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚ff÷&ÖB‚&6öç7B²ÒuÆåÆç·6Æ–6WÒ"’’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚&—2†öö²&ÖWFW""’À¢&f–ÆR×66÷R6öç7B¶×W7Bæ÷BföÆBF†R&÷VæBFò³s£Ó¢¶×6wÒ ¢“° ¢òòF†RGvò&öÆW2c&VgW6W>(
`¢f÷"B–â²&6ÖBçF–6·2çG'Væ3Æ³â‚’"Â&6ÖBçF–6·2ç¦W‡CÆ³â‚’%Ò°¢ÆWB7&2Òö–çB‡B“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’ÂÆ÷vW#£¥c7FGW3£¥&V¦V7G2“°¢76W'B†×6ræ6öçF–ç2‚'v–GF‚ÖÖWF†öBv–GF‚¶"’Â&·GÖ¢¶×6wÒ"“°¢ÆWBRÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’æW‡V7EöW'"‚'c&VgW6W2æöâÖ6öç7Bv–GF‚"“°¢76W'B€¢RçFõ÷7G&–ær‚’æ6öçF–ç2‚'&WV—&W26öç7FçB–çFVvW"v–GF‚"’À¢&·GÖ¢¶WÒ ¢“°¢Ð ¢òò(
fæBF†RöæR—B66WG2v†–ÆRG&÷–ærF†Rv–GF‚öâF†RfÆö÷"à¢ÆWB67BÒö–çB‚"†6ÖBçF–6·22V–çCÆ³â’"“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚f67B’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚&67Bv–GF‚¶"’Â'¶×6wÒ"“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f67B’’æW‡V7B‚'cVÖ—G2—BÂv–GF‚æBÆÂ"“°¢76W'B€¢cæ6öçF–ç2‚"‡V–çC3%÷B’†²’"’À¢'cæWfW"&W6öÇfW267Bv–GFƒ¢·cÒ ¢“°§Ð ¢òòòäTtD•dRföÆFVB&÷VæBÂv†–6‚F†R6öç7FçBföÆBÖFR&V6†&ÆP¢òòò†³ÒÖv2æ÷B6öç7FçBW‡&W76–öâ&Vf÷&R—B’æBv†–6‚¢òòòf—'7BfW'6–öâ6ÆÆVB–çfÆ–F(	BGvVçG’Æ–æW2&÷fRF&ÆR6––æp¢òòòc6ö×–ÆW2F†RWV—fÆVçBà¢òòð¢òòòTôf—2‚Ó–öâvÆ–&2Â6òGWBæÆæUö–Eö÷WE³ÒÖæ@¢òòòGWBæÆæUö–Eö÷WE´TôeÖ&RF†R6ÖR2²²gFW"&W&ö6W76–ærâc¢òòòVÖ—G2&÷F‚Â&÷F‚6ö×–ÆRÂ&÷F‚–æFW‚BÓâöæR6ææ÷B&R&öw&Ð¢òòòW'&÷"v†–ÆRF†R÷F†W"—26–ÆVçBÖ—2ÖÆ÷vW&–ærà¢5·FW7EÐ¦fâöæVvF—fUöföÆFVEö&÷VæEö—5ö6Æ76–f–VEöÆ–¶Uö—G5öÖ7&õ÷7VÆÆ–ær‚’°¢ÆWBÆæW2Òf—‡GW&R‚'6¶VE÷fV5öÆæU÷FW7Bæ†&2"“°¢6öç7BÄäS¢g7G"Ò&6÷fW"GWBæÆæUö–Eö÷WE³Ò#°¢76W'B†ÆæW2æ6öçF–ç2„ÄäR’Â&f—‡GW&R6†R6†ævVB"“°¢ÆWBÆæRÒÇC¢g7G'ÂÆæW2ç&WÆ6Vâ„ÄäRÂff÷&ÖB‚&6÷fW"·GÒ"’Â“° ¢ÆWB×WBcöf÷&×2ÒfV3£¦æWr‚“°¢f÷"‡BÂvçB’–â°¢‚&GWBæÆæUö–Eö÷WE³ÒÒ"Â&ÆæUö–Eö÷WE³ÒÒ"’À¢‚&GWBæÆæUö–Eö÷WE´TôeÒ"Â&ÆæUö–Eö÷WE´TôeÒ"’À¢Ò°¢ÆWB7&2ÒÆæR‡B“°¢76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚&·GÖ¢¶WÒ"’“°¢76W'B‡cæ6öçF–ç2‡vçB’Â&·GÖ¢cVÖ—G2·vçGÖ"“°¢cöf÷&×2çW6‚‡c“°¢Ð¢76W'EöW€¢cöf÷&×5³Òç&WÆ6R‚%³ÒÒ"Â%´TôeÒ"’À¢cöf÷&×5³ÒÀ¢'F†RGvò7VÆÆ–æw2F–ffW"öæÇ’–âF†RFö¶VâÂv†–6‚—2v‡’F†W’6†&RfW&F–7B ¢“° ¢òòæVvF—fRt”ED‚—2F–ffW&VçB&öÆRæBF¶W2F†B&öÆRw0¢òòfW&F–7BÂæ÷BF†RÆæRöæRà¢ÆWB6÷bÒf—‡GW&R‚&6÷eöW‡%÷F&vWG5÷FW7Bæ†&2"“°¢òò(
fæBF†RdU$D”5BÂæ÷B§W7BF†RÖW76vRâf—'7BfW'6–öâ76W'FV@¢òòöæÇ’6öçF–ç2‚'v–GF‚ÖÖWF†öBv–GF‚Ó‚"–Âv†–6‚7W'f—fW0¢òò&WÆ6–ærF†Rv†öÆR&Òv—F‚F†R–çfÆ–FF†R6öÖÖ—B7VæG2f÷W ¢òò&w&‡26ÆÆ–ærw&öærà¢ÆWBæVrÒÇC¢g7G'Â6÷bç&WÆ6Vâ‚&6÷fW"GWBæ6÷VçEö÷WE³3£ÕÆâ"Âff÷&ÖB‚&6÷fW"·GÕÆâ"’Â“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚fæVr‚&GWBæ6÷VçEö÷WE³3£Òç6W‡CÃÒƒâ‚’"’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥&V¦V7G2À¢“°¢76W'B†×6ræ6öçF–ç2‚'v–GF‚ÖÖWF†öBv–GF‚Ó‚"’Â'¶×6wÒ"“° ¢òòF†R÷F†W"Gvò&öÆW2F¶RF†V—"÷vâfW&F–7G2ÂæBæV—F†W"v0¢òò6÷fW&VBBÆÂà¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚fæVr‚&GWBæ6÷VçEö÷WE³Ò£Ò"’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚&&—B×6Æ–6R&÷VæBÓ"’Â'¶×6wÒ"“° ¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚fæVr‚"†GWBæ6÷VçEö÷WB2V–çCÃÒƒâ’"’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚&67Bv–GF‚Ó‚"’Â'¶×6wÒ"“°¢òòcG&÷267Bv–GF‚&F†W"F†â&W6öÇf–ær—BÂv†–6‚—2v‡¢òòF†B&öÆR—26–ÆVçFÇ”Ö—4Æ÷vW'6æBæ÷B&V¦V7G6à¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fæVr‚"†GWBæ6÷VçEö÷WB2V–çCÃÒƒâ’"’’¢æW‡V7B‚'cVÖ—G2—BÂv–GF‚æBÆÂ"“°¢76W'B€¢cæ6öçF–ç2‚"‡V–çCcE÷B’††&5÷'C£¦†&5÷&VB†GWBÓæ6÷VçEö÷WB’’"’À¢'cVÖ—G2Æ–âcBÖ&—B67C¢·cÒ ¢“°§Ð ¢òòòv–GF‚&÷fRcB&—G2—2$TeU4TBÂæ÷B6Æ×VB(	BæBâV&Æ–W ¢òòòfW'6–öâöbF†—2FW7B76W'FVBF†R6Æ×ÂöâF†R&wVÖVçBF†B&¢òòò6÷fW'ö–çB6×ÆW2cB&—G2Â6òv–FVæ–ær7B—B—2F†R–FVçF—G’"à¢òòð¢òòòF†B†öÆG2öæÇ’v†VâF†Rv–FVæVBfÇVR—26×ÆVBD•$T5DÅ’â6Æ–6P¢òòò—Bf—'7BæBc¶VW2&VÂ#‚Ö&—B–çFW&ÖVF–FRv†–ÆR6Æ×V@¢òòòD"Ô•"F‡&÷w2F†Rv–GF‚v’æB6†–gG2V–çCcE÷F7B—G2÷và¢òòòv–GFƒ ¢òòð¢òòòFW‡@¢òòò6÷fW"GWBæ6÷VçEö÷WE³3£Òç6W‡CÃ#ƒâ‚•³s£cUÐ¢òòòc¢†&5ö&—G2††&5÷6W‡E÷S#‚††&5ö&—G2‡bÃ2Ã’ÂBÂ#‚’ÂsÂcR¢òòòD"Ô•#¢‚‚‡V–çCcE÷B’†æ–&&ÆR6–vâÖW‡FVæFVBFòcB’’ãâcR’bƒ4`¢òòò ¢òòð¢òòòB6÷VçEö÷WBÒVF†÷6R&Rc2æBâ&÷F‚&6¶VæG2'V–ÆC²r²°¢òòòv&ç2öâF†RD"Ô•"6–FRæB„$26—2æ÷F†–ærBÆÂâF†R6Æ× ¢òòòGW&æVBâ67W&FRÒÖ6öFVvVâc7VvvW7F–öâ–çFò6–ÆVçFÇ’w&öæp¢òòò6×ÆRÂv†–6‚—2F†RöæR÷WF6öÖRF†—27vVWW†—7G2Fò&WfVçBà¢òòð¢òòòF†R&VgW6Ç27Æ—Bv†W&Rc7F÷2v÷&¶–æs ¢òòð¢òòòÂ6öç7G'V7BÂcÂfW&F–7BÀ¢òòòÂÒÒ×ÂÒÒ×ÂÒÒ×À¢òòòÂç6W‡CÃ#ƒâ‚–Â2V–çCÃ#ƒæÂö†&5÷S#†Â6ö×–ÆW2ÂVç7W÷'FVFÀ¢òòòÂ2V–çCÃ#æÂ†&5v–FSÃsæÂr²²&V¦V7G2F†R7F÷"ÂVÖ—G5Væ6ö×–Æ&ÆVÀ¢5·FW7EÐ¦fâö6÷fW'ö–çE÷v–GF…ö&÷fUócEö—5÷&VgW6VEö&V6W6U÷cö¶VW5÷F†UöW‡G&ö&—G2‚’°¢ÆWB6÷bÒf—‡GW&R‚&6÷eöW‡%÷F&vWG5÷FW7Bæ†&2"“°¢6öç7B5¢g7G"Ò&6÷fW"GWBæ6÷VçEö÷WE³3£ÕÆâ#°¢ÆWB6Æ–6RÒÇC¢g7G'Â6÷bç&WÆ6Vâ„5Âff÷&ÖB‚&6÷fW"·GÕÆâ"’Â“° ¢òòF†R–FVçF—G’6Æ–ÒÂfÇ6–f–VC¢cw26Æ–6VBf÷&Ò&VG2&—G2F†P¢òòcBÖ&—BÖöFVÂFöW2æ÷B†fRà¢ÆWB6Æ–6VBÒ6Æ–6R‚&GWBæ6÷VçEö÷WE³3£Òç6W‡CÃ#ƒâ‚•³s£cUÒ"“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚g6Æ–6VB’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&ç6W‡CÃ#ƒâ‚–"’Â'¶×6wÒ"“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g6Æ–6VB’’æW‡V7B‚'cVÖ—G2F†R#‚Ö&—B6Æ–6R"“°¢76W'B€¢cæ6öçF–ç2‚&†&5÷6W‡E÷S#‚"’bbcæ6öçF–ç2‚"‡V–çC3%÷B’ƒs’Â‡V–çC3%÷B’ƒcR’"’À¢'c6Æ–6W2&—G2s£cRöb&VÂ#‚Ö&—BfÇVS¢·cÒ ¢“° ¢òòWfW'’v–FVæ–ærf÷&Ò&VgW6W2ÂæBV6‚æÖW2—G6VÆbà¢f÷"‡BÂvçB’–â°¢‚&GWBæ6÷VçEö÷WE³3£Òç6W‡CÃ#ƒâ‚’"Â&ç6W‡CÃ#ƒâ‚–"’À¢‚&GWBæ6÷VçEö÷WE³3£Òç¦W‡CÃ#ƒâ‚’"Â&ç¦W‡CÃ#ƒâ‚–"’À¢‚&GWBæ6÷VçEö÷WE³3£Òç&W6—¦SÃ#ƒâ‚’"Â&ç&W6—¦SÃ#ƒâ‚–"’À¢‚"†GWBæ6÷VçEö÷WB2V–çCÃ#ƒâ’"Â&67BFò#‚&—G2"’À¢‚"†GWBæ6÷VçEö÷WB26–çCÃ#ƒâ’"Â&67BFò#‚&—G2"’À¢‚"†GWBæ6÷VçEö÷WB2V–çCÃcSâ’"Â&67BFòcR&—G2"’À¢Ò°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚g6Æ–6R‡B’’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‡vçB’Â&·GÖ¢¶×6wÒ"“°¢òò(
fæBF†R6Æ–Ò&V†–æBF†R7VvvW7F–öã¢c'V–ÆG2—Bà¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g6Æ–6R‡B’’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚&·GÖ¢c×W7BVÖ—B—Bf÷"F†R7VvvW7F–öâFò†öÆC¢¶WÒ"’“°¢Ð ¢òòG'Væ6—2F†RW†6WF–öã¢—Bä%$õu2Â6òv†WF†W"c66WG2—@¢òòFWVæG2öâF†R6÷W&6Rv–GF‚&F†W"F†âöâcBâöâF†RBÖ&—@¢òòæ–&&ÆR—B—2w&öærÖF—&V7F–öâ67BæB&÷F‚&6¶VæG2&VgW6R(	@¢òò–çfÆ–FÂæ÷BvÂæBæ÷BF†R&Ææ¶WB&V¦V7G6âV&Æ–W ¢òòfW'6–öâvfRWfW'’G'Væ6&÷fRcBöâF†R7G&VæwF‚öbF†—2öæP¢òò–çWBà¢ÆWBBÒ6Æ–6R‚&GWBæ6÷VçEö÷WE³3£ÒçG'Væ3Ã#ƒâ‚’"“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚gB’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚'v–GF‚×W7B&R7G&–7FÇ’ÆW72F†âF†R6÷W&6Rv–GF‚"’À¢'¶×6wÒ ¢“°¢ÆWBRÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gB’’æW‡V7EöW'"‚'c&VgW6W2—BFöò"“°¢76W'B€¢RçFõ÷7G&–ær‚¢æ6öçF–ç2‚'v–GF‚×W7B&R7G&–7FÇ’ÆW72F†âF†R6÷W&6Rv–GF‚"’À¢'cw2÷vâ'VÆR—2v†BÖ¶W2—B&öw&ÒW'&÷#¢¶WÒ ¢“° ¢òò&÷fR#‚c¶VW2v÷&¶–ærFöòÂ6òF†R7VvvW7F–öâ7F—2à¢òð¢òòF†—276W'FVBVÖ—G5Væ6ö×–Æ&ÆVf÷"f÷W"&÷VæG2öâF†R7G&VæwF€¢òòöbr²²&V¦V7F–ær†&5v–FSÃsã£¤†&5v–FR…õö–çC#‚Vç6–væVB–âF†@¢òò&V¦V7F–öâ—2â'F–f7BöbF†RfÆrF†R&ö&RW6VC¢†&5v–FVw0¢òò6öçfW'F–ær6öç7G'V7F÷"—2vFVBöâ7FC£¦—5ö–çFVw&Å÷cÅCæÂv†–6€¢òòÆ–'7FF2²²&W÷'G2dÅ4Rf÷"õö–çC#†VæFW"×7FCÖ2²³#æ@¢òòE%TRVæFW"×7FCÖvçR²³#(	BæB7&2öÖ–âç'6'V–ÆG2F†RVÖ—GFV@¢òòFW7F&Væ6‚v—F‚4duô5…„dÄu5õ5DCÒ×7FCÖvçR²³#à¢òòFW7G2÷v–FUö67Eö7ç'6Ç&VG’6–B6ò–â6öÖÖVçBà¢ÆWBv–FRÒ6Æ–6R‚"†GWBæ6÷VçEö÷WB2V–çCÃ#â’"“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚gv–FR’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&67BFò#&—G2"’Â'¶×6wÒ"“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gv–FR’’æW‡V7B‚'cVÖ—G2—B"“°¢76W'B‡cæ6öçF–ç2‚$†&5v–FSÃsâ"’Â'·cÒ"“° ¢òòF†RÆæwVvRÆ–Ö—B—2F†RöæRv–GF‚'VÆRF†B•2&öw&ÒW'&÷"À¢òòæBF†—2F‚6†V6¶VBæB7F÷VB(	B6òç¦W‡CÃ#â‚–v0¢òòFöÆBFò&R×'VâVæFW"cF†B&VgW6W2—Bà¢f÷"B–â°¢&GWBæ6÷VçEö÷WE³3£Òç¦W‡CÃ#â‚’"À¢&GWBæ6÷VçEö÷WE³3£Òç&W6—¦SÃ#â‚’"À¢Ò°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚g6Æ–6R‡B’’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚##BÖ&—BÆæwVvRÆ–Ö—B"’Â&·GÖ¢¶×6wÒ"“°¢ÆWBRÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g6Æ–6R‡B’’’æW‡V7EöW'"‚'c&VgW6W2—BFöò"“°¢76W'B†RçFõ÷7G&–ær‚’æ6öçF–ç2‚##BÖ&—BÆæwVvRÆ–Ö—B"’Â'¶WÒ"“°¢Ð ¢òòæBF†Rv–GF‚&wVÖVçB×W7B&RÆ–âÆ—FW&ÂÂv†–6‚F†RföÆ@¢òò†BV–WFÇ’v–FVæVC¢ç¦W‡CÃ²sâ‚–Æ÷vW&VBv†–ÆRc&VgW6VB—Bà¢f÷"B–â°¢&GWBæ6÷VçEö÷WE³3£Òç¦W‡CÃ²sâ‚’"À¢&GWBæ6÷VçEö÷WE³3£ÒçG'Væ3Ã²â‚’"À¢Ò°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚g6Æ–6R‡B’’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚'Æ–â–çFVvW"Æ—FW&Â"’Â&·GÖ¢¶×6wÒ"“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g6Æ–6R‡B’’’æW‡V7EöW'"‚'c&VgW6W2æöâÖÆ—FW&Âv–GF‚"“°¢Ð ¢òòv–GF‡2B÷"&VÆ÷rcB&RVçF÷V6†VB(	BF†R&VgW6Â—2æ÷B¢òò&Ææ¶WBöæRà¢f÷"B–â°¢&GWBæ6÷VçEö÷WE³3£Òç6W‡CÃcCâ‚’"À¢&GWBæ6÷VçEö÷WE³3£Òç¦W‡CÃƒâ‚’"À¢"†GWBæ6÷VçEö÷WB2V–çCÃcCâ’"À¢Ò°¢VÖ—Eö7÷7&2‚g6Æ–6R‡B’“°¢Ð§Ð ¢òòòf–ÆR×66÷R6öç7F26÷fW'ö–çB6Æ–6R&÷VæBâcVÖ—G2öæR0¢òòò‡V–çC3%÷B’„²–v–ç7B—G2÷vâ7FF–26öç7FW‡"¶(	B–FVçF–6À¢òòò6VÖçF–72FòF†RÆ—FW&Â(	Bv†–ÆRD"Ô•"&VgW6VBç—F†–ær'WBÆ–à¢òòò–çFVvW"à¢òòð¢òòòF†R&ö&RF†BW7F&Æ—6†VBF†—2×WFFW26÷eöW‡%÷F&vWG5÷FW7FÂ¢òòòf—‡GW&R$õD‚&6¶VæG2Ç&VG’72æBv†–6‚F†RWV—fÆVæ6R†&æW70¢òòòG&6RÖF–fg2â7F'F–ærg&öÒ&Vv—7FW&VBf—‡GW&R&F†W"F†â¢òòò7–çF†WF–2öæR—2v†BÖFRF†R&W7VÇBG'W7Gv÷'F‡“¢GvòV&Æ–W ¢òòò7–çF†WF–26÷fW&w&÷W&ö&W2ÖV7W&VBf–ÆW2–âv†–6‚æV—F†W"&6¶Væ@¢òòò†BVÖ—GFVBç’6×Æ–ærÆöv–2BÆÂÂ&V6W6R6÷fW&w&÷WF†B—0¢òòòæWfW"–ç7FçF–FVBVÖ—G2æöæRà¢5·FW7EÐ¦fâö6öç7Eö6÷fW'ö–çE÷6Æ–6Uö&÷VæEöföÆG2‚’°¢ÆWBf—‡GW&RÒ7FC£¦g3£§&VE÷Fõ÷7G&–ær€¢7FC£§Fƒ£¥Fƒ£¦æWr†Vçb‚$4$tõôÔä”dU5EôD•""’¢æ¦ö–â‚'FW7G2öf—‡GW&W2ö6÷eöW‡%÷F&vWG5÷FW7Bæ†&2"’À¢¢æW‡V7B‚'&VBF†R&Vv—7FW&VBf—‡GW&R"“°¢ÆWBv—F…ö6öç7BÒÆ³¢S3'Â°¢f÷&ÖB‚&6öç7B²Ò¶·ÕÆåÆâ"¢²ff—‡GW&Rç&WÆ6R€¢&6÷fW"GWBæ6÷VçEö÷WE³3£ÕÆâ"À¢&6÷fW"GWBæ6÷VçEö÷WE´³£ÕÆâ"À¢¢Ó° ¢òòF†RföÆBv—fW2'—FRÖ–FVçF–6Â÷WGWBFòF†RÆ—FW&Â&÷VæN(
`¢ÆWBÆ—FW&ÂÒVÖ—Eö7÷7&2‚ff—‡GW&R“°¢ÆWB³2ÒVÖ—Eö7÷7&2‚gv—F…ö6öç7Bƒ2’“°¢76W'EöW€¢Æ—FW&ÂÂ³2À¢&´³£Öv—F‚6öç7B²Ò6×W7BÆ÷vW"W†7FÇ’Æ–¶R³3£Ö ¢“° ¢òò(
fæBF†RdÅTR—2vVçV–æVÇ’W6VBÂ6òF†RWVÆ—G’&÷fR—2æ÷@¢òòF†RföÆB6–ÆVçFÇ’G&÷–ærF†R&÷VæBà¢ÆWB³rÒVÖ—Eö7÷7&2‚gv—F…ö6öç7Bƒr’“°¢76W'EöæR†³2Â³rÂ&F–ffW&VçB6öç7B×W7B&öGV6RF–ffW&VçBÖ6²"“°§Ð ¢òòò&Vv&Æö6²&Vv—7FW"ÆFG#æöfg6WBæB&W6WFfÇVRæ÷rdôÄBÀ¢òòòF‡&÷Vv‚F†R6ÖR†VÇW"2F†RFG&Ö&6RæB6—¦RâÆ–¶RF†÷6RÀ¢òòòF†—2WG2D"Ô•"†VBöbc&F†W"F†âÆWfVÂv—F‚—C¢cföÆG2&÷F€¢òòòFò¤U$òâF†Röfg6WB66R—2F†Rv÷'6RöbF†RGvò(	BF†RFG&W72D$ÄP¢òòòVçG'’&V6öÖW2²%5$2"ÂÂ3"ÖæBF†RFV6öFR&V6öÖW2FG"ÓÒÀ¢òòò6òF†R&Vv—7FW"Æ–6W2v†FWfW"Æ—fW2Böfg6WBæB—G2&VG2æ@¢òòòw&—FW26–ÆVçFÇ’†—BF–ffW&VçB&Vv—7FW"à¢òòð¢òòò&ö&VB'’×WFF–ær&Vv&Æö6µ÷7V'6WE÷FW7F(	B&Vv—7FW&VBÂæB76–æp¢òòòVæFW"&÷F‚&6¶VæG2(	BöæRFö¶VâBF–ÖRÂ6òF†R6öçG&öÂ—0¢òòò¶æ÷vâÖvööB&F†W"F†â7–çF†WF–2à¢5·FW7EÐ¦fâö6öç7E÷&Vv&Æö6µööfg6WEö÷%÷&W6WEöföÆG2‚’°¢ÆWBf—‡GW&RÒ7FC£¦g3£§&VE÷Fõ÷7G&–ær€¢7FC£§Fƒ£¥Fƒ£¦æWr†Vçb‚$4$tõôÔä”dU5EôD•""’¢æ¦ö–â‚'FW7G2öf—‡GW&W2÷&Vv&Æö6µ÷7V'6WE÷FW7Bæ†&2"’À¢¢æW‡V7B‚'&VBF†R&Vv—7FW&VBf—‡GW&R"“° ¢òò6öçG&öÃ¢F†RVæ×WFFVBf—‡GW&RÆ÷vW'2Â6òF†R×WFF–öç2&VÆ÷p¢òò&V6‚F†R&Vv&Æö6²&×2&F†W"F†âG&—–ærâV&Æ–W"vFRà¢ÆWBÆ—FW&ÂÒVÖ—Eö7÷7&2‚ff—‡GW&R“° ¢ÆWBöfg6WBÒÇ&S¢g7G"Âöfc¢g7G'Â°¢f÷&ÖB€¢'·&W×·Ò"À¢f—‡GW&Rç&WÆ6R€¢'&Vv—7FW"5$2ƒ‚66W72'r"À¢ff÷&ÖB‚'&Vv—7FW"5$2¶öfgÒ66W72'r"’À¢¢¢Ó°¢ÆWB&W6WBÒÇ&S¢g7G"Â'c¢g7G'Â°¢f÷&ÖB€¢'·&W×·Ò"À¢f—‡GW&Rç&WÆ6R€¢'&Vv—7FW"5E$Âƒ&W6WBr66W72'r"À¢ff÷&ÖB‚'&Vv—7FW"5E$Âƒ&W6WB·'gÒ66W72'r"’À¢¢¢Ó° ¢òòWfW'’föÆF–ær7VÆÆ–æröbF†Röfg6WBÆ÷vW'2W†7FÇ’Æ–¶RF†P¢òòÆ—FW&Â—B6ö×WFW>(
`¢f÷"‡&RÂöfb’–â°¢‚&6öç7B²Òƒ…ÆåÆâ"Â$²"’À¢‚""Â#ƒ²ƒ‚"’À¢‚&6öç7B²Òƒ…ÆåÆâ"Â#ƒ²²"’À¢Ò°¢76W'EöW€¢VÖ—Eö7÷7&2‚föfg6WB‡&RÂöfb’’À¢Æ—FW&ÂÀ¢&¶öfgÖ×W7BÆ÷vW"W†7FÇ’Æ–¶Rƒ† ¢“°¢Ð¢òò(
fæB6òFöW2F†R&W6WBfÇVRà¢76W'EöW€¢VÖ—Eö7÷7&2‚g&W6WB‚&6öç7B"ÒuÆåÆâ"Â%""’’À¢Æ—FW&ÂÀ¢&&W6WB&v—F‚6öç7B"Òv×W7BÆ÷vW"W†7FÇ’Æ–¶R&W6WBv ¢“° ¢òò&÷F‚fÇVW2vVçV–æVÇ’ÖGFW"Â6òF†RWVÆ—F–W2&÷fR&RF†P¢òòföÆG2v÷&¶–ær&F†W"F†âF†RfÇVW2&V–ærG&÷VBà¢76W'EöæR€¢VÖ—Eö7÷7&2‚föfg6WB‚&6öç7B²Òƒ5ÆåÆâ"Â$²"’’À¢Æ—FW&ÂÀ¢&F–ffW&VçBöfg6WB×W7B6†ævRF†R÷WGWB ¢“°¢76W'EöæR€¢VÖ—Eö7÷7&2‚g&W6WB‚&6öç7B"Ò•ÆåÆâ"Â%""’’À¢Æ—FW&ÂÀ¢&F–ffW&VçB&W6WB×W7B6†ævRF†R÷WGWB ¢“° ¢òòcw26–FRÂv—F‚&÷F‚æ6†÷'2âF†R6öç7Böfg6WBVÖ—G2F†RF&ÆP¢òòVçG'’Ä•DU$Â¤U$òöfg6WBv÷VÆN(
`¢ÆWBcÒÇ7&3¢g7G'Â7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‡7&2’’æW‡V7B‚'cVÖ—G2"“°¢ÆWBföÆFVBÒc‚föfg6WB‚&6öç7B²Òƒ…ÆåÆâ"Â$²"’“°¢76W'B€¢föÆFVBæ6öçF–ç2‡"2'²%5$2"ÂÂ3"Ò"2’À¢'cföÆG2F†R6öç7Böfg6WBFò¥Æç¶föÆFVGÒ ¢“°¢76W'B€¢c‚g&W6WB‚&6öç7B"ÒuÆåÆâ"Â%""’’æ6öçF–ç2‚'V–çC3%÷B5E$ÂÒ²"’À¢'cföÆG2F†R6öç7B&W6WBFò ¢“°¢òò(
fæBäõBF†RöæR—Bv2w&—GFVâ2Âv†–6‚—2v†BÖ¶W2F†RföÆ@¢òò6–ÆVçB'Vr&F†W"F†â7VÆÆ–ærF–ffW&Væ6Rà¢ÆWBÆ—FW&ÂÒc‚ff—‡GW&R“°¢76W'B€¢Æ—FW&Âæ6öçF–ç2‡"2'²%5$2"Âƒ‚Â3"Ò"2’À¢'F†RÆ—FW&Âöfg6WB7W'f—fW2Â6òF†RfÇVRvVçV–æVÇ’ÖGFW'3¥Æç¶Æ—FW&ÇÒ ¢“°§Ð ¢òòòF†R&Vv&Æö6²föÆG266WB4ôå5DåBW‡&W76–öç2Âæ÷B&&—G&'’öæW2À¢òòòæBF†RfÇVW2F†W’Fò66WB&R7F–ÆÂ6†V6¶VBâF†RF‡&VR÷WF6öÖW0¢òòò&RFVÆ–&W&FVÇ’F–ffW&VçBW'&÷"¶–æG2Â&V6W6RF†W’&RF‡&VP¢òòòF–ffW&VçB&ö&ÆV×2à¢5·FW7EÐ¦fâå÷VæföÆF&ÆUö÷%ö÷WEööe÷&ævU÷&Vv&Æö6µ÷fÇVUö—5÷7F–ÆÅ÷&V¦V7FVB‚’°¢ÆWBf—‡GW&RÒ7FC£¦g3£§&VE÷Fõ÷7G&–ær€¢7FC£§Fƒ£¥Fƒ£¦æWr†Vçb‚$4$tõôÔä”dU5EôD•""’¢æ¦ö–â‚'FW7G2öf—‡GW&W2÷&Vv&Æö6µ÷7V'6WE÷FW7Bæ†&2"’À¢¢æW‡V7B‚'&VBF†R&Vv—7FW&VBf—‡GW&R"“°¢VÖ—Eö7÷7&2‚ff—‡GW&R“° ¢ÆWBöfg6WBÒÇ&S¢g7G"Âöfc¢g7G'Â°¢f÷&ÖB€¢'·&W×·Ò"À¢f—‡GW&Rç&WÆ6R€¢'&Vv—7FW"5$2ƒ‚66W72'r"À¢ff÷&ÖB‚'&Vv—7FW"5$2¶öfgÒ66W72'r"’À¢¢¢Ó° ¢òòæ÷B6öç7FçBBÆÂâcF¶W2—BæBVÖ—G2öfg6WBÂ6òF†—0¢òò7F—26–ÆVçFÇ”Ö—4Æ÷vW'2&F†W"F†âö–çF–ærBcà¢ÆWBW'"ÒÆ÷vW%÷7&2‚föfg6WB‚""Â&GWBæ6÷VçEö÷WB"’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2“°¢76W'B†×6ræ6öçF–ç2‚&æöâÖ6öç7FçBÆFG#æöfg6WB"’Â'¶×6wÒ"“° ¢òò$$RfW&–Æör×6—¦VBÆ—FW&Â—2F†RöæR6†R†W&RcvWG2&–v‡BÀ¢òòæBD"Ô•"FöW2æ÷BÆ÷vW"F†VÒ„U$R(	BÆWB¢Ò3"vƒ†—2&VgW6V@¢òòF†R6ÖRv’ÂF†÷Vv‚¶VW6öç7G&–çBÆ÷vW'2F†VÒVæFW"&÷F€¢òò&6¶VæG2â6ò—B7F—2Vç7W÷'FVFÂö–çF–ærBcÀ¢òòæB×W7Bæ÷B&R7vWB–âv—F‚F†RföÆB×Fò×¦W&ò6†W2&÷VæB—Bà¢ÆWBW'"ÒÆ÷vW%÷7&2‚föfg6WB‚""Â#3"vƒ‚"’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fW'"“°¢76W'B†×6ræ6öçF–ç2‚&—2fW&–Æör×6—¦VBÆ—FW&Â"’Â'¶×6wÒ"“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚föfg6WB‚""Â#3"vƒ‚"’’¢æW‡V7B‚'cVÖ—G26—¦VBÆ—FW&Â"¢æ6öçF–ç2‡"2'²%5$2"Âƒ‚Â3"Ò"2’À¢'cÆ÷vW'2&&R6—¦VBÆ—FW&ÂFòF†R&–v‡Böfg6WBÂv†–6‚—2v‡’F†—2ö–çG2B—B ¢“° ¢òò(
f'WBôäÅ’F†R&&R4•¤TBf÷&ÒÂæBF†R&Òæ'&÷w2Gv–6Rf÷"Gvð¢òòF–ffW&VçB&V6öç2âcw25ö–çEöÆ—FW&Åög&öÖÖF6†W0¢òòW‡$¶–æC£¤–çFæBæ÷F†–ærVÇ6RÂ6ò6—¦VBÆ—FW&Â–ç6–FRà¢òòW‡&W76–öâ(	B÷"ÖW&VÇ’&VçF†W6—6VB(	B†—G2—G2#&&Ó²æBà¢òò÷fW"×v–FRÆ—FW&ÂÂVç&VF&ÆR'’F†R6ÖR'6W"Â&V6öÖW2¢òòö†&5÷S#†6ö×÷6—FRF†BG'Væ6FW2–çFòF†RcBÖ&—BF&ÆP¢òòf–VÆBÂv†–6‚—2öfg6WBv–ââö–çF–ærBcf÷"ç’öbF†W6P¢òòv÷VÆB&RfÇ6R&öÖ—6Rà¢f÷"öfb–â²#3"vƒ²ƒ‚"Â"ƒ3"vƒ‚’"Â#ƒ%Ò°¢ÆWBW'"ÒÆ÷vW%÷7&2‚föfg6WB‚""Âöfb’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2“°¢76W'B€¢×6ræ6öçF–ç2‚&æöâÖ6öç7FçBÆFG#æöfg6WB"’À¢&¶öfgÖ¢¶×6wÒ ¢“°¢ÆWBcö÷WBÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚föfg6WB‚""Âöfb’’’æW‡V7B‚'cVÖ—G2"“°¢ÆWBVçG'’Òcö÷W@¢æÆ–æW2‚¢æf–æB‡ÆÇÂÂæ6öçF–ç2‡"2"%5$2""2’¢çVçw&ö÷%öVÇ6R‡ÇÂæ–2‚&æò5$2F&ÆRVçG'“¥Æç·cö÷WGÒ"’“°¢76W'B€¢VçG'’æ6öçF–ç2‚#ƒ‚"’À¢'cFöW2æ÷B&öGV6Röfg6WBƒ‚f÷"¶öfgÖÂv†–6‚—2v‡’F†—2×W7BäõBÀ¢ö–çBB—C¢¶VçG'—Ò ¢“°¢Ð ¢òòföÆF&ÆR'WBæVvF—fS¢âFG&W726ææ÷B&RÂVæFW"ç’&6¶VæBà¢ÆWBW'"ÒÆ÷vW%÷7&2‚föfg6WB‚&6öç7B²ÒÒ…ÆåÆâ"Â$²"’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fW'"“°¢76W'B†×6ræ6öçF–ç2‚&×W7Bæ÷B&RæVvF—fR"’Â'¶×6wÒ"“° ¢òòâVçG—VB6öç7FföÆG24”täTBÂ6òfÇVRB÷"&÷fR%ãc0¢òòÆæG2–âF†B6ÖR&ÒâF†RÖW76vRæÖW2F†RW66R†F6‚Âæ@¢òòF†RW66R†F6‚v÷&·2à¢ÆWBW'"ÒÆ÷vW%÷7&2‚föfg6WB‚&6öç7B²ÒƒƒÆåÆâ"Â$²"’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fW'"“°¢76W'B†×6ræ6öçF–ç2‚&6öç7BäÔR¢V–çCÃcCâÒââæ"’Â'¶×6wÒ"“°¢Æ÷vW%÷7&2‚föfg6WB‚&6öç7B²¢V–çCÃcCâÒƒƒÆåÆâ"Â$²"’¢æW‡V7B‚&V–çCÃcCâ6öç7B7VÆÇ2ãÓ%ãc2FG&W72"“° ¢òòdõ%t$B&VfW&Væ6RföÆG3¢&Vv&Æö6·2Æ÷vW"gFW"F†Rv†öÆP¢òò6öç7FçBF&ÆR—2'V–ÇBÂ6òF†RFV6Æ&F–öâÖ÷&FW"6fVBF†@¢òòÆ–W2–ç6–FR6öç7F–æ—F–Æ—¦W"×W7Bæ÷B&R&WVFVB†W&Rà¢Æ÷vW%÷7&2‚ff÷&ÖB‚'·ÕÆæ6öç7B²Òƒ…Æâ"Âöfg6WB‚""Â$²"’’¢æW‡V7B‚&f÷'v&B6öç7B&VfW&Væ6RföÆG2BâFG&W726—FR"“°¢ÆWBW'"ÒÆ÷vW%÷7&2‚föfg6WB‚""Â$äõR"’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fW'"“°¢76W'B€¢×6ræ6öçF–ç2‚&FV6Æ&F–öâ÷&FW""’À¢'F†RVæ¶æ÷vâÖæÖRÖW76vR×W7Bæ÷B&ÆÖR÷&FW&–ær†W&S¢¶×6wÒ ¢“° ¢òòföÆF&ÆR'WBv–FW"F†âF†R&Vv—7FW"âÆ—FW&Â7BF†Rv–GF‚v0¢òò&Wf–÷W6Ç’6Vv‡B'’2²²æ'&÷v–æs²föÆF–ærÖ¶W2—B&V6†&ÆP¢òòv—F‚6öç7FÂ6òÆ÷vW&–ær6†V6·2—Bà¢ÆWBW'"ÒÆ÷vW%÷7&2‚ff÷&ÖB€¢&6öç7B"ÒC#“C“cs#“eÆåÆç·Ò"À¢f—‡GW&Rç&WÆ6R€¢'&Vv—7FW"5E$Âƒ&W6WBr66W72'r"À¢'&Vv—7FW"5E$Âƒ&W6WB"66W72'r"À¢¢’¢çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fW'"“°¢76W'B†×6ræ6öçF–ç2‚&FöW2æ÷Bf—B—G23"Ö&—Bv–GF‚"’Â'¶×6wÒ"“°§Ð ¢òòò6öç7FFVfVÇBöâG&ç67F–öæò7G'V7Ff–VÆBæ÷rföÆG2âc¢òòòVÖ—G2öæR2V–çCcE÷BÒ³¶v–ç7B—G2÷vâ7FF–26öç7FW‡"¶À¢òòòv†–6‚—24õ%$T5B(	BVæÆ–¶RF†RFG&ÖæB&Vv&Æö6²öfg6WG2Âv†W&RF†P¢òòò6ÖRÆ—FW&Ç2ÖöæÇ’Æö6ÂföÆFW"6—G2–âg&öçBöbcF†BföÆG2Fð¢òòò¤U$òâf÷W"–ç7Fæ6W2öböæR6öFRGFW&âÂF‡&VRF–ffW&VçBc¢òòò&V†f–÷W'2&V†–æB—BÂv†–6‚—2v‡’V6‚v2&ö&VB6W&FVÇ’&F†W ¢òòòF†â6Æ76–f–VB'’æÆöw’à¢5·FW7EÐ¦fâö6öç7E÷&V6÷&Eöf–VÆEöFVfVÇEöföÆG2‚’°¢ÆWBf—‡GW&RÒ7FC£¦g3£§&VE÷Fõ÷7G&–ær€¢7FC£§Fƒ£¥Fƒ£¦æWr†Vçb‚$4$tõôÔä”dU5EôD•""’¢æ¦ö–â‚'FW7G2öf—‡GW&W2÷&V6÷&EöÆWEö6÷•÷FW7Bæ†&2"’À¢¢æW‡V7B‚'&VBF†R&Vv—7FW&VBf—‡GW&R"“°¢6öç7BôÄC¢g7G"Ò&¢V–çCÃƒâFVfVÇB#° ¢ÆWBv—F‚ÒÆFV6Ã¢g7G"Â&S¢g7G'Â°¢f÷&ÖB‚'·&W×·Ò"Âf—‡GW&Rç&WÆ6R„ôÄBÂFV6Â’¢Ó° ¢òò6öçG&öÃ¢F†RVæ×WFFVB&Vv—7FW&VBf—‡GW&RÆ÷vW'2à¢VÖ—Eö7÷7&2‚ff—‡GW&R“° ¢ÆWBÆ—FW&ÂÒVÖ—Eö7÷7&2‚gv—F‚‚&¢V–çCÃƒâFVfVÇB’"Â""’“°¢ÆWB³’ÒVÖ—Eö7÷7&2‚gv—F‚‚&¢V–çCÃƒâFVfVÇB²"Â&6öç7B²Ò•ÆåÆâ"’“°¢76W'EöW€¢Æ—FW&ÂÂ³’À¢&FVfVÇB¶v—F‚6öç7B²Ò–×W7BÆ÷vW"W†7FÇ’Æ–¶RFVfVÇB– ¢“° ¢òòF†RfÇVR—2vVçV–æVÇ’W6VBÂ6òF†RWVÆ—G’—2F†RföÆBv÷&¶–æp¢òò&F†W"F†âF†RFVfVÇB&V–ærG&÷VBà¢ÆWB³2ÒVÖ—Eö7÷7&2‚gv—F‚‚&¢V–çCÃƒâFVfVÇB²"Â&6öç7B²Ò5ÆåÆâ"’“°¢76W'EöæR†³’Â³2Â&F–ffW&VçB6öç7B×W7B6†ævRF†RVÖ—GFVBFVfVÇB"“°§Ð ¢òòò'W2ç'6w2&VÖ–æ–ær÷WBÖöb×7V'6WB&×2ÂWfW'’öæRöbF†VÒ&ö&VB'¢òòò×WFF–ær$Tt•5DU$TBf—‡GW&R†FÆÕöÖWF†öEö'W5÷FW7FÀ¢òòò7G&VÕö'W'7EöÖöå÷FW7F’öæRFö¶VâBF–ÖRâf—fRöbF†R6—‚GW&à¢òòò÷WBFò&R6†W2c&V¦V7G2Föò(	B—G2G'•öVÖ—Eö'W5÷FÆÕöf÷&¶æ@¢òòòVÖ—Eö'W5ö6ÆÆ6''’F†R6ÖRwV&G2Â–âF†R6ÖR÷&FW"Âv—F‚F†P¢òòò6ÖRw&çVÆ&—G’(	B6òæöæRöbF†VÒ—2cW66R†F6‚âF†R6—‡F€¢òòò†Ò&–æBÆæöâÖGWCæ’—2F†R÷÷6—FS¢c66WG2—BæBVÖ—G0¢òòòVæ6ö×–Æ&ÆR2²²à¢òòð¢òòòv—F‚F†W6RÂ7&2ö—"öÆ÷vW"ö'W2ç'6†2æòÆ÷vW$W'&÷#£¥Vç7W÷'FVF ¢òòò6—FRÆVgBà¢5·FW7EÐ¦fâF†U÷&VÖ–æ–æuö'W5÷6†W5ö&Uöæ÷E÷cöW66Uö†F6†W2‚’°¢ÆWBFÆÒÒf—‡GW&R‚'FÆÕöÖWF†öEö'W5÷FW7Bæ†&2"“°¢6öç7Bdõ$´TC¢g7G"Ò&ÆWBf÷&¶VCÒf÷&²ÖVÒç&VEöööòƒ’’#° ¢òò6öçG&öÃ¢F†RVæ×WFFVBf—‡GW&RÆ÷vW'2VæFW"&÷F‚&6¶VæG2Â6òF†P¢òò×WFF–öç2&VÆ÷r&V6‚F†R'W2&×2&F†W"F†ââV&Æ–W"vFRà¢VÖ—Eö7÷7&2‚gFÆÒ“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gFÆÒ’’æW‡V7B‚'cVÖ—G2F†R6öçG&öÂ"“° ¢òòD•$T5B†æöâÖf÷&¶’6ÆÂöââ÷WEööeö÷&FW&ÖWF†öBâF†R'6W ¢òòFÖ—G2öæÇ’&Æö6¶–ævæB÷WEööeö÷&FW&Â6òF†—2—2F†R6öÆRv¢òòFò&V6‚Æ÷vW%÷FÆÕöÖWF†öEö6ÆÆw2ÖöFRwV&Bà¢òð¢òòF†RF‡&VRf÷&¶$…26†RwV&G2föÆÆ÷s¢æ÷B6ÆÂÂ6ÆÂv†÷6P¢òò6ÆÆVR—2&&R–FVçBÂæBf–VÆB6†–âF†B—2æ÷B&ö÷FVBB¢òò'W2&–æF–ærà¢f÷"†×WFF–öâÂæVVFÆR’–â°¢€¢&ÆWBf÷&¶VCÒÖVÒç&VEöööòƒ’’"À¢&÷WEööeö÷&FW&FÆÕöÖWF†öB6ÆÇ2"À¢’À¢‚&ÆWBf÷&¶VCÒf÷&²’"Â&æ÷BF—&V7B'W2FÆÕöÖWF†öB6ÆÂ"’À¢€¢&ÆWBf÷&¶VCÒf÷&²&VEöööòƒ’’"À¢&æ÷BÆ'W3âãÆÖWF†öCâ†&w2–"À¢’À¢€¢&ÆWBf÷&¶VCÒf÷&²ÖVÒæ–ææW"ç&VEöööòƒ’’"À¢&æ÷B&ö÷FVBB'W2&–æF–ær"À¢’À¢Ò°¢ÆWB7&2ÒFÆÒç&WÆ6R„dõ$´TBÂ×WFF–öâ“°¢ÆWBW'"ÒÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥&V¦V7G2“°¢76W'B†×6ræ6öçF–ç2†æVVFÆR’Â&¶×WFF–öçÖ¢¶×6wÒ"“°¢òòF†Ræ6†÷"F†BÖ¶W2&V¦V7G66Æ–ÒæBæ÷Bâ77V×F–öã ¢òòc&VgW6W2F†R6ÖR6÷W&6Rà¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’æ—5öW'"‚’À¢'c×W7B&V¦V7B¶×WFF–öçÖFöòÂ÷"F†—2—2&VÂv ¢“°¢Ð ¢òò6†ææVÂÖWF†öB÷WG6–FR6VæFò&V7fâ7G&VÕö'W'7EöÖöå÷FW7F ¢òòFV6Æ&W2—G2'W2Æö6ÆÇ’Â6òæòW6V&W6öÇWF–öâ—2æVVFVC²FF–æp¢òò&V7b‚–Fò—B—2—G6VÆb6öçG&öÂ†&÷F‚&6¶VæG2VÖ—B’Âv†–6€¢òò–ç2F†R&V¦V7F–öç2&VÆ÷röâF†RÔUD„ôBäÔR&F†W"F†âöâF†P¢òò6ÆÂ6—FR&V–æræWrà¢òð¢òòF†—2&Ò—2VÖ—G5Væ6ö×–Æ&ÆV&F†W"F†â&V¦V7G6ÂæBF†RGvð¢òò&ö&W2&VÆ÷r&Rv‡“¢cw2&V†f–÷W"7Æ—G2öâv†WF†W"F†RæÖP¢òò†Vç2Fò&R6†ææVÂ4”täÂÂæBöæÇ’F†Rv÷'6R†Æb6WG2F†P¢òò7FGW2à¢ÆWB7G&VÒÒf—‡GW&R‚'7G&VÕö'W'7EöÖöå÷FW7Bæ†&2"“°¢6öç7Bt•C¢g7G"Ò"v—B"7–6ÆW2#°¢ÆWBv—F…ö6ÆÂÐ¢ÆÓ¢g7G'Â7G&VÒç&WÆ6R…t•BÂff÷&ÖB‚"ÆWBBÒ7G&Òç2ç¶×Ò‚•Æçµt•GÒ"’“° ¢VÖ—Eö7÷7&2‚gv—F…ö6ÆÂ‚'&V7b"’“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gv—F…ö6ÆÂ‚'&V7b"’’’æW‡V7B‚'cVÖ—G26†ææVÂ&V7b"“° ¢f÷"Ò–â²'ö¶R"Â&FF%Ò°¢ÆWB7&2Òv—F…ö6ÆÂ†Ò“°¢ÆWBW'"ÒÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¤VÖ—G5Væ6ö×–Æ&ÆR“°¢76W'B€¢×6ræ6öçF–ç2‚ff÷&ÖB‚&'W26†ææVÂÖWF†öBç¶×Ò‚âââ–"’’À¢'¶×6wÒ ¢“°¢Ð ¢òòçö¶V—2æ÷B6–væÂöâ6Â6òc&W6öÇfW2—Bv–ç7BF†P¢òò6†ææVÂw26–væÂÆ—7BæB&VgW6W2(	Bv—F‚&WGFW"ÖW76vRF†à¢òò÷W'2à¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gv—F…ö6ÆÂ‚'ö¶R"’’’æ—5öW'"‚’À¢'c&V¦V7G2ÖWF†öBæÖRF†B—2æ÷B6†ææVÂ6–væÂ ¢“°¢òòæFF•26–væÂÂ6òcVÖ—G2(	BæBVÖ—G26–væÂ$TBv—F€¢òòF†R6ÆÂ&Vç27F–ÆÂGF6†VBâ†&5÷&VF&WGW&ç2fÇVRÂ6ð¢òò†&5÷&VB‚âââ’‚–—2&W‡&W76–öâ6ææ÷B&RW6VB2gVæ7F–öâ"à¢òòF†B†Æb—2v†BÖ¶W2F†R&ÒVÖ—G5Væ6ö×–Æ&ÆRà¢ÆWBFFÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gv—F…ö6ÆÂ‚&FF"’’¢æW‡V7B‚'c66WG26†ææVÂÖWF†öBF†BæÖW26–væÂ"“°¢76W'B€¢FFæ6öçF–ç2‚&†&5÷'C£¦†&5÷&VB†GWBÓç7G&Õ÷5öFF’‚’"’À¢'cÆVfW2F†R6ÆÂ&Vç2öâ6–væÂ&VC¥Æç¶FFÒ ¢“°§Ð ¢òòòÆWBÆ#â¢Ä'W3âÒ&–æBÇƒæv†W&R†—2æ÷BGWFâVæÆ–¶RWfW'¢òòò÷F†W"&Ò–â'W2ç'6ÂF†—2öæR—26†Rc44UE2(	BæBF†Và¢òòòVÖ—G22²²F†B6ææ÷B6ö×–ÆRâcFöW2æ÷B&W6öÇfRF†R&–æBF&vW@¢òòòBÆÃ¢—B7V'7F—GWFW2F†R&–æBU…$U54”ôâv†W&RF†REUBö–çFW ¢òòòvöW2æBFW&VfW&Væ6W2—BÂv†–6‚f–Ç2GvòF–ffW&VçBv—2FWVæF–æp¢òòòöâF†R6†RÂ6ò&÷F‚&R&ö&VBà¢5·FW7EÐ¦fâö'W5ö&÷VæE÷FõööæöåöGWE÷F&vWEöFöW5öæ÷E÷ö–çEöE÷c‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚'FÆÕöÖWF†öEö&Æö6¶–æuö'W5÷FW7Bæ†&2"“°¢6öç7B$”äC¢g7G"Ò#Ò&–æBGWB#°¢76W'B†f—‡GW&Ræ6öçF–ç2„$”äB’Â&f—‡GW&R6†R6†ævVB"“° ¢òò6öçG&öÃ¢F†R&Vv—7FW&VBf—‡GW&RÆ÷vW'2VæFW"&÷F‚&6¶VæG2à¢VÖ—Eö7÷7&2‚ff—‡GW&R“°¢ÆWBvööBÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚ff—‡GW&R’’æW‡V7B‚'cVÖ—G2F†R6öçG&öÂ"“°¢76W'B€¢vööBæ6öçF–ç2‚&GWBÓæÖVÕ÷&VEöFG""’À¢'F†R6öçG&öÂ&÷fW2F†R66W72—2&VÂÂæ÷BG&÷VC¥Æç¶vööGÒ ¢“° ¢ÆWB&÷VæE÷FòÒÇF&vWC¢g7G'Âf—‡GW&Rç&WÆ6R„$”äBÂff÷&ÖB‚#Ò&–æB·F&vWGÒ"’“°¢f÷"F&vWB–â²&æ÷R"Â&GWBæ6÷&R%Ò°¢ÆWBW'"ÒÆ÷vW%÷7&2‚f&÷VæE÷Fò‡F&vWB’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¤VÖ—G5Væ6ö×–Æ&ÆR“°¢76W'B†×6ræ6öçF–ç2‚&æöâÔEUBF&vWB"’Â&·F&vWGÖ¢¶×6wÒ"“°¢Ð ¢òò&&RæÖS¢c&–çG2—B2F†REUBö–çFW.(
`¢ÆWB&&RÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f&÷VæE÷Fò‚&æ÷R"’’’æW‡V7B‚'c66WG2æöâÔEUB&–æB"“°¢76W'B€¢&&Ræ6öçF–ç2‚&æ÷RÓæÖVÕ÷&VEöFG""’À¢'c&–çG2F†R&–æBæÖR2F†REUBö–çFW#¥Æç¶&&WÒ ¢“°¢òò(
fæBæWfW"'&–æw2—B–çFò66÷Rà¢76W'B€¢&&Ræ6öçF–ç2‚&æ÷RÒ"’À¢&–bcWfW"FV6Æ&W2æ÷VÂF†—2—2æòÆöævW"Væ6ö×–Æ&ÆS¥Æç¶&&WÒ ¢“° ¢òòf–VÆBFƒ¢cVÖ—G2&VÂEUBÖVÖ&W"&VBæBF†VâÆ–W0¢òò÷W&F÷"ÓæFò—Bâ†&5÷&VF&WGW&ç2dÅTRÂ6òF†—2—0¢òòVæ6ö×–Æ&ÆRf÷"F–ffW&VçB&V6öâF†âF†R&&RÖæÖR66R(	Bæ@¢òòF†R&V6öâF†RF–væ÷7F–2æÖW2&÷F‚à¢ÆWBf–VÆBÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f&÷VæE÷Fò‚&GWBæ6÷&R"’’’æW‡V7B‚'c66WG2f–VÆB&–æB"“°¢76W'B€¢f–VÆBæ6öçF–ç2‚&†&5÷'C£¦†&5÷&VB†GWBÓæ6÷&R’ÓæÖVÕ÷&VEöFG""’À¢'cFW&VfW&Væ6W2F†R&–æBW‡&W76–öâ—G6VÆc¥Æç¶f–VÆGÒ ¢“°§Ð ¢òòò6÷fW&w&÷W&–âw2U„5BdÅTRÖ’æ÷r&R'VçF–ÖRW‡&W76–öà¢òòò†¶GWBæVçÖ’Âæ÷BöæÇ’föÆFVB6öç7FçBâ&ævW2v÷B'VçF–ÖR&÷VæG0¢òòòf—'7BæBW†7BfÇVW2vW&RÆVgB&V†–æBÂ6ò¶GWBæVâââuÖv÷&¶V@¢òòòv†–ÆR¶GWBæVçÖv2&V¦V7FVB–âF†R6ÖR&–ç2&Æö6²(	B7Æ—Bc¢òòòæWfW"†BÂ6–æ6R—B&VæFW'2&÷F‚v—F‚F†R6ÖRVÖ—EöW‡&à¢òòð¢òòò&÷F‚f÷&×2æ÷r6''’6÷d&–ä&÷VæFÂ6òÆ÷vW%ö&–åö&÷VæF—2F†P¢òòò6–ævÆR–×ÆVÖVçFF–öâæBF†R6×ÆW"×7V'6WBF–væ÷7F–2—26†&VBà¢5·FW7EÐ¦fâ6÷fW&w&÷W÷'VçF–ÖUö&–å÷fÇVW5öÆ÷vW%öæEöVÖ—B‚’°¢ÆWB&örÒÆ÷vW%÷7&2‚ff—‡GW&R‚&6÷e÷'VçF–ÖUö&–å÷fÇVU÷FW7Bæ†&2"’’æW‡V7B‚&Æ÷vW'2"“°¢fW&–g“£§fW&–g•÷&öw&Ò‚g&ör’æW‡V7B‚'fW&–f–W2"“°¢ÆWB7ö6÷VçBÒg&öræ6÷fw&÷W5³Òçö–çG5³Ó°¢76W'EöW†7ö6÷VçBææÖRÂ&7ö6÷VçB"“° ¢òòWöVâÒ¶GWBæVçÖ(	B&&R÷'B&VBÂ6'&–VB2'VçF–ÖRà¢76W'EöW†7ö6÷VçBæ&–ç5³ÒææÖRÂ&WöVâ"“°¢76W'B€¢ÖF6†W2€¢f7ö6÷VçBæ&–ç5³ÒçfÇVW5³ÒÀ¢—#£¤6÷d&–åfÇVS£¤W†—#£¤6÷d&–ä&÷VæC£¥'VçF–ÖR†—#£¤W‡#£¥÷'B…ò’’¢’À¢&WöVâ×W7B&R'VçF–ÖR÷'B&VBÂv÷B³£÷Ò"À¢7ö6÷VçBæ&–ç5³ÒçfÇVW5³Ð¢“° ¢òòWöW‡"Ò¶GWBæVâ²GÖ(	BâW‡&W76–öâ÷fW"F†R÷'Bà¢76W'EöW†7ö6÷VçBæ&–ç5³ÒææÖRÂ&WöW‡""“°¢76W'B€¢ÖF6†W2€¢f7ö6÷VçBæ&–ç5³ÒçfÇVW5³ÒÀ¢—#£¤6÷d&–åfÇVS£¤W†—#£¤6÷d&–ä&÷VæC£¥'VçF–ÖR†—#£¤W‡#£¤&–æ'’‚ââ’’¢’À¢&WöW‡"×W7B&R'VçF–ÖR&–æ'’W‡"Âv÷B³£÷Ò"À¢7ö6÷VçBæ&–ç5³ÒçfÇVW5³Ð¢“° ¢òòWö6öç7BÒ³'Ö(	BF†R6öç7FçBf7BF‚—2Tä4„ätTBÂv†–6‚—0¢òòv†B¶VW2F†Rv–FVæ–ærg&öÒ&V–ær&Ww&—FRöbWfW'’&–âà¢76W'EöW†7ö6÷VçBæ&–ç5³%ÒææÖRÂ&Wö6öç7B"“°¢76W'B€¢ÖF6†W2€¢f7ö6÷VçBæ&–ç5³%ÒçfÇVW5³ÒÀ¢—#£¤6÷d&–åfÇVS£¤W†—#£¤6÷d&–ä&÷VæC£¤6öç7Bƒ"’¢’À¢&Wö6öç7B×W7B7F–ÆÂföÆBÂv÷B³£÷Ò"À¢7ö6÷VçBæ&–ç5³%ÒçfÇVW5³Ð¢“° ¢òòÖ—†VBÒ¶GWBæVâ²ÂWÖ(	BöæR6WBÂ&÷F‚¶–æG2Â–â÷&FW"à¢76W'EöW†7ö6÷VçBæ&–ç5³5ÒææÖRÂ&Ö—†VB"“°¢76W'EöW†7ö6÷VçBæ&–ç5³5ÒçfÇVW2æÆVâ‚’Â"“°¢76W'B†ÖF6†W2€¢f7ö6÷VçBæ&–ç5³5ÒçfÇVW5³ÒÀ¢—#£¤6÷d&–åfÇVS£¤W†—#£¤6÷d&–ä&÷VæC£¤6öç7BƒR’¢’“° ¢òòVÖ—76–öã¢F†R'VçF–ÖRfÇVR—26ö×&VBv–ç7BÆ—fR÷'B&V@¢òòB6×ÆRF–ÖRÂv†–6‚—2W†7FÇ’v†BcVÖ—G2à¢ÆWB7ÒVÖ—Eöf—‡GW&Uö7‚&6÷e÷'VçF–ÖUö&–å÷fÇVU÷FW7Bæ†&2"“°¢76W'B€¢7æ6öçF–ç2‚%÷bÓÒ†&5÷'C£¦†&5÷&VB†GWBÓæVâ’"’À¢&'VçF–ÖR&–âfÇVR×W7BVÖ—BÆ—fR÷'B&VC²v÷C¥Æç¶7Ò ¢“°§Ð ¢òòòF†RcWf–FVæ6R&V†–æBF†R&–â×fÇVRv–FVæ–ærÂv—F‚&÷F‚æ6†÷'2(	@¢òòòæBF†R&V6öâ—B—2v4Äõ4TB&F†W"F†âF–væ÷7F–0¢òòò&V6Æ76–f–VBà¢òòð¢òòò&ö&VB'’×WFF–ær6÷e÷'VçF–ÖUö&÷VæE÷FW7F‡&Vv—7FW&VBÂ76–æp¢òòòVæFW"&÷F‚&6¶VæG2’öæRFö¶VâBF–ÖRÂBWfW'’÷6—F–öââW†7@¢òòòfÇVR6â&Rw&—GFVâà¢5·FW7EÐ¦fâcö†5öÇv—5ö6ö×&VE÷'VçF–ÖUö&–å÷fÇVW5÷W%÷6×ÆR‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&6÷e÷'VçF–ÖUö&÷VæE÷FW7Bæ†&2"“°¢6öç7BÄ•C¢g7G"Ò&VãÒ³Ò#° ¢òò6öçG&öÃ¢F†RVæ×WFFVBf—‡GW&RVÖ—G2VæFW"&÷F‚&6¶VæG>(
`¢ÆWB7FÅ÷cÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚ff—‡GW&R’’æW‡V7B‚'cVÖ—G2F†R6öçG&öÂ"“°¢VÖ—Eö7÷7&2‚ff—‡GW&R“°¢òò(
fæB—G2Æ—FW&Â&–â&VæFW'226ö×&—6öâv–ç7BÂ6òF†P¢òòfÇVR—2vVçV–æVÇ’–âF†R÷WGWBæBF†RF–fg2&VÆ÷rÖV7W&R—Bà¢76W'B†7FÅ÷cæ6öçF–ç2‚"…÷bÓÒ’"’Â&6öçG&öÃ¥Æç¶7FÅ÷cÒ"“° ¢òòWfW'’÷6—F–öââW†7BfÇVR6âV#¢&&RÂ–âöæRÖVÆVÖVç@¢òò6WBÂÆöæw6–FRÆ—FW&ÂÂæB2âW‡&W76–öââF†RW‡V7FV@¢òòæVVFÆR—2W"×7VÆÆ–ær&V6W6RD"Ô•"&VçF†W6—6W26ö×÷Væ@¢òò7V"ÖW‡&W76–öâv†W&RcFöW2æ÷B(	B&VæFW&–ærF–ffW&Væ6RF†P¢òòWV—fÆVæ6R†&æW72G&6RÖF–fg27BÂæBöæRF†RW†—7F–æp¢òò'VçF–ÖRÔ$õTäBf—‡GW&RÇ&VG’6'&–W2à¢f÷"‡7VÆÆ–ærÂæVVFÆR’–â°¢‚&VãÒGWBæVâ"Â%÷bÓÒ†&5÷'C£¦†&5÷&VB†GWBÓæVâ’"’À¢‚&VãÒ¶GWBæVçÒ"Â%÷bÓÒ†&5÷'C£¦†&5÷&VB†GWBÓæVâ’"’À¢‚&VãÒ³ÂGWBæVçÒ"Â%÷bÓÒ†&5÷'C£¦†&5÷&VB†GWBÓæVâ’"’À¢‚&VãÒ¶GWBæVâ²Ò"Â&†&5÷'C£¦†&5÷&VB†GWBÓæVâ’²"’À¢Ò°¢ÆWB7&2Òf—‡GW&Rç&WÆ6R„Ä•BÂ7VÆÆ–ær“°¢òòcVÖ—G2Æ—fRW"×6×ÆR6ö×&—6öâ(	Bv÷&¶–ær6öFRÂæ÷B¢òò6†R—B6–ÆVçFÇ’G&÷>(
`¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’æW‡V7B‚'cVÖ—G2'VçF–ÖR&–âfÇVR"“°¢76W'B€¢cæ6öçF–ç2†æVVFÆR’À¢&·7VÆÆ–æwÖ×W7BVÖ—BÆ—fR÷'B&VBVæFW"c¥Æç·cÒ ¢“°¢òò(
fæB—BF–ffW'2g&öÒF†R6öçG&öÂÂ6òF†R&–âfÇVR&V6†W2F†P¢òò÷WGWB&F†W"F†âF†RGvò†Væ–ærFòw&VRà¢76W'EöæR‡cÂ7FÅ÷cÂ&·7VÆÆ–æwÖ×W7B6†ævRcw2÷WGWB"“°¢òòD"Ô•"æ÷rÖF6†W2–ç7FVBöb&V¦V7F–ærà¢76W'B€¢VÖ—Eö7÷7&2‚g7&2’æ6öçF–ç2†æVVFÆR’À¢&·7VÆÆ–æwÖ×W7BÆ÷vW"æBVÖ—BF†R6ÖR6ö×&—6öâVæFW"D"Ô•" ¢“°¢Ð§Ð ¢òòò'VçF–ÖR&–âÖVÖ&W'2æB&ævR&÷VæG2&RäõBVÖ—GFVB'—FRÖf÷"Ö'—FP¢òòòv—F‚cÂæBF†RF–ffW&Væ6R—2öæRv†W&Rc—2w&öærà¢òòð¢òòò6÷fW%öW‡%ö7&VçF†W6—6W26ö×÷VæBW‡&W76–öã²cw0¢òòòVÖ—EöW‡&FöW2æ÷Bâf÷"â÷W&F÷"&–æF–ærF–v‡FW"F†âF†P¢òòò6ö×&—6öâF†RGvòw&VR(	Bv†–6‚—2v‡’6÷e÷'VçF–ÖUö&–å÷fÇVU÷FW7F ¢òòò6âW6R¶æB6—B–âF†RWV—fÆVæ6R&Vv—7G'’âf÷"öæRF†BFöW0¢òòòæ÷BÂcVÖ—G2÷bÓÒ†&5÷&VB†GWBÓæVâ’Â†Âv†–6‚2²²w&÷W20¢òòò…÷bÓÒVâ’Â†(	Bæöâ×¦W&òöâWfW'’6×ÆRÂ6òF†R&–âÇv—2†—G2à¢òòð¢òòòF†—2&V6ÖR&V6†&ÆRöâF†RW†7B×fÇVRF‚v†Vâ'VçF–ÖRÖVÖ&W'0¢òòòÆæFVC²—Bv2Ç&VG’&V6†&ÆRöâF†R&ævRÖ&÷VæBF‚ÂæB&÷F€¢òòò&R–ææVB†W&R6ògWGW&R&Ö¶RF†VÒ'—FRÖ–FVçF–6Â"6†ævR†2Fð¢òòò6öæg&öçBF†R'Vr&F†W"F†âF÷B—Bà¢5·FW7EÐ¦fâöÆ÷u÷&V6VFVæ6Uö&–å÷fÇVUö—5ö÷Æ6U÷cö—5÷w&öær‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&6÷e÷'VçF–ÖUö&÷VæE÷FW7Bæ†&2"“° ¢òò6öçG&öÃ¢F†R&Vv—7FW&VBf—‡GW&RVÖ—G2VæFW"&÷F‚&6¶VæG2à¢VÖ—Eö7÷7&2‚ff—‡GW&R“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚ff—‡GW&R’’æW‡V7B‚'cVÖ—G2F†R6öçG&öÂ"“° ¢òòâ÷W&F÷"F†B&–æG2D”t…DU"F†âÓÖ¢F†RGvòw&VRÂ6òF†P¢òò&Vv—7G'’f—‡GW&Rw2¶&VÆÇ’—26fR&F†W"F†âVçFW7FVBà¢ÆWBF–v‡BÒf—‡GW&Rç&WÆ6R‚&VãÒ³Ò"Â&VãÒ¶GWBæVâ²‡Ò"“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gF–v‡B’¢æW‡V7B‚'cVÖ—G2"¢æ6öçF–ç2‚%÷bÓÒ†&5÷'C£¦†&5÷&VB†GWBÓæVâ’²‚"’À¢'cw&÷W2¶6÷'&V7FÇ’ÂæVVF–æræò&Vç2 ¢“°¢76W'B€¢VÖ—Eö7÷7&2‚gF–v‡B’æ6öçF–ç2‚&†&5÷'C£¦†&5÷&VB†GWBÓæVâ’²‚"’À¢%D"Ô•"6ö×WFW2F†R6ÖRfÇVR ¢“° ¢òòâ÷W&F÷"F†B&–æG2Äôõ4U#¢cw2&VæFW&–ær6†ævW2v†BF†P¢òò&–âÖVç>(
`¢ÆWBÆö÷6RÒf—‡GW&Rç&WÆ6R‚&VãÒ³Ò"Â&VãÒ¶GWBæVâÂ‡Ò"“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fÆö÷6R’¢æW‡V7B‚'cVÖ—G2"¢æ6öçF–ç2‚%÷bÓÒ†&5÷'C£¦†&5÷&VB†GWBÓæVâ’Â‚"’À¢'cVÖ—G2F†R&&RW‡&W76–öâÂv†–6‚2²²w&÷W22…÷bÓÒVâ’Â† ¢“°¢òò(
fæBD"Ô•"w2FöW2æ÷Bà¢76W'B€¢VÖ—Eö7÷7&2‚fÆö÷6R’æ6öçF–ç2‚%÷bÓÒ††&5÷'C£¦†&5÷&VB†GWBÓæVâ’Â‚’"’À¢%D"Ô•"&VçF†W6—6W2Â6òF†R6ö×&—6öâ—2v–ç7BF†RfÇVR ¢“° ¢òò6ÖRöâ&ævR$õTäBÂv†–6‚&VFFW2F†RW†7B×fÇVRv–FVæ–ærà¢ÆWB&÷VæBÒf—‡GW&Rç&WÆ6R‚'6VÅöÆòÒ¶GWBæVâââuÒ"Â'6VÅöÆòÒ¶GWBæVâÂ‚ââuÒ"“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f&÷VæB’¢æW‡V7B‚'cVÖ—G2"¢æ6öçF–ç2‚%÷bãÒ†&5÷'C£¦†&5÷&VB†GWBÓæVâ’Â‚"’À¢'c†2F†R6ÖR'Vröâ&ævR&÷VæB ¢“°¢76W'B€¢VÖ—Eö7÷7&2‚f&÷VæB’æ6öçF–ç2‚%÷bãÒ††&5÷'C£¦†&5÷&VB†GWBÓæVâ’Â‚’"’À¢%D"Ô•"&VçF†W6—6W2F†R&÷VæBFöò ¢“°§Ð ¢òòò&&RæÖR–â&–â7V2F†B—2æV—F†W"6öç7FöVçVÒf&–çBæ÷ ¢òòò†öö²&Ò¶VW2F†RÖW76vRF†B6—26ò(	B&÷WF–ær—BF‡&÷Vv‚F†P¢òòòvVæW&–26×ÆW"×7V'6WBÆ—7Bv÷VÆB&RÆW72&V6—6RæBv÷VÆBÆ&VÂ¢òòò&–â2'ö–çB"(	B'WB—BFöW2äõB¶VWF†RÒÖ6öFVvVâc ¢òòò7VvvW7F–öâÂv†–6‚v2æWfW"G'VRà¢òòð¢òòòcVÖ—G2F†RæÖR7G&–v‡B–çFò—G26ö×&—6öã¢–b‚‚…÷bÓÒäõR’’–à¢òòòf÷"âVæFV6Æ&VBæÖRF†B—2"täõRrv2æ÷BFV6Æ&VB–âF†—0¢òòò66÷R#²f÷"öæRF†BÇ6òæÖW2Ö7&ò—B—2v÷'6RÂ&V6W6P¢òòò÷bÓÒTôf4ôÕ”ÄU2æB––VÆG2&–âF†B6âæWfW"ÖF6‚Âv—F‚æð¢òòòF–væ÷7F–2g&öÒV—F†W"&6¶VæBâF†R&Òw27FGW2—2F†Rv÷'7BF†–æp¢òòòcFöW2VæFW"—BÂ6ò—B—26–ÆVçFÇ”Ö—4Æ÷vW'6à¢5·FW7EÐ¦fâå÷Væ¶æ÷våö&&UöæÖUö–åöö&–åö¶VW5ö—G5÷&V6—6UöF–væ÷7F–2‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&6÷e÷'VçF–ÖUö&÷VæE÷FW7Bæ†&2"“°¢VÖ—Eö7÷7&2‚ff—‡GW&R“° ¢ÆWBW'"ÒÆ÷vW%÷7&2‚ff—‡GW&Rç&WÆ6R‚&VãÒ³Ò"Â&VãÒ´äõWÒ"’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2“°¢76W'B†×6ræ6öçF–ç2‚&&–âVã"’Â&&–â—2æ÷Bö–çC¢¶×6wÒ"“°¢76W'B†×6ræ6öçF–ç2‚&äõV—2æ÷Bf–ÆR×66÷R6öç7B"’Â'¶×6wÒ"“° ¢òòv†BcFöW2v—F‚—BÂv†–6‚—2v†BÖ¶W2F†R7VvvW7F–öâw&öærà¢f÷"†æÖRÂæVVFÆR’–â²‚$äõR"Â%÷bÓÒäõR"’Â‚$Tôb"Â%÷bÓÒTôb"•Ò°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2€¢ff—‡GW&Rç&WÆ6R‚&VãÒ³Ò"Âff÷&ÖB‚&VãÒ··¶æÖW××Ò"’’À¢’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'¶æÖWÓ¢cVÖ—G3¢¶WÒ"’“°¢76W'B€¢cæ6öçF–ç2†æVVFÆR’À¢'¶æÖWÓ¢c7FW2F†RæÖR–çFòF†R6ö×&—6öâ ¢“°¢Ð ¢òòF†RW66R†F6†W2F†RÖW76vRæÖW2ÆÂv÷&²Â6òF†R&W7Böb—@¢òò—267W&FRà¢Æ÷vW%÷7&2‚ff÷&ÖB€¢&6öç7BäõRÒ5ÆåÆç·Ò"À¢f—‡GW&Rç&WÆ6R‚&VãÒ³Ò"Â&VãÒ´äõWÒ"¢’¢æW‡V7B‚&6öç7BæÖRföÆG2"“°§Ð ¢òòòâ–çFVvW"Æ—FW&Â&–â7V26ææ÷BföÆB7Æ—G2F†R6ÖRv’¢òòò6Æ–6R&÷VæBFöW2†F—fW&vVæ6Rƒr’ÂæBf÷"F†R6ÖRÖV7W&V@¢òòò&V6öç2(	B'WB—B—24U$DR&ÒÂ&V6†VB&Vf÷&RföÆEö6öç7F—0¢òòòWfW"6öç7VÇFVBÂ6ò—BF¶W2—G2÷vâ&ö&R&F†W"F†âF†R÷F†W ¢òòòöæRw2fW&F–7Bà¢òòð¢òòòÂ7V2ÂcVÖ—G2ÂfW&F–7BÀ¢òòòÂÒÒ×ÂÒÒ×ÂÒÒ×À¢òòòÂBvCÂföÆG2Fò÷bÓÒ(	B6÷'&V7BÂVç7W÷'FVF¢c&VÆÇ’—2v’÷WBÀ¢òòòÂ““““““““““““““““““““““–ÂfW&&F–Ó²r²²v&ç2æBG'Væ6FW2Â6–ÆVçFÇ”Ö—4Æ÷vW'6À¢5·FW7EÐ¦fâå÷VæföÆF&ÆUö&–åöÆ—FW&Å÷7Æ—G5ööå÷v†–6…ö¶–æEö—Eö—2‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&6÷e÷'VçF–ÖUö&÷VæE÷FW7Bæ†&2"“°¢ÆWB7V2ÒÆ#¢g7G'Âf—‡GW&Rç&WÆ6R‚&VãÒ³Ò"Âff÷&ÖB‚&VãÒ··¶'××Ò"’“° ¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚g7V2‚#BvC"’’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚'6—¦VBÆ—FW&Â"’Â'¶×6wÒ"“°¢òò(
fæBF†R6Æ–Ò&V†–æBF†B7VvvW7F–öã¢cföÆG2—BFòF†R6ÖP¢òò6ö×&—6öâF†RÆ–âÆ—FW&Â&öGV6W2à¢76W'EöW€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7V2‚#BvC"’’’æW‡V7B‚'cVÖ—G2"’À¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7V2‚#"’’’æW‡V7B‚'cVÖ—G2"’À¢'cföÆG2BvCW†7FÇ’Æ–¶RÂv†–6‚—2v†BÖ¶W2ÒÖ6öFVvVâc†öæW7B†W&R ¢“° ¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g7V2‚#““““““““““““““““““““““’"’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚'FöòÆ&vRf÷"—G2G—R"’Â'¶×6wÒ"“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7V2‚#““““““““““““““““““““““’"’’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢cæ6öçF–ç2‚%÷bÓÒ““““““““““““““““““““““’"’À¢'c7FW2F†R÷fW"×v–FRÆ—FW&Â–çFòF†R6ö×&—6öã¢·cÒ ¢“° ¢òò&÷F‚ÆæF–æw2(	BÖVÖ&W"æB&ævRVæB(	B6†&RöæP¢òò–×ÆVÖVçFF–öâÂ6ò&÷F‚&R6†V6¶VBà¢ÆWB&ævRÒf—‡GW&Rç&WÆ6R‚'6VÅöÆòÒ¶GWBæVâââuÒ"Â'6VÅöÆòÒ³BvCââuÒ"“°¢76W'B†76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚g&ævR’çVçw&öW'"‚’’æ6öçF–ç2‚'6—¦VBÆ—FW&Â"’“°§Ð ¢òòò&–â7V2—26öç7FçBU…$U54”ôâÂæ÷B§W7BÆ—FW&Â÷"æÖRà¢òòòcVÖ—G2³ÒÖ2–b‚‚…÷bÓÒÒ’’–Âv†–6‚ÖVç2÷bÓÒÀ¢òòò6òföÆF–ær—B—2W†7B(	BæB—BÖ¶W2F†R&–â6öç7F&F†W"F†à¢òòòW"×6×ÆRW‡&W76–öâÂÖF6†–ær†÷rF†RÆ—FW&Â7VÆÇ2÷WBà¢òòð¢òòòF†R†öö²×&Ò&V6VFVæ6R7W'f—fW2F†RföÆC¢†öö²&ÖWFW"&VG0¢òòòf–ÆR×66÷R6öç7FöbF†R6ÖRæÖRÂ6òF†RföÆB—26¶—VBv†Và¢òòòöæRV'2ç—v†W&R–âF†R&÷VæBà¢5·FW7EÐ¦fâö6öç7FçEöW‡&W76–öåö&–å÷7V5öföÆG2‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&6÷e÷'VçF–ÖUö&÷VæE÷FW7Bæ†&2"“°¢ÆWB&6RÒVÖ—Eö7÷7&2‚ff—‡GW&Rç&WÆ6R‚&VãÒ³Ò"Â&VãÒ³Ò"’“°¢f÷"‡v†BÂ&Vf—‚Â7V2’–â°¢‚&&—F†ÖWF–2"Â""Â#Ò"’À¢‚&6öç7B–ââW‡&W76–öâ"Â&6öç7B¢ÒÆåÆâ"Â%¢Ò"’À¢‚&6†–gB"Â""Â"ƒÃÂ2’Ò‚"’À¢Ò°¢ÆWB7&2Òf÷&ÖB€¢'·&Vf—‡×·Ò"À¢f—‡GW&Rç&WÆ6R‚&VãÒ³Ò"Âff÷&ÖB‚&VãÒ···7V7××Ò"’¢“°¢76W'EöW€¢VÖ—Eö7÷7&2‚g7&2’À¢&6RÀ¢'·v†GÒ×W7BÆ÷vW"W†7FÇ’Æ–¶RF†RÆ—FW&Â·³×Ö ¢“°¢Ð ¢òò&V6VFVæ6S¢†öö²&ÒöbF†R6ÖRæÖR7F–ÆÂv–ç2÷fW"¢òò6öç7BâF†Rf—'7BfW'6–öâöbF†—276W'FVBöâ6öç7BF–6·2Ò“– ¢òòv–ç7B6÷fW&w&÷Wö†ööµ÷&Õ÷FW7FÂv†÷6R†öö²&ÖWFW"—0¢òò6ÖF(	BF–6·6—2d”TÄBöbF†R&V6÷&B—B6'&–W2æBæWfW ¢òòV'22&&R–FVçF–f–W"–â&÷VæBâF†B76W'F–öâ76V@¢òòf÷"WfW'’×WFF–öâöbF†RwV&BÂ–æ6ÇVF–ærFVÆWF–ær—C¢—Bv0¢òò6ö×&–ærGvò&öw&×2æV—F†W"öbv†–6‚W†W&6—6VB&V6VFVæ6Rà¢òð¢òò&VÂöæRæVVG2F†R†öö²&ÖWFW"—G6VÆb–â$”âÂv†–6‚ÖVç0¢òòv—f–ærF†R†öö¶&ÆR66Æ"&ÖWFW"Fò6×ÆRà¢ÆWB†öö¶VBÒ7&FS£¦f—‡GW&R‚&6÷fW&w&÷Wö†ööµ÷&Õ÷FW7Bæ†&2"¢ç&WÆ6Vâ€¢"†öö¶&ÆR'Våöf÷"†6ÖC¢'Vä6ÖB’"À¢"†öö¶&ÆR'Våöf÷"†6ÖC¢'Vä6ÖBÂ³¢V–çCÃƒâ’"À¢À¢¢ç&WÆ6Vâ€¢&6÷fW&w&÷W'Vä6ÖD6÷b†G'bç'Våöf÷"†6ÖB’÷7B’"À¢&6÷fW&w&÷W'Vä6ÖD6÷b†G'bç'Våöf÷"†6ÖBÂ²’÷7B’"À¢À¢¢ç&WÆ6Vâ€¢"G'bç'Våöf÷"†6ÖB’"À¢"G'bç'Våöf÷"†6ÖBÂ2’"À¢À¢“°¢76W'B€¢†öö¶VBæ6öçF–ç2‚&6ÖC¢'Vä6ÖBÂ³¢V–çCÃƒâ"’bb†öö¶VBæ6öçF–ç2‚''Våöf÷"†6ÖBÂ2’"’À¢'F†R66Æ"†öö²&ÖWFW"×W7B7GVÆÇ’&RFFVB ¢“°¢f÷"‡7V2ÂvçB’–â²‚&²"Â%÷bÓÒ²’"’Â‚&²²"Â%÷bÓÒ†²²’’"•Ò°¢ÆWB&–âÒ†öö¶VBç&WÆ6Vâ‚&öæRÒ³Ò"Âff÷&ÖB‚&öæRÒ···7V7××Ò"’Â“°¢76W'EöæR†&–âÂ†öö¶VBÂ&·7V7Ö¢F†R&–â×W7B7GVÆÇ’6†ævR"“°¢ÆWBÆ–âÒVÖ—Eö7÷7&2‚f&–â“°¢76W'B€¢Æ–âæ6öçF–ç2‡vçB’À¢&·7V7Ö¢F†R&–â×W7B6ö×&Rv–ç7BF†R†öö²$uTÔTåB†·vçGÖ’ ¢“°¢òò(
fæBâVç&VÆFVB6öç7B¶×W7Bæ÷B6GW&R—BâF†—2—2F†P¢òò76W'F–öâF†B7GVÆÇ’&—FW3¢v—F‚F†RwV&B&VÖ÷fVBF†P¢òò&–âföÆG2Fò÷bÓÒ“–æBF†R÷WGWG27F÷ÖF6†–ærà¢76W'EöW€¢VÖ—Eö7÷7&2‚ff÷&ÖB‚&6öç7B²Ò“•ÆåÆç¶&–çÒ"’’À¢Æ–âÀ¢&·7V7Ö¢f–ÆR×66÷R6öç7B¶×W7Bæ÷BGW&âF†R†öö²&ÖWFW"–çFò“’ ¢“°¢Ð§Ð ¢òòò&V6÷&B×G—VB†öö²&ÖWFW"—2&V¦V7FVB–â&–âF†R6ÖRv’—@¢òòò—2–â6÷fW'ö–çBD$tUBÂæBf÷"F†R6ÖR&V6öã¢&–â6ö×&W0¢òòò÷fv–ç7BF†R&÷VæBÂ6òöæRÒ¶6ÖGÖVÖ—G2÷bÓÒ6ÖF(	@¢òòòV–çCcE÷Fv–ç7B7G'V7BÂv†–6‚r²²&V¦V7G2à¢òòð¢òòòcVÖ—G2W†7FÇ’F†R6ÖRF†–ærÂ6òF†—2—2ÖÆf÷&ÖVB&öw&Ð¢òòòVæFW"&÷F‚&6¶VæG2†–çfÆ–F’Âæ÷B7V'6WBvà¢òòð¢òòòF†RW†7B×fÇVRÆæF–ær&V6ÖR&V6†&ÆRv†Vâ&–âÖVÖ&W'2vW&P¢òòòÆÆ÷vVBFò&R'VçF–ÖRW‡&W76–öç3²F†R&ævRÖ&÷VæBÆæF–ærv0¢òòò&V6†&ÆR&Vf÷&RF†BæBVæwV&FVBâ&÷F‚&R6†V6¶VBà¢5·FW7EÐ¦fâ÷&V6÷&Eö†ööµ÷&Õö—5÷&V¦V7FVEö–åöö&–åöæ÷Eö§W7Eö÷F&vWB‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&6÷fW&w&÷Wö†ööµ÷&Õ÷FW7Bæ†&2"“° ¢òò6öçG&öÃ¢F†R&Vv—7FW&VBf—‡GW&RÆ÷vW'2(	BF†RwV&BFFVB†W&R—0¢òòæ÷B&V¦V7F–ær66Æ"†öö²×&Ò&–ç2à¢VÖ—Eö7÷7&2‚ff—‡GW&R“° ¢f÷"†g&öÒÂFò’–â°¢‚&öæRÒ³Ò"Â&öæRÒ¶6ÖGÒ"’À¢‚'—"Ò³"âã5Ò"Â'—"Ò¶6ÖBâã5Ò"’À¢Ò°¢ÆWBW'"ÒÆ÷vW%÷7&2‚ff—‡GW&Rç&WÆ6R†g&öÒÂFò’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fW'"“°¢76W'B€¢×6ræ6öçF–ç2‚&×W7B6ö×&Rv–ç7B66Æ""’À¢&·F÷Ö¢¶×6wÒ ¢“°¢76W'B†×6ræ6öçF–ç2‚'&V6÷&B'Vä6ÖF"’Â'¶×6wÒ"“°¢Ð ¢òòF†R66Æ"d”TÄBöbF†R6ÖR&Ò—27F–ÆÂf–æRÂ6òF†RwV&@¢òò&V¦V7G2F†R&V6÷&B&F†W"F†âF†R†öö²&Òà¢Æ÷vW%÷7&2‚ff—‡GW&Rç&WÆ6R‚&öæRÒ³Ò"Â&öæRÒ¶6ÖBçF–6·7Ò"’¢æW‡V7B‚&66Æ"†öö²×&Òf–VÆB—2fÆ–B&–âÖVÖ&W""“° ¢òò†öö²&Òt”å2÷fW"f–ÆR×66÷R6öç7BöbF†R6ÖRæÖRÂv†–6€¢òò—2F†R&V6VFVæ6R6÷fW'ö–çBF&vWBÇ&VG’W6W2âv—F†÷WB—BÀ¢òòFF–ærâVç&VÆFVB6öç7B6ÖBÒv÷VÆB6–ÆVçFÇ’GW&à¢òòöæRÒ¶6ÖGÖ–çFò÷bÓÒv†–ÆR6÷fW"6ÖBçF–6·6–âF†R6ÖP¢òò6÷fW&w&÷W7F–ÆÂÖVçBF†R†öö²&wVÖVçB(	BöæRæÖRÂGvð¢òòÖVæ–æw2ÂæòF–væ÷7F–2â†W&RF†R†öö²&Òv–ç2æBF†R66Æ ¢òòwV&B&÷fR6F6†W2—BÂv†–6‚—2F†Rv†öÆRö–çBà¢ÆWBW'"ÒÆ÷vW%÷7&2‚ff÷&ÖB€¢&6öç7B6ÖBÒÆåÆç·Ò"À¢f—‡GW&Rç&WÆ6R‚&öæRÒ³Ò"Â&öæRÒ¶6ÖGÒ"¢’¢çVçw&öW'"‚“°¢76W'B€¢76W'Eö–çfÆ–B‚fW'"’æ6öçF–ç2‚&×W7B6ö×&Rv–ç7B66Æ""’À¢&†öö²&Ò×W7B6†F÷r6ÖRÖæÖVB6öç7C¢¶W'#£÷Ò ¢“°§Ð ¢òòò&V¦V7FVB&–â7V2—2&W÷'FVB2&–æÂæ÷B2ö–çFà¢òòð¢òòòÆ÷vW%÷ö–çE÷F&vWF6W'fW26÷fW'ö–çBF&vWG2Â†öö²&×2Â&–à¢òòò&÷VæG2æB&–âfÇVW2'WBæÖW2öæÇ’F†Rf—'7BÂ6òFVÆVvF–ær&–à¢òòòÖVÖ&W'2Fò—BÖFR&V¦V7FVB²'‚'Ö&W÷'B'ö–çB"F†R6÷W&6P¢òòòæWfW"FV6Æ&VBà¢5·FW7EÐ¦fâ÷&V¦V7FVEö&–å÷7V5ö—5÷&W÷'FVEö5öö&–â‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&6÷e÷'VçF–ÖUö&÷VæE÷FW7Bæ†&2"“°¢VÖ—Eö7÷7&2‚ff—‡GW&R“° ¢òò&÷F‚ÔTÔ$U"æB&ævRTäBÂ6–æ6RF†W’6†&RöæR–×ÆVÖVçFF–öà¢òòæB&VÆ&VÂÆ–VBFòöæÇ’öæRöbF†VÒv÷VÆBvòVææ÷F–6VBà¢f÷"†g&öÒÂFò’–â°¢‚&VãÒ³Ò"Â"2&VãÒ²'‚'Ò"2’À¢‚'6VÅöÆòÒ¶GWBæVâââuÒ"Â"2'6VÅöÆòÒ²'‚"ââuÒ"2’À¢Ò°¢ÆWBW'"ÒÆ÷vW%÷7&2‚ff—‡GW&Rç&WÆ6R†g&öÒÂFò’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2“°¢76W'B†×6ræ6öçF–ç2‚&&–â"’Â&·F÷Ö¢¶×6wÒ"“°¢76W'B‚×6ræ6öçF–ç2‚'ö–çB"’Â&·F÷Ö¢¶×6wÒ"“°¢Ð ¢òòF†RG—òÖW76vR&V6†W2&ævRVæBFöòÂv—F‚F†R6ÖRfW&F–7Bà¢ÆWBW'"ÒÆ÷vW%÷7&2‚ff—‡GW&Rç&WÆ6R‚'6VÅöÆòÒ¶GWBæVâââuÒ"Â'6VÅöÆòÒ´äõRââuÒ"’¢çVçw&öW'"‚“°¢76W'B€¢76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2¢æ6öçF–ç2‚&äõV—2æ÷Bf–ÆR×66÷R6öç7B"¢“° ¢òò&V¦V7FVB6÷fW'ö–çBD$tUB7F–ÆÂ6—2'ö–çB"Â6òF†R&VÆ&VÂ—0¢òò66÷VBFòF†R&–âF‚&F†W"F†â&VæÖ–ærF†Ræ÷VâWfW'—v†W&Rà¢ÆWBW'"Ð¢Æ÷vW%÷7&2‚ff—‡GW&Rç&WÆ6R‚&7öVâ¢6÷fW"GWBæVâ"Â"2&7öVâ¢6÷fW"'‚""2’’çVçw&öW'"‚“°¢76W'B€¢76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2’æ6öçF–ç2‚'ö–çB7öVæ"¢“°§Ð ¢òòòF†R6÷fW'ö–çBö&–â7V'6WBvFRW6VBFò&RöæR&Ææ¶WBVç7W÷'FVF ¢òòò&Ò(	B'&R×'Vâv—F‚ÒÖ6öFVvVâc"f÷"WfW'—F†–ær—B&V¦V7FVBà¢òòò&ö&–ærWfW'’W‡$¶–æF—B6â&V6V—fRf÷VæBdõU"F–ffW&VçBc¢òòò&V†f–÷W'2ÂæBöæÇ’öæRöbF†VÒÖ¶W2câW66R†F6‚â—B—2æ÷@¢òòòF†R6öÖÖöâöæRà¢òòð¢òòòc†2æò7V'6WBvFR†W&RBÆÃ¢—B&VæFW'2F†RW‡&W76–öâv—F€¢òòòVÖ—EöW‡&æB67G2FòV–çCcE÷FÂ6òv†FWfW"F†RW6W"w&÷FRÆæG0¢òòò–âF†R6×ÆW"fW&&F–Òâv†Bf&–W2—2v†WF†W"F†BFW‡B6ö×–ÆW2À¢òòòæBv†WF†W"—BÖVç2ç—F†–ærà¢5·FW7EÐ¦fâF†Uö6÷fW'ö–çE÷7V'6WEövFUö—5ö6Æ76–f–VEö'•÷v†E÷cöFöW2‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&6÷e÷'VçF–ÖUö&÷VæE÷FW7Bæ†&2"“°¢6öç7BDuC¢g7G"Ò&7öVâ¢6÷fW"GWBæVâ#° ¢òò6öçG&öÃ¢F†R&Vv—7FW&VBf—‡GW&RÆ÷vW'2VæFW"&÷F‚&6¶VæG2Â6ð¢òòWfW'’×WFF–öâ&VÆ÷r&V6†W2F†R7V'6WBvFRà¢VÖ—Eö7÷7&2‚ff—‡GW&R“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚ff—‡GW&R’’æW‡V7B‚'cVÖ—G2F†R6öçG&öÂ"“°¢ÆWBF&vWBÒÇC¢g7G'Âf—‡GW&Rç&WÆ6R…DuBÂff÷&ÖB‚&7öVâ¢6÷fW"·GÒ"’“° ¢òò)H)Hc6ö×–ÆW2—BæB6×ÆW2F†Ru$ôärD„”är)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H ¢f÷"†W‡"ÂæVVFÆR’–â°¢‚%³âã%Ò"Â'6×Æ–ær&ævR"’À¢‚#ãR"Â'6×Æ–ærfÆöBÆ—FW&Â"’À¢‡"2"'‚""2Â'6×Æ–ær7G&–ærÆ—FW&Â"’À¢Ò°¢ÆWBW'"ÒÆ÷vW%÷7&2‚gF&vWB†W‡"’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2“°¢76W'B†×6ræ6öçF–ç2†æVVFÆR’Â&¶W‡'Ö¢¶×6wÒ"“°¢Ð¢òòF†RÆöBÖ&V&–æröæRÂv—F‚cw2÷vâv÷&G22F†RWf–FVæ6S¢—G0¢òòVÖ—GFW"ÆVfW26öÖÖVçBFÖ—GF–ær—BG&÷VBF†R&÷VæG2ÂF†Và¢òò6×ÆW2¦W&òöâWfW'’7–6ÆRà¢ÆWBc÷&ævRÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gF&vWB‚%³âã%Ò"’’’æW‡V7B‚'cVÖ—G2&ævRF&vWB"“°¢76W'B€¢c÷&ævRæ6öçF–ç2‚"‡V–çCcE÷B’‚ò¢&ævRâã"¢ò’"’À¢'cG&÷2F†R&ævRæB6×ÆW2¥Æç·c÷&ævWÒ ¢“°¢òò(
fæBF†R6öçG&öÂ&÷fW2&VÂF&vWBFöW2&V6‚F†B6ÖR6Æ÷BÀ¢òò6òF†R¦W&ò—2F†R&ævR&V–ærG&÷VB&F†W"F†âF†R6×ÆW ¢òò&V–ær–æW'Bà¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚ff—‡GW&R’¢æW‡V7B‚'cVÖ—G2"¢æ6öçF–ç2‚"‡V–çCcE÷B’††&5÷'C£¦†&5÷&VB†GWBÓæVâ’’"’À¢&&VÂF&vWBÆæG2–âF†R6ÖR6Æ÷B ¢“° ¢òò&ævRäU5DTB–â6WBæWfW"&V6†W2F†RvFR(	BÆ÷vW%ö&–å÷fÇVW6 ¢òò†2—G2÷vâ&ÒæBÆ÷vW'2—B6÷'&V7FÇ’FöF’â7vVW–ærF†RGvð¢òòFövWF†W"v÷VÆB†fR'&ö¶Vâv÷&¶–ær6öFRà¢VÖ—Eö7÷7&2‚ff—‡GW&Rç&WÆ6R‚&VãÒ³Ò"Â&VãÒµ³âã%×Ò"’“° ¢òò)H)HcVÖ—G26öÖWF†–ærF†BFöW2æ÷B6ö×–ÆR)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H ¢f÷"†W‡"ÂæVVFÆR’–â°¢‚$äõR"Â&æ÷BEUB÷'B÷"†öö²&ÖWFW""’À¢‚&föòæ&""Â&æ÷BEUB÷'B÷"†öö²&ÖWFW""’À¢‚&föõ³Ò"Â&æ÷BEUB÷'B÷"†öö²&ÖWFW""’À¢‚"æVâ"Â&æ÷BEUB÷'B÷"†öö²&ÖWFW""’À¢‚'VæFVf–æVEöfâƒ’"Â'6×Æ–ærâVç7W÷'FVB6ÆÂ"’À¢‚&GWBæVâææ÷R‚’"Â'6×Æ–ærâVç7W÷'FVB6ÆÂ"’À¢Ò°¢ÆWBW'"ÒÆ÷vW%÷7&2‚gF&vWB†W‡"’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¤VÖ—G5Væ6ö×–Æ&ÆR“°¢76W'B†×6ræ6öçF–ç2†æVVFÆR’Â&¶W‡'Ö¢¶×6wÒ"“°¢òòc66WG2V6‚öæRæB&–çG2—BfW&&F–Ò–çFòF†R6×ÆW"à¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gF&vWB†W‡"’’¢æW‡V7B‚'cVÖ—G2"¢æ6öçF–ç2‚'V–çCcE÷B÷bÒ‡V–çCcE÷B’‚"’À¢&¶W‡'Ö&V6†W2cw26×ÆW"67B ¢“°¢Ð ¢òò)H)Hc&VgW6W2Föò)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H)H ¢f÷"†W‡"ÂæVVFÆR’–â°¢‚'³Â'Ò"Â'6WBÆ—FW&Â÷"&æFöÖ—¦V"’À¢‚'&æFöÖ—¦R†GWBæVâ’"Â'6WBÆ—FW&Â÷"&æFöÖ—¦V"’À¢‚&f÷&²GWBæVâ‚’"Â'FV×÷&Â÷"f÷&²W‡&W76–öâ"’À¢‚"23GWBæVâ"Â'FV×÷&Â÷"f÷&²W‡&W76–öâ"’À¢‚"F6Æös"†GWBæVâ’"Â'7—7FVÒgVæ7F–öâ"’À¢Ò°¢ÆWBW'"ÒÆ÷vW%÷7&2‚gF&vWB†W‡"’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥&V¦V7G2“°¢76W'B†×6ræ6öçF–ç2†æVVFÆR’Â&¶W‡'Ö¢¶×6wÒ"“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gF&vWB†W‡"’’’æ—5öW'"‚’À¢'c×W7B&V¦V7B¶W‡'ÖFöòÂ÷"F†—2—2&VÂv ¢“°¢Ð§Ð ¢òòòF6Æös"‡‚–—2F†R66RF†BæV&Ç’vVçB–âÖ—2Ö6Æ76–f–VBÂæBF†P¢òòò&V6öâ—2v÷'F‚FW7Böb—G2÷vã¢—Bv2f—'7B&ö&VB0¢òòò6Æös"†GWBæVâ–Ât•D„õUBF†RFà¢òòð¢òòòF†B7VÆÆ–ær—2æ÷B7—7FVÒ6ÆÂBÆÂ(	B—B'6W22Æ–â6ÆÀ¢òòòFòâVæFVf–æVBgVæ7F–öâÂv†–6‚cVÖ—G2fW&&F–ÒæBr²²&V¦V7G2â—@¢òòòÆöö¶VBÆ–¶RVÖ—G5Væ6ö×–Æ&ÆV–âÆVv—F–ÖFRÆæwVvR7W&f6RâF†P¢òòò&VÂ7VÆÆ–ær—26öç7G'V7Bc&VgW6W2÷WG&–v‡BÂæBF†RGvòÆæB–à¢òòòF–ffW&VçB&×2à¢5·FW7EÐ¦fâF†U÷6–v–Åö—5÷v†EöÖ¶W5ö÷7—7FVÕö6ÆÅö÷7—7FVÕö6ÆÂ‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&6÷e÷'VçF–ÖUö&÷VæE÷FW7Bæ†&2"“°¢ÆWBF&vWBÒÇC¢g7G'Âf—‡GW&Rç&WÆ6R‚&7öVâ¢6÷fW"GWBæVâ"Âff÷&ÖB‚&7öVâ¢6÷fW"·GÒ"’“° ¢òòv—F‚F†R6–v–Ã¢7—7FVÒ6ÆÂÂæBc&V¦V7G2—Bà¢ÆWBW'"ÒÆ÷vW%÷7&2‚gF&vWB‚"F6Æös"†GWBæVâ’"’’çVçw&öW'"‚“°¢76W'B†76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥&V¦V7G2’æ6öçF–ç2‚'7—7FVÒgVæ7F–öâ"’“°¢76W'B†7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gF&vWB‚"F6Æös"†GWBæVâ’"’’’æ—5öW'"‚’“° ¢òòv—F†÷WB—C¢â÷&F–æ'’6ÆÂFòæÖRæ÷F†–ærFV6Æ&W2Âv†–6‚c¢òò66WG2æBVÖ—G2–çFò2²²F†B6ææ÷B6ö×–ÆRà¢ÆWBW'"ÒÆ÷vW%÷7&2‚gF&vWB‚&6Æös"†GWBæVâ’"’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¤VÖ—G5Væ6ö×–Æ&ÆR“°¢76W'B€¢×6ræ6öçF–ç2‚&æ÷BFV6Æ&VB†VÇW"÷"W‡FW&âgVæ7F–öâ"’À¢'¶×6wÒ ¢“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gF&vWB‚&6Æös"†GWBæVâ’"’’¢æW‡V7B‚'cVÖ—G2âVæFVf–æVB6ÆÂ"¢æ6öçF–ç2‚"‡V–çCcE÷B’†6Æös"††&5÷'C£¦†&5÷&VB†GWBÓæVâ’’’"’À¢'c&–çG2F†RVæFVf–æVB6ÆÂfW&&F–Ò ¢“°§Ð ¢òòòf–ÆR×66÷R6öç7F6×ÆVBF—&V7FÇ’'’6÷fW'ö–çBâFVvVæW&FR(	@¢òòòF†Rö–çB&VG2F†R6ÖRfÇVRf÷&WfW"(	B'WBÆVvÂÂæBc7W÷'G0¢òòò—C¢—BVÖ—G2—G2÷vâ7FF–26öç7FW‡"V–çCcE÷B²Òs¶æB6×ÆW0¢òòò‡V–çCcE÷B’„²–Âv†–6‚6ö×–ÆW2æB—26÷'&V7Bà¢òòð¢òòò6òF†—2—2v4Äõ4TBÂæ÷B6Æ76–f–VBâ—Bv2æV&Ç’F†R÷÷6—FS ¢òòòF†R7V'6WBÖvFR7Æ—B&÷fRv2&ö&VBv—F‚Tä´äõtâ–FVçG2æBv÷VÆ@¢òòò†fR7vWB¶æ÷vâ6öç7B–âv—F‚F†VÒÂFVÆÆ–ærW6W'2cVÖ—G0¢òòòVæ6ö×–Æ&ÆR2²²f÷"6öÖWF†–ærcvWG2&–v‡Bà¢5·FW7EÐ¦fâö6öç7Eö6÷fW'ö–çE÷F&vWEöföÆG2‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&6÷e÷'VçF–ÖUö&÷VæE÷FW7Bæ†&2"“°¢6öç7BDuC¢g7G"Ò&7öVâ¢6÷fW"GWBæVâ#°¢ÆWBv—F‚ÒÇ&S¢g7G"ÂC¢g7G'Â°¢f÷&ÖB€¢'·&W×·Ò"À¢f—‡GW&Rç&WÆ6R…DuBÂff÷&ÖB‚&7öVâ¢6÷fW"·GÒ"’¢¢Ó° ¢òò6öçG&öÃ¢F†R&Vv—7FW&VBf—‡GW&RÆ÷vW'2à¢VÖ—Eö7÷7&2‚ff—‡GW&R“° ¢òòF†RföÆBv—fW2'—FRÖ–FVçF–6Â÷WGWBFòF†RÆ—FW&Â—B6ö×WFW>(
`¢ÆWBÆ—FW&ÂÒVÖ—Eö7÷7&2‚gv—F‚‚""Â#r"’“°¢ÆWBföÆFVBÒVÖ—Eö7÷7&2‚gv—F‚‚&6öç7B²ÒuÆåÆâ"Â$²"’“°¢76W'EöW€¢föÆFVBÂÆ—FW&ÂÀ¢&6÷fW"¶v—F‚6öç7B²Òv×W7BÆ÷vW"W†7FÇ’Æ–¶R6÷fW"v ¢“°¢òò(
fæBF†RdÅTR—2W6VBÂ6òF†RWVÆ—G’—2æ÷BF†RF&vWB&V–æp¢òòG&÷VBà¢76W'EöæR€¢VÖ—Eö7÷7&2‚gv—F‚‚&6öç7B²Ò•ÆåÆâ"Â$²"’’À¢föÆFVBÀ¢&F–ffW&VçB6öç7B×W7B6†ævRF†R6×ÆW" ¢“° ¢òòcw26–FS¢—BFV6Æ&W2F†R6öç7FçBæB6×ÆW2—BÂv†–6‚—2v‡¢òòF†—2—2vFò6Æ÷6R&F†W"F†âF—fW&vVæ6RFòFö7VÖVçBà¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gv—F‚‚&6öç7B²ÒuÆåÆâ"Â$²"’’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢cæ6öçF–ç2‚"‡V–çCcE÷B’„²’"’À¢'c6×ÆW2F†R6öç7FçC¥Æç·cÒ ¢“°§Ð ¢òòòF†R6Æ76–f–W"—2†æFVBF†RæöFRF†RvÆ²d”ÄTBöâÂæ÷BF†P¢òòòF÷ÖÆWfVÂF&vWBà¢òòð¢òòòÆ÷vW%÷ö–çE÷F&vWFFW66VæG2F‡&÷Vv‚&VçF†W6W2&Vf÷&Rf–Æ–ærÂ6ð¢òòò6Æ76–f–W"6Æ÷6VB÷fW"F†R÷WFW"W‡&W76–öâv÷VÆB6VRF†P¢òòò&VçF†W6—2æB&V6‚—G26F6‚ÖÆÂ(	BÆ÷6–ærF†R&ævR66RÂv†–6‚—0¢òòòF†Rv†öÆR&V6öâF†R6Æ76–f–W"W†—7G2à¢5·FW7EÐ¦fâ&VçF†W6W5öFõöæ÷Eö†–FU÷F†Uöf–Æ–æuöæöFUög&öÕ÷F†Uö6Æ76–f–W"‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&6÷e÷'VçF–ÖUö&÷VæE÷FW7Bæ†&2"“°¢ÆWBF&vWBÒÇC¢g7G'Âf—‡GW&Rç&WÆ6R‚&7öVâ¢6÷fW"GWBæVâ"Âff÷&ÖB‚&7öVâ¢6÷fW"·GÒ"’“° ¢òò&&RæB&VçF†W6—6VB×W7B6Æ76–g’F†R6ÖRv’Â&Òf÷"&Òà¢f÷"†&&RÂw&VBÂ7FGW2ÂæVVFÆR’–â°¢€¢%³âã%Ò"À¢"…³âã%Ò’"À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢'6×Æ–ær&ævR"À¢’À¢€¢$äõR"À¢"„äõR’"À¢Æ÷vW#£¥c7FGW3£¤VÖ—G5Væ6ö×–Æ&ÆRÀ¢&æ÷BEUB÷'B÷"†öö²&ÖWFW""À¢’À¢Ò°¢f÷"7VÆÆ–ær–â¶&&RÂw&VEÒ°¢ÆWBW'"ÒÆ÷vW%÷7&2‚gF&vWB‡7VÆÆ–ær’’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"Â7FGW2“°¢76W'B†×6ræ6öçF–ç2†æVVFÆR’Â&·7VÆÆ–æwÖ¢¶×6wÒ"“°¢Ð¢Ð ¢òòcw2&VçF†W6—6VB÷WGWB6öæf—&×2F†R6Æ76–f–6F–öâG&fVÇ2v—F€¢òòF†R–ææW"æöFS¢—B7F–ÆÂG&÷2F†R&ævRæB6×ÆW2à¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gF&vWB‚"…³âã%Ò’"’’¢æW‡V7B‚'cVÖ—G2"¢æ6öçF–ç2‚"ò¢&ævRâã"¢ò"’À¢'cG&÷2&VçF†W6—6VB&ævRFöò ¢“°§Ð ¢òòòfW&–Æör×6—¦VBÆ—FW&Â–â6÷fW'ö–çBF&vWB¶VW2ö–çF–ærBcÀ¢òòòv†–6‚Æ÷vW'2&&RöæR6÷'&V7FÇ’(	BF†R6ÖR7Æ—BF†RFG&Öæ@¢òòò&Vv&Æö6²FG&W72föÆG26''’à¢5·FW7EÐ¦fâ÷6—¦VEöÆ—FW&Åö6÷fW'ö–çE÷F&vWE÷7F–ÆÅ÷ö–çG5öE÷c‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&6÷e÷'VçF–ÖUö&÷VæE÷FW7Bæ†&2"“°¢ÆWB7&2Òf—‡GW&Rç&WÆ6R‚&7öVâ¢6÷fW"GWBæVâ"Â&7öVâ¢6÷fW"3"vƒr"“° ¢ÆWBW'"ÒÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fW'"“°¢76W'B†×6ræ6öçF–ç2‚'6×Æ–ærF†R6—¦VBÆ—FW&Â"’Â'¶×6wÒ"“° ¢òòcw2Wf–FVæ6S¢—BÆ÷vW'2F†R6—¦VBÆ—FW&ÂFòF†R&–v‡BfÇVRà¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’¢æW‡V7B‚'cVÖ—G26—¦VBÆ—FW&Â"¢æ6öçF–ç2‚"‡V–çCcE÷B’ƒƒr’"’À¢'cÆ÷vW'2&&R6—¦VBÆ—FW&Â6÷'&V7FÇ’Âv†–6‚—2v‡’F†—2ö–çG2B—B ¢“° ¢òòâõdU"Õt”DRÆ—FW&Â6†&W2F†R6ÖRW‡$¶–æC£¤–çF&ÒæBvWG0¢òòF†R÷÷6—FR6Æ76–f–6F–öã¢cVÖ—G2ö†&5÷S#†6ö×÷6—FP¢òòF†Bæ'&÷w2Â6òF†R6÷fW'ö–çBv÷VÆB6×ÆRG'Væ6FVBfÇVRà¢òòö–çF–ærF†W&Rv÷VÆB&RF†RÖ—6F—&V7F–öâF†—27vVWW†—7G2Fð¢òò&VÖ÷fRà¢ÆWBv–FRÒf—‡GW&Rç&WÆ6R‚&7öVâ¢6÷fW"GWBæVâ"Â&7öVâ¢6÷fW"ƒ"“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚gv–FR’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚&÷fW"×v–FR–çFVvW"Æ—FW&Â"’Â'¶×6wÒ"“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gv–FR’¢æW‡V7B‚'cVÖ—G2â÷fW"×v–FRÆ—FW&Â"¢æ6öçF–ç2‚%ö†&5÷S#‚"’À¢'cVÖ—G2F†Ræ'&÷v–ær6ö×÷6—FR ¢“°§Ð ¢òòòG6W2ç'6†2Gvò&V¦V7F–öâ6—FW2f÷W"Æ–æW2'BF†BÆöö°¢òòò–çFW&6†ævV&ÆRæB6Æ76–g’õõ4•DTÅ’à¢òòð¢òòòF†RF–ffW&Væ6R—2v†WF†W"F†RVÆVÖVçB×G—Rææ÷FF–öâ—2%4TåB÷ ¢òòò$U4TåBÖ'WB×Vç&W6öÇf&ÆRââ'6VçBöæRÖ¶W2c7V'7F—GWFRv÷&¶–æp¢òòòFVfVÇB(	B6òD"Ô•"æ÷r7V'7F—GWFW2F†R6ÖRöæRæBF†Rv—0¢òòò4Äõ4TBâ&BöæRÖ¶W2c&–çBF†RæÖRfW&&F–Ò–çFòG—P¢òòò÷6—F–öâÂv†–6‚FöW2æ÷B6ö×–ÆRâöæR6öFRF‚†æFÆVB&÷F‚Âv†–6€¢òòò—2W†7FÇ’†÷rF†W’6ÖRFò6†&R6Æ76–f–6F–öâà¢5·FW7EÐ¦fâåö'6VçE÷G6WöVÆVÖVçE÷G—Uö—5öæ÷Eöö&EööæR‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚'G6W÷66Æ%÷FW7Bæ†&2"“°¢6öç7BDT4Ã¢g7G"Ò'G6W7V&W2†ã¢–çB’ÓâE6WÇV–çCÃcãâ#°¢76W'B†f—‡GW&Ræ6öçF–ç2„DT4Â’Â&f—‡GW&R6†R6†ævVB"“° ¢òò6öçG&öÃ¢F†R&Vv—7FW&VBf—‡GW&RÆ÷vW'2VæFW"&÷F‚&6¶VæG2Âæ@¢òòcw2ÆÖ&F&WGW&ç2F†RVÆVÖVçBG—R—Bv2v—fVâà¢VÖ—Eö7÷7&2‚ff—‡GW&R“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚ff—‡GW&R’¢æW‡V7B‚'cVÖ—G2F†R6öçG&öÂ"¢æ6öçF–ç2‚"Óâ7FC£§fV7F÷#ÇV–çCcE÷Câ"’À¢'F†R6öçG&öÂw2VÆVÖVçBG—R&V6†W2cw2&WGW&âG—R ¢“° ¢òò%4TåC¢v4Äõ4TBÂæ÷B6Æ76–f–VBâcFVfVÇG2F†RVÆVÖVçBG—P¢òòFò6–væVBcBÖ&—B66Æ"æBF†R6WVVæ6R'Vç3²D"Ô•"æ÷rFöW2F†P¢òò6ÖRÂ'—FRÖ–FVçF–6ÆÇ’à¢ÆWB'6VçBÒf—‡GW&Rç&WÆ6R„DT4ÂÂ'G6W7V&W2†ã¢–çB’"“°¢ÆWBF&—"ÒVÖ—Eö7÷7&2‚f'6VçB“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f'6VçB’’æW‡V7B‚'cVÖ—G2v—F†÷WB&WGW&âG—R"“°¢f÷"÷WB–â²gF&—"ÂgcÒ°¢76W'B€¢÷WBæ6öçF–ç2‚"Óâ7FC£§fV7F÷#Æ–çCcE÷Câ"’À¢&&÷F‚&6¶VæG2FVfVÇBF†RVÆVÖVçBG—S¥Æç¶÷WGÒ ¢“°¢Ð¢òò(
fæBF†RFVfVÇB—2DTdTÅBÂæ÷B6ö–æ6–FVæ6S¢F†Rææ÷FFV@¢òò6öçG&öÂ7F–ÆÂvWG2F†RVÆVÖVçBG—R—BFV6Æ&VBà¢76W'B€¢VÖ—Eö7÷7&2‚ff—‡GW&R’æ6öçF–ç2‚"Óâ7FC£§fV7F÷#ÇV–çCcE÷Câ"’À¢'F†Rææ÷FFVB6öçG&öÂ¶VW2—G2÷vâVÆVÖVçBG—R ¢“° ¢òò$U4TåBäB$C¢c&–çG2F†RæÖR–çFòF†R&WGW&âG—RÂæÖ–ær¢òòG—Ræ÷F†–ærFV6Æ&W2à¢ÆWB&BÒf—‡GW&Rç&WÆ6R„DT4ÂÂ'G6W7V&W2†ã¢–çB’ÓâE6WÄæõ7V6…G—Sâ"“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚f&B’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¤VÖ—G5Væ6ö×–Æ&ÆRÀ¢“°¢76W'B†×6ræ6öçF–ç2‚&æõ7V6…G—V"’Â'¶×6wÒ"“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f&B’¢æW‡V7B‚'cVÖ—G2&BVÆVÖVçBG—R"¢æ6öçF–ç2‚"Óâ7FC£§fV7F÷#Äæõ7V6…G—Sâ"’À¢'cVÖ—G2F†RVæFV6Æ&VBG—RfW&&F–Ò ¢“°§Ð ¢òòò&Vv—7FW"v–GF‚÷WG6–FRâãÓcBÂ÷"¦W&ò×v–GF‚f–VÆBÂ—2&öw&Ð¢òòòW'&÷"VæFW"WfW'’&6¶VæB(	Bæ÷B7V'6WBvÂ6òæòÒÖ6öFVvVâcà¢òòð¢òòòcFöW2æ÷B6†V6²V—F†W"öæRâ&Bv–GF‚fÆÇ2F†RÖ—'&÷"&6²Fð¢òòòV–çCcE÷FæB&V6÷&G2F†RFV6Æ&VBv–GF‚–ââFG&W72F&ÆRæ÷F†–æp¢òòò&VG3²¦W&ò×v–GF‚f–VÆBvWG2Ö6²ƒVÂ6ò—B&VG2f÷&WfW"æ@¢òòòWfW'’w&—FRFò—B—2æòÖ÷â&÷F‚6ö×–ÆRÂv†–6‚—2F†R&ö&ÆVÒà¢5·FW7EÐ¦fâ÷&Vv&Æö6µ÷v–GF…ö÷WG6–FU÷F†U÷fÇVUöÖöFVÅö—5ö÷&öw&ÕöW'&÷"‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚'&Vv&Æö6µ÷7V'6WE÷FW7Bæ†&2"“°¢6öç7BDT4Ã¢g7G"Ò'&Vv&Æö6²FÖ&Vw2f–&Vt†VÇW"v–GF‚3"#° ¢òò6öçG&öÃ¢F†R&Vv—7FW&VBf—‡GW&RÆ÷vW'2ÂæBc6—¦W2F†RÖ—'&÷ ¢òòg&öÒF†RFV6Æ&VBv–GF‚à¢VÖ—Eö7÷7&2‚ff—‡GW&R“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚ff—‡GW&R’¢æW‡V7B‚'cVÖ—G2F†R6öçG&öÂ"¢æ6öçF–ç2‚'V–çC3%÷B5E$ÂÒs²"’À¢'F†R6öçG&öÂw2v–GF‚&V6†W2F†RÖ—'&÷"G—R ¢“° ¢f÷"v–GF‚–â³ÂcUÒ°¢ÆWB7&2Òf—‡GW&Rç&WÆ6R€¢DT4ÂÀ¢ff÷&ÖB‚'&Vv&Æö6²FÖ&Vw2f–&Vt†VÇW"v–GF‚·v–GF‡Ò"’À¢“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&×W7B&RâãÓcB"’Â'v–GF‚·v–GF‡Ó¢¶×6wÒ"“°¢òòF†Rv–GF‚6ÖRg&öÒF†R&Vv&Æö6²DTdTÅB†W&RÂ6òF†RÖW76vP¢òò×W7Bæ÷B&ÆÖR&Vv—7FW"F†BFV6Æ&W2æòv–GF‚öb—G2÷vâà¢76W'B†×6ræ6öçF–ç2‚&FVfVÇBv–GF‚"’Â'v–GF‚·v–GF‡Ó¢¶×6wÒ"“°¢òòcw2Wf–FVæ6S¢—B66WG2ÂæB6–ÆVçFÇ’v–FVç2F†R6VÆÂà¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’¢æW‡V7B‚'c66WG2&Bv–GF‚"¢æ6öçF–ç2‚'V–çCcE÷B5E$ÂÒs²"’À¢'cfÆÇ2&6²FòcBÖ&—B6VÆÂf÷"v–GF‚·v–GF‡Ò ¢“°¢Ð ¢òòF†R¦W&ò×v–GF‚d”TÄB—26W&FR6†V6²Â&V6†&ÆRöæÇ’7BF†P¢òò&Vv—7FW"×v–GF‚öæR(	B×WFF–ærF†R&Vv&Æö6²FVfVÇB&WGW&ç2&Vf÷&P¢òò—BWfW"'Vç2â&Vv&Æö6µöf–VÆG5÷FW7Fv÷VÆB&RF†RæGW&Âf—‡GW&P¢òò'WB&W6öÇfW2—G2'W2F‡&÷Vv‚W6R'W4†”Æ—FVÂv†–6‚F†—2†&æW70¢òòFöW2æ÷B6V&6‚f÷"Â6òF†Rf–VÆB—2w&÷vâöâF†R6VÆbÖ6öçF–æV@¢òòöæR–ç7FVBà¢6öç7B$Ts¢g7G"Ò"&Vv—7FW"5E$Âƒ&W6WBr66W72'r#°¢ÆWBv—F…öf–VÆBÒÇG“¢g7G'Â°¢f—‡GW&Rç&WÆ6R€¢$TrÀ¢ff÷&ÖB€¢'µ$TwÕÆâf–VÆBÔôDR¢·G—ÒB&W6WB66W72'uÆâVæB&Vv—7FW"5E$Â ¢’À¢¢Ó° ¢òò6öçG&öÃ¢&VÂf–VÆBv–GF‚Æ÷vW'2VæFW"&÷F‚&6¶VæG2à¢VÖ—Eö7÷7&2‚gv—F…öf–VÆB‚'V–çCÃ3â"’“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gv—F…öf–VÆB‚'V–çCÃ3â"’’’æW‡V7B‚'cVÖ—G2F†R6öçG&öÂ"“° ¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚gv—F…öf–VÆB‚'V–çCÃâ"’’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&f–VÆBÔôDV†2¦W&òv–GF‚"’Â'¶×6wÒ"“°¢òòcw2Wf–FVæ6S¢—BFöW2æ÷B6†V6²V—F†W"ÂæB66WG2F†R6ÖP¢òò6÷W&6Râ„Bâ44U526—FR—BvöW2gW'F†W"æBVÖ—G2F†Rf–VÆBw0¢òòÖ6²2ƒVÂ6òF†Rf–VÆB&VG2f÷&WfW"æBWfW'’w&—FR—2¢òòæòÖ÷(	B'WBf–VÆBv—F‚æò66W726—FRVÖ—G2æ÷F†–ærBÆÂÂ6ð¢òòF†B—2æ÷Bv†BF†—2f—‡GW&R6â6†÷râ¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gv—F…öf–VÆB‚'V–çCÃâ"’’’æW‡V7B‚'c66WG2¦W&ò×v–GF‚f–VÆB"“°§Ð ¢òòòF†R&Vv&Æö6²æBFG&Ö44U526F6‚ÖÆÇ2âc†2æòvFRBV—F†W# ¢òòò—B&–çG2F†R66W72F‚7G&–v‡B–çFòF†R2²²Â6òâVæ¶æ÷và¢òòò&Vv—7FW"&V6öÖW2æöâÖÖVÖ&W"æBÖWF†öB6ÆÂ&V6öÖW2âVæFV6Æ&V@¢òòògVæ7F–öââæV—F†W"6ö×–ÆW2à¢5·FW7EÐ¦fâ÷WEööe÷7V'6WE÷&Vv&Æö6µöæEöFG&Öö66W75öFöW5öæ÷E÷ö–çEöE÷c‚’°¢òò&÷F‚f—‡GW&W2&W6öÇfRF†V—"'W2F‡&÷Vv‚W6R'W4†”Æ—FVÂv†–6€¢òòF†RVæ—B×FW7B†&æW72FöW2æ÷B6V&6‚f÷"(	B6òÆ÷vW"F†R66W70¢òòF‡2v–ç7BÆö6ÆÇ’ÖFV6Æ&VB&Vv&Æö6²–ç7FVBÂæB¶VWF†P¢òòcÖ&V†f–÷W"æ6†÷'2öâv†B—BVÖ—G2f÷"F†RF‚—G6VÆbà¢ÆWBf—‡GW&RÒf—‡GW&R‚'&Vv&Æö6µ÷7V'6WE÷FW7Bæ†&2"“°¢VÖ—Eö7÷7&2‚ff—‡GW&R“° ¢òòâVæ¶æ÷vâ$Tt•5DU"—2v†B&V6†W2F†R6F6‚ÖÆÂâÖWF†öB6ÆÀ¢òò†&Vw2ç&W6WEöÆÂ‚–’FöW2äõC¢vVæW&–27FFVÖVçBÆ÷vW&–æp¢òò–çFW&6WG2—Bf—'7BÂ6ò—B—27F–ÆÂVç7W÷'FVFæB&VÆöæw2Fð¢òòv†FWfW"7vVW6÷fW'27F×G2ç'6â76W'FVB†W&R6òF†R&÷VæF'’—0¢òò&V6÷&FVB&F†W"F†â&VF—66÷fW&VBà¢ÆWB7&2Òf—‡GW&Rç&WÆ6R‚'&Vw2å5$2Ò3SC“ƒ“b"Â'&Vw2ääõRÒ3SC“ƒ“b"“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¤VÖ—G5Væ6ö×–Æ&ÆRÀ¢“°¢76W'B†×6ræ6öçF–ç2‚&æ÷BFV6Æ&VB&Vv—7FW""’Â'¶×6wÒ"“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’¢æW‡V7B‚'c66WG2F†R&B66W72"¢æ6öçF–ç2‚$äõR"’À¢'cVÖ—G2F†RF‚fW&&F–Ò ¢“° ¢ÆWBÖWF†öBÒf—‡GW&Rç&WÆ6R‚'&Vw2å5$2Ò3SC“ƒ“b"Â'&Vw2ç&W6WEöÆÂ‚’"“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚fÖWF†öB’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚&ÖWF†öB6ÆÂç&W6WEöÆÂ‚âââ–"’À¢&ÖWF†öB6ÆÂ—2–çFW&6WFVB&Vf÷&RF†R&Vv&Æö6²6F6‚ÖÆÃ¢¶×6wÒ ¢“° ¢òòF†RFG&ÖGv–âÂöâF†R6VÆbÖ6öçF–æVBDE$ÔõD&‡F†P¢òò&Vv—7FW&VBFG&Öf—‡GW&W2&W6öÇfRF†V—"'W2F‡&÷Vv€¢òòW6R'W4†”Æ—FVÂv†–6‚F†—2†&æW72FöW2æ÷B6V&6‚f÷"’à¢ÆWBÖÒFG&Ö÷7&2‚$ƒ6—¦Rƒ3"“°¢VÖ—Eö7÷7&2‚fÖ“°¢ÆWB&BÒÖç&WÆ6R‚&6†—æÖÓ'2å4"Â&6†—ææ÷Rå4"“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚f&B’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¤VÖ—G5Væ6ö×–Æ&ÆRÀ¢“°¢76W'B†×6ræ6öçF–ç2‚&æò7V6‚–ç7Fæ6R÷&Vv—7FW"öf–VÆB"’Â'¶×6wÒ"“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f&B’¢æW‡V7B‚'c66WG2F†R&BFG&Ö66W72"¢æ6öçF–ç2‚&æ÷Rå4"’À¢'cVÖ—G2F†RFG&ÖF‚fW&&F–Ò ¢“°§Ð ¢òòòGvò6öçG&öÂç'66—FW2Â6Æ76–f–VB÷÷6—FRv—2à¢òòð¢òòòf÷"‚–âÇ66Æ#æÖ¶W2cVÖ—B2²²&ævRÖf÷"÷fW"fÇVRv—F€¢òòòæò—FW&F÷"(	B6ö×–ÆRW'&÷"âæöâÖÆ—FW&ÂF–ÖV÷WBÖW76vRÖ¶W2c¢òòòD•44$BF†RÖW76vRæB7V'7F—GWFR—G2÷vâÂv†–6‚6ö×–ÆW2æB'Vç3 ¢òòòF†Rf–ÇW&R7F–ÆÂf—&W2Â'WBF†RF–væ÷7F–2F†RW6W"w&÷FR—2vöæP¢òòòæBæ÷F†–ær6—26òà¢5·FW7EÐ¦fâF†Uö6öçG&öÅöfÆ÷u÷6—FW5÷7Æ—Eö&WGvVVå÷Væ6ö×–Æ&ÆUöæE÷6–ÆVçB‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&6÷e÷'VçF–ÖUö&÷VæE÷FW7Bæ†&2"“° ¢òò6öçG&öÃ¢F†R&Vv—7FW&VBf—‡GW&RÆ÷vW'2VæFW"&÷F‚&6¶VæG2Âæ@¢òòcVÖ—G2F†RF–ÖV÷WBÖW76vR—Bv2v—fVâà¢VÖ—Eö7÷7&2‚ff—‡GW&R“°¢ÆWB7FÂÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚ff—‡GW&R’’æW‡V7B‚'cVÖ—G2F†R6öçG&öÂ"“°¢76W'B€¢7FÂæ6öçF–ç2‡"2"&6÷VçBæWfW"&V6†VBR""2’À¢'F†R6öçG&öÂw2ÖW76vR&V6†W2cw2÷WGWB ¢“° ¢òòæöâÖÆ—FW&ÂF–ÖV÷WBÖW76vS¢c&WÆ6W2—Bv—F‚vVæW&–2Æ–æRà¢ÆWB×6u÷7&2Òf—‡GW&Rç&WÆ6R‡"2&f–Â‚&6÷VçBæWfW"&V6†VBR"’"2Â&f–Â†GWBæ6÷VçEö÷WB’"“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚f×6u÷7&2’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚&æWfW"V'2"’Â'¶×6wÒ"“°¢ÆWBcö×6rÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f×6u÷7&2’’æW‡V7B‚'c66WG2—B"“°¢76W'B€¢cö×6ræ6öçF–ç2‚'v—BVçF–ÂF–ÖVB÷WBgFW""’À¢'c7V'7F—GWFW2—G2÷vâÖW76vS¥Æç·cö×6wÒ ¢“°¢76W'B€¢cö×6ræ6öçF–ç2‡"2"&6÷VçBæWfW"&V6†VBR""2’À¢&æBG&÷2F†RöæRF†Bv2w&—GFVâ ¢“° ¢òòf÷"‚–âÇ66Æ#æ¢cVÖ—G2&ævRÖf÷"÷fW"fÇVRv—F‚æð¢òò&Vv–â‚–à¢ÆWBf÷%÷7&2Òf—‡GW&Rç&WÆ6R€¢"GWBæVâÒÆâ"À¢"f÷"‚–âGWBæ6÷VçEö÷WEÆâv—B7–6ÆUÆâVæBf÷%ÆâGWBæVâÒÆâ"À¢“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚ff÷%÷7&2’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¤VÖ—G5Væ6ö×–Æ&ÆRÀ¢“°¢76W'B†×6ræ6öçF–ç2‚&æò—FW&F÷""’Â'¶×6wÒ"“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚ff÷%÷7&2’¢æW‡V7B‚'c66WG2—B"¢æ6öçF–ç2‚&f÷"†WFòb‚¢†&5÷'C£¦†&5÷&VB†GWBÓæ6÷VçEö÷WB’’"’À¢'cVÖ—G2&ævRÖf÷"÷fW"66Æ" ¢“°§Ð ¢òòòFVfVÇF–ærâVæææ÷FFVBG6Ww2VÆVÖVçBG—Rv–FVæVBF†R66WFV@¢òòò6WBÂæBGvò6†V6·2F†Ræ'&÷vW"vFR†B&VVâ&÷f–F–ærf÷"g&VR†@¢òòòFò&Rw&—GFVâ'’†æBà¢òòð¢òòòF†—2—2F†R6ÖRf–ÇW&RÖöFR2F—fW&vVæ6W23bæBCB(	BföÆB÷ ¢òòòFVfVÇBF†B&WÆ6W2&V¦V7F–öâ–æ†W&—G2F†R&V¦V7F–öâw2¦ö"à¢5·FW7EÐ¦fâF†U÷G6WöFVfVÇEöFöW5öæ÷E÷7vÆÆ÷u÷v†Eö—E÷6†÷VÆE÷&V¦V7B‚’°¢6öç7B5$3¢g7G"Ò"2'G&ç67F–öâ&W¢¢V–çCÃƒà¦VæBG&ç67F–öâ&W §G6WvVâ†ã¢–çB’ÓâE6WÅ&Wà¢ÆWB’Ò¢v†–ÆR’ÃÒà¢ÆWBB¢&W¢BæÒ¢––VÆB@¢’Ò’²¢VæBv†–ÆP¦VæBG6WvVà §FW7F&Væ6‚ÕF ¢GWB¢F÷ ¦VæBFW7F&Væ6‚ÕF  ¦–×ÂÕFW7Bf÷"ÕF ¢'Và¢ÆWB‡2ÒvVâƒ"¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂÕFW7B"3°¢6öç7BDT4Ã¢g7G"Ò'G6WvVâ†ã¢–çB’ÓâE6WÅ&Wâ#° ¢òò6öçG&öÃ¢F†Rææ÷FFVB&V6÷&BG6WÆ÷vW'2à¢VÖ—Eö7÷7&2…5$2“° ¢òòG&÷–ærF†Rææ÷FF–öâFVfVÇG2F†RVÆVÖVçBFò44Ä"Â'WBF†P¢òò&öG’7F–ÆÂ––VÆG2&V6÷&Bâv—F†÷WBF†R––VÆB6†V6²F†BVÖ—GFV@¢òò7FC£§fV7F÷#Æ–çCcE÷Cã£§W6…ö&6²‡B–v—F‚F7G'V7B(	@¢òòVæ6ö×–Æ&ÆR2²²v—F‚æòF–væ÷7F–2à¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚e5$2ç&WÆ6R„DT4ÂÂ'G6WvVâ†ã¢–çB’"’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¤VÖ—G5Væ6ö×–Æ&ÆRÀ¢“°¢76W'B€¢×6ræ6öçF–ç2‚&æöâ×66Æ"fÇVR–âG6Wv†÷6RVÆVÖVçBG—R—266Æ""’À¢'¶×6wÒ ¢“°¢òòF†RÖW76vRæÖW2F†R&V6÷&BF†RW6W"6†÷VÆBææ÷FFRv—F‚Âæ÷B¢òòÆ6V†öÆFW"à¢76W'B†×6ræ6öçF–ç2‚&ÓâE6WÅ&Wæ"’Â'¶×6wÒ"“° ¢òò4UTTä4R×G—VB––VÆBÆV·2F†R6ÖRv’æB&V6÷&BÖöæÇ’6†V6°¢òòv÷VÆBÖ—72—C¢W6…ö&6¶öb7FC£§fV7F÷&–çFò¢òò7FC£§fV7F÷#Æ–çCcE÷Cæ—2§W7B26–ÆVçBà¢ÆWB6W÷––VÆBÒ5$0¢ç&WÆ6R€¢DT4ÂÀ¢'G6W–ææW"†ã¢–çB’ÓâE6WÇV–çCÃƒãåÆâ––VÆBÆæVæBG6W–ææW%ÆåÆçG6WvVâ†ã¢–çB’"À¢¢ç&WÆ6R€¢"ÆWBB¢&WÆâBæÒ•Æâ––VÆBEÆâ"À¢"ÆWB‡2Ò–ææW"ƒ"•Æâ––VÆB‡5Æâ"À¢“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g6W÷––VÆB’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¤VÖ—G5Væ6ö×–Æ&ÆRÀ¢“°¢76W'B†×6ræ6öçF–ç2‚&æöâ×66Æ"fÇVR"’Â'¶×6wÒ"“° ¢òò$U4TåB'WBVçW6&ÆRææ÷FF–öâ×W7Bæ÷B&V6‚F†RFVfVÇ@¢òòV—F†W#¢c&VæFW'2V6‚öbF†W6RF–ffW&VçFÇ’†E6WÆ–çCæÓà¢òòfV7F÷#ÇV–çCcE÷CæÂE6WÅfV3ÂâããæÓâfV7F÷#Ç7FC£¦'&“Ââããæ’À¢òò6òFVfVÇF–ærF†VÒFò–çCcE÷Fv÷VÆB6–ÆVçFÇ’6†ævRF†RVÆVÖVç@¢òòG—R&F†W"F†â6Æ÷6Rvà¢f÷"&WB–â°¢%E6WÆ–çCâ"À¢%E6WÇF–ÖSâ"À¢òò$U4TåBæöâÖE6W&WGW&â†2æòE6W&w2V—F†W"Â6ð¢òòvF–æröâ'F†R&w2F–Bæ÷B&W6öÇfR"v÷VÆB†fRÆWBF†W6P¢òòFVfVÇB6–ÆVçFÇ’âF†RvFR—2&WGW&å÷G–&V–ær%4TåBà¢%&W"À¢'V–çCÃƒâ"À¢Ò°¢ÆWB7&2Ò5$2ç&WÆ6R„DT4ÂÂff÷&ÖB‚'G6WvVâ†ã¢–çB’Óâ·&WGÒ"’“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&VÆVÖVçBG—R"’Â&Óâ·&WGÖ¢¶×6wÒ"“°¢Ð ¢òòF†RFVfVÇB7F–ÆÂÆ–W2v†W&R—B—26fS¢âVæææ÷FFVBG6W¢òòv†÷6R&öG’––VÆG266Æ'2à¢ÆWB66Æ"Ò5$2ç&WÆ6R„DT4ÂÂ'G6WvVâ†ã¢–çB’"’ç&WÆ6R€¢"ÆWBB¢&WÆâBæÒ•Æâ––VÆBEÆâ"À¢"––VÆB•Æâ"À¢“°¢76W'B€¢VÖ—Eö7÷7&2‚g66Æ"’æ6öçF–ç2‚"Óâ7FC£§fV7F÷#Æ–çCcE÷Câ"’À¢&âVæææ÷FFVB66Æ"G6W7F–ÆÂFVfVÇG2 ¢“°§Ð ¢òòò6ö×öæVçG2ç'6w27V"Ö6ö×öæVçBÖf–VÆB&Ò6÷fW&VBGvò–çWG2F†Bc¢òòòG&VG2÷÷6—FVÇ’ÂæB6Æ76–f–VBF†VÒÆ–¶Rà¢òòð¢òòòDT4Ä$TBG—RF†B6–×Ç’—2æ÷B7W÷'FVB7V"Ö6ö×öæVçB¶–æB(	B¢òòò6÷fW&w&÷WÂ6’(	B—2&VÂcW66R†F6ƒ¢cVÖ—G0¢òòòæÇ—6—46÷d6öÆÆV7F÷#"vV—&C¶æBv—&W2—G26×Æ–æp¢òòò†VçbçvV—&Bæ7æ#²¶’âæÖRFV6Æ&VBäõt„U$R—2G—òÂæBc¢òòò77VÖW2fW&–ÆFVBEUB†æFÆS¢dæõ7V6…F†–ær¢vV—&BÒçVÆÇG#¶À¢òòòæÖ–ærG—RF†BFöW2æ÷BW†—7Bà¢òòð¢òòò&ö&VBv–ç7BæÇ—6—5÷6–æµö6öææV7E÷FW7FÂv†–6‚—2&Vv—7FW&VBÀ¢òòò6VÆbÖ6öçF–æVBÂæB76W2VæFW"&÷F‚&6¶VæG2à¢5·FW7EÐ¦fâå÷VæFV6Æ&VEö6ö×öæVçEöf–VÆE÷G—Uö—5ö÷G—õöæ÷Eö÷7V'6WEöv‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&æÇ—6—5÷6–æµö6öææV7E÷FW7Bæ†&2"“°¢6öç7Bä4„õ#¢g7G"Ò"6÷b¢æÇ—6—46÷d6öÆÆV7F÷"#°¢6öç7BU…E$ô4õc¢g7G"Ò"2&6÷fW&w&÷WW‡G&6÷b‡÷6VFvRGWBæ6Æ²¢7¢6÷fW"GWBæ6÷VçEö÷W@¢&–ç0¢#Ò³Ð¢VæB&–ç0¦VæB6÷fW&w&÷WW‡G&6÷` ¦VçbæÇ—6—4Vçb"3° ¢òò6öçG&öÃ¢F†R&Vv—7FW&VBf—‡GW&RÆ÷vW'2VæFW"&÷F‚&6¶VæG2à¢VÖ—Eö7÷7&2‚ff—‡GW&R“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚ff—‡GW&R’’æW‡V7B‚'cVÖ—G2F†R6öçG&öÂ"“° ¢òòæÖRFV6Æ&VBæ÷v†W&S¢&öw&ÒW'&÷"VæFW"WfW'’&6¶VæBà¢ÆWBG—òÒf—‡GW&Rç&WÆ6R„ä4„õ"Âff÷&ÖB‚'´ä4„õ'ÕÆâvV—&B¢æõ7V6…F†–ær"’“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚gG—ò’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&æ÷BFV6Æ&VBç—v†W&R–âF†Rf–ÆR"’Â'¶×6wÒ"“°¢òòcw2Wf–FVæ6S¢—B–çfVçG2fW&–ÆFVB†æFÆRG—Rf÷"—Bà¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gG—ò’¢æW‡V7B‚'c66WG2F†RG—ò"¢æ6öçF–ç2‚%dæõ7V6…F†–ær¢vV—&BÒçVÆÇG#²"’À¢'cVÖ—G2âVæFV6Æ&VBEUBÖ†æFÆRö–çFW" ¢“° ¢òòDT4Ä$TBG—RöbâVç7W÷'FVB¶–æB7F—2Vç7W÷'FVFÂ6–æ6P¢òòc†æFÆW2—B6÷'&V7FÇ’(	BF†R&Ò×W7Bæ÷B7vVWF†RGvòFövWF†W"à¢ÆWBFV6Æ&VBÒf—‡GW&P¢ç&WÆ6R„ä4„õ"Âff÷&ÖB‚'´ä4„õ'ÕÆâvV—&B¢W‡G&6÷b"’¢ç&WÆ6Vâ‚&VçbæÇ—6—4Vçb"ÂU…E$ô4õbÂ“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚fFV6Æ&VB’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚'7V"Ö6ö×öæVçBf–VÆB"’Â'¶×6wÒ"“° ¢òòâTåTÒ—2F†R66RF†Rf—'7BG&gB'&ö¶S¢F†RFV6Æ&VB×G—R6W@¢òòv2v†—FVÆ—7BöbF†R¶–æG2F†B6VVÖVB&VÆWfçBÂæBöÖ—GF–æp¢òòöæRGW&ç2fÆ–B&öw&Ò–çFòfÇ6R&æ÷BFV6Æ&VBç—v†W&R"à¢òòF†R6WB—2÷fW"Ö–æ6ÇW6—fRf÷"W†7FÇ’F†—2&V6öâ(	BÖ—76–æræÖP¢òò—2†&BW'&÷"öâv÷&¶–ær6öFRÂ7W&–÷W2öæR—2§W7BF†R†öæW7@¢òòVç7W÷'FVFà¢ÆWBVçVÕöf–VÆBÒf÷&ÖB€¢&VçVÒÖöFR·²Â"×ÕÆåÆç·Ò"À¢f—‡GW&Rç&WÆ6R„ä4„õ"Âff÷&ÖB‚'´ä4„õ'ÕÆâvV—&B¢ÖöFR"’¢“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚fVçVÕöf–VÆB’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚'7V"Ö6ö×öæVçBf–VÆB"’À¢&âVçVÒ—2FV6Æ&VC¢¶×6wÒ ¢“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fVçVÕöf–VÆB’¢æW‡V7B‚'cVÖ—G2âVçVÒf–VÆB"¢æ6öçF–ç2‚$ÖöFRvV—&B"’À¢'c†æFÆW2—BÂ6òc—2&VÂW66R†F6‚†W&R ¢“°¢òòcw2Wf–FVæ6S¢&VÂÖVÖ&W"ÂæBF†R6×Æ–ær—2v—&VBà¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fFV6Æ&VB’’æW‡V7B‚'cVÖ—G26÷fW&w&÷Wf–VÆB"“°¢76W'B€¢cæ6öçF–ç2‚$W‡G&6÷bvV—&C²"’À¢'cFV6Æ&W2F†RÖVÖ&W#¥Æç·cÒ ¢“°¢76W'B‡cæ6öçF–ç2‚&VçbçvV—&Bæ7æ#²²"’Â'cv—&W2F†R6×Æ–ær"“°§Ð ¢òòòF&vWB×6W'f–ærF‡&VFw&—GFVâöââVçbövVçB–ç7FVBöb¢òòò'W2Ö&÷VæBG&ç67F÷"âc66WG2—BæBVÖ—G2F†R6ö×öæVçBt•D„õU@¢òòò—B(	BæòF‡&VE6Æ÷FÂæò66†VGVÆW"&Vv—7G&F–öâÂæò6÷&÷WF–æR(	B6ð¢òòòF†RF&vWB6–ÆVçFÇ’æWfW"6W'fW2à¢òòð¢òòòF†Rf—'7B&ö&RöbF†—2'&÷fVB"—B'’6†÷v–ærcw2÷WGWBv0¢òòò'—FRÖ–FVçF–6Âv—F‚æBv—F†÷WBF†RF‡&VBâF†B&÷fVBæ÷F†–æs¢à¢òòòTä$õTäBF‡&VBVÖ—G2æ÷F†–ærVæFW"cv†W&WfW"—B6—G2Â6òF†P¢òòò6ö×&—6öâ†Bæò6–væÂ–â—Bâ&÷F‚æ6†÷'2&VÆ÷rW†—7B&V6W6Rö`¢òòòF†Bà¢5·FW7EÐ¦fâ÷F‡&VEö–åöö6ö×öæVçEö—5÷6–ÆVçFÇ•öG&÷VEö'•÷c‚’°¢ÆWB&÷VæBÒf—‡GW&R‚'FÆÕ÷F&vWE÷F‡&VE÷FW7Bæ†&2"“°¢ÆWBF‡&VE÷7F'BÒ&÷Væ@¢æf–æB‚"F‡&VB'W2ç&VB†FG#¢V–çCÃƒâ’"¢æW‡V7B‚&f—‡GW&R6†R6†ævVB"“°¢ÆWBF‡&VEöVæBÒ&÷VæE·F‡&VE÷7F'BâåÐ¢æf–æB‚&VæBF‡&VB"¢æÖ‡Æ—ÂF‡&VE÷7F'B²’²&VæBF‡&VEÆâ"æÆVâ‚’¢æW‡V7B‚&f—‡GW&R6†R6†ævVB"“°¢ÆWBF‡&VE÷7&2Òf&÷VæE·F‡&VE÷7F'BâçF‡&VEöVæEÓ° ¢òò6öçG&öÃ¢F†R&Vv—7FW&VBf—‡GW&RVÖ—G2VæFW"&÷F‚&6¶VæG2à¢VÖ—Eö7÷7&2‚f&÷VæB“°¢ÆWBcö&÷VæBÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f&÷VæB’’æW‡V7B‚'cVÖ—G2F†R6öçG&öÂ"“° ¢òòõ4•D•dRæ6†÷#¢v†W&RF†R6öç7G'V7B•27W÷'FVBÂ—BvVçV–æVÇ¢òò6öçG&–'WFW2(	B&VÖ÷f–ær—B6÷7G2F†RF‡&VE6Æ÷BæBF†R6÷&÷WF–æRà¢òòv—F†÷WBF†—2Â&æòF–fb"&VÆ÷rv÷VÆB&RVç&VF&ÆRà¢ÆWBv—F†÷WBÒf÷&ÖB‚'·×·Ò"Âf&÷VæE²âçF‡&VE÷7F'EÒÂf&÷VæE·F‡&VEöVæBâåÒ“°¢ÆWBc÷v—F†÷WBÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gv—F†÷WB’’æW‡V7B‚'cVÖ—G2"“°¢òò6÷VçFVBÂæ÷B§W7B&W6VçC¢÷F†W"Ö6†–æW'’VÖ—G2F‡&VE6Æ÷G2FöòÀ¢òò6ò6öçF–ç6v÷VÆB&R6F—6f–VB'’F†÷6RæBÖV7W&Ræ÷F†–ærà¢76W'B€¢cö&÷VæBæÖF6†W2‚&†&5÷'C£¥F‡&VE6Æ÷B"’æ6÷VçB‚¢âc÷v—F†÷WBæÖF6†W2‚&†&5÷'C£¥F‡&VE6Æ÷B"’æ6÷VçB‚’À¢&'W2Ö&÷VæBF‡&VBFG2F‡&VE6Æ÷C²&VÖ÷f–ær—BF¶W2öæRv’ ¢“° ¢òòäTtD•dRæ6†÷#¢F†R6ÖRF‡&VBÖ÷fVB–çFòâVçfFG2öæÇ’F†P¢òòV×G’6ö×öæVçB7G'V7BâæòF‡&VE6Æ÷BÂæò6W'f–ærÆöv–2à¢òò”å5DåD”DTBâc&Vv—7FW'266†VGVÆW"6Æ÷G2BF†RÆWF6—FRÂ6òà¢òòVçbF†B—2FV6Æ&VB'WBæWfW"&÷VæB6öçG&–'WFW2æ÷F†–ærv†WF†W"÷ ¢òòæ÷BF†RF‡&VB–ç6–FR—Bv÷&·2(	BF†R6÷VçBWVÆ—G’v÷VÆB†öÆ@¢òòG&—f–ÆÇ’æBÖV7W&RW†7FÇ’æ÷F†–ærÂv†–6‚—2F†Rf–ÇW&RF†—0¢òòFW7BW†—7G2Fòfö–Bà¢ÆWB–åöVçbÒ&÷Væ@¢ç&WÆ6Vâ€¢'FW7F&Væ6‚FÆÕF&vWEF‡&VEF""À¢ff÷&ÖB‚&Vçbw&VçeÆç·F‡&VE÷7&7ÖVæBVçbw&VçeÆåÆçFW7F&Væ6‚FÆÕF&vWEF‡&VEF""’À¢À¢¢ç&WÆ6Vâ€¢"ÆWBF&vWB¢FÆÔÖVÕF&vWB76—fRÒ&–æBÖVÒ"À¢"ÆWBF&vWB¢FÆÔÖVÕF&vWB76—fRÒ&–æBÖVÕÆâÆWBw&¢w&Vçb"À¢À¢“°¢76W'B€¢–åöVçbæ6öçF–ç2‚&ÆWBw&¢w&Vçb"’À¢'F†RVçb—2–ç7FçF–FVB ¢“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚f–åöVçb’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚&F‡&VF—FVÒ–â6ö×öæVçB"’Â'¶×6wÒ"“° ¢ÆWBcöVçbÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f–åöVçb’’æW‡V7B‚'c66WG2F‡&VBöââVçb"“°¢76W'B€¢cöVçbæ6öçF–ç2‚'7G'V7Bw&Vçb"’À¢'cVÖ—G2F†R6ö×öæVçB7G'V7B ¢“°¢òòF†R6÷VçB—2v†BÖGFW'3¢F†RVçbw2F‡&VBFG2æòäUr6Æ÷Bà¢76W'EöW€¢cöVçbæÖF6†W2‚&†&5÷'C£¥F‡&VE6Æ÷B"’æ6÷VçB‚’À¢cö&÷VæBæÖF6†W2‚&†&5÷'C£¥F‡&VE6Æ÷B"’æ6÷VçB‚’À¢'F†RVçbÖ†VÆBF‡&VB6öçG&–'WFW2æòF‡&VE6Æ÷B(	B—B6–ÆVçFÇ’æWfW"6W'fW2 ¢“°§Ð ¢òòòF†R6ÖR&ÒÇ6ò6F6†W2F‡&VFöâG&ç67F÷"F†B•0¢òòò'W2Ö&÷VæC¢G&ç67F÷%ö—5ö6ö×öæVçF&÷WFW2öæRF÷vâF†R6ö×öæVç@¢òòòF‚v†Vâ—BFF—F–öæÆÇ’†2æöâ×W&–öF–2öæ†æFÆW"à¢òòð¢òòòF†W&RF†R6öç7G'V7Bv÷&·2(	BVÖ—Eö&÷VæE÷FÆÕ÷F&vWEö7F÷'6VÖ—G2F†P¢òòòF&vWB7F÷"&Vv&FÆW72öbF†Röæ†æFÆW"(	B6òc—2vVçV–æP¢òòòW66R†F6‚æBF†R&Ò¶VW2Vç7W÷'FVFâF†Rf—'7BfW'6–öâö`¢òòòF†R7Æ—B&V6Æ76–f–VBF†Rv†öÆR&Òg&öÒ&ö&RF†BöæÇ’WfW"W@¢òòòF‡&VFöââVçbà¢5·FW7EÐ¦fâ÷F‡&VEööåöö&÷VæE÷G&ç67F÷%÷7F–ÆÅ÷ö–çG5öE÷c‚’°¢ÆWB&÷VæBÒf—‡GW&R‚'FÆÕ÷F&vWE÷F‡&VE÷FW7Bæ†&2"“°¢VÖ—Eö7÷7&2‚f&÷VæB“° ¢òòFBæöâ×W&–öF–2öæ†æFÆW"Âv†–6‚—2v†B&÷WFW2F†—0¢òòG&ç67F÷"F÷vâF†R6ö×öæVçBF‚à¢òò'W2ç&VF—2FÆÕöÖWF†öBÂ6òF†Röæ†æFÆW"æVVG2&VÀ¢òò†æG6†¶R6†ææVÂ(	BF†R'W2—2v—fVâöæR†W&Râæòf—‡GW&R–âF†P¢òò6÷'W2†2F†—26†R†&÷VæBG&ç67F÷"²öæ²F‡&VF’Âv†–6€¢òò—2v‡’—BvVçBVç&ö&VBF†Rf—'7BF–ÖRà¢ÆWBf–ö6ö×öæVçBÒ&÷Væ@¢ç&WÆ6Vâ€¢"FÆÕöÖWF†öB&VB†FG#¢V–çCÃƒâ’ÓâV–çCÃ3#ã¢&Æö6¶–æs²"À¢"2"FÆÕöÖWF†öB&VB†FG#¢V–çCÃƒâ’ÓâV–çCÃ3#ã¢&Æö6¶–æs°¢†æG6†¶Uö6†ææVÂö'3¢&V6V—fR¶–æC¢fÆ–E÷&VG¢FF¢T–çCÃƒã°¢VæB†æG6†¶Uö6†ææVÂö'2"2À¢À¢¢ç&WÆ6Vâ€¢'G&ç67F÷"FÆÔÖVÕF&vWB&÷VæBFòFÆÔÖVÔ'W2"À¢'G&ç67F÷"FÆÔÖVÕF&vWB&÷VæBFòFÆÔÖVÔ'W5Æâ6–æ²¢V–çCÃƒâFVfVÇB"À¢À¢¢ç&WÆ6Vâ€¢"F‡&VB'W2ç&VB†FG#¢V–çCÃƒâ’"À¢"öâ'W2æö'2æ†æG6†¶R‡b•Æâ6–æ²ÒeÆâVæBöåÆåÆâF‡&VB'W2ç&VB†FG#¢V–çCÃƒâ’"À¢À¢“°¢ÆWBW'"ÒÆ÷vW%÷7&2‚gf–ö6ö×öæVçB’çVçw&öW'"‚“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fW'"“°¢76W'B†×6ræ6öçF–ç2‚&&÷VæBG&ç67F÷""’Â'¶×6wÒ"“° ¢òòcw2Wf–FVæ6S¢F†RF&vWB7F÷"—27F–ÆÂVÖ—GFVBÂ6òö–çF–ærF†P¢òòW6W"Bc—267W&FR&F†W"F†âFVBVæBà¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gf–ö6ö×öæVçB’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢cæÖF6†W2‚&†&5÷'C£¥F‡&VE6Æ÷B"’æ6÷VçB‚¢â7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f&÷VæB’¢æW‡V7B‚'cVÖ—G2"¢æÖF6†W2‚&†&5÷'C£¥F‡&VE6Æ÷B"¢æ6÷VçB‚’À¢'F†Röæ†æFÆW"FG26Æ÷G2æBF†RF‡&VBw27F÷"7W'f—fW2 ¢“°§Ð ¢òòòF†R6ö×öæVçG2ç'6†æFÆW"×fÆ–FF÷"fÖ–Ç“¢D…$TR†öö²&×2æBöæP¢òòò†6R&ÒÂæBF†R†öö²&×2æBF†R†6R&Ò6Æ76–g’÷÷6—FRv—0¢òòòFW7—FR6—GF–ærf÷W"Æ–æW2'Bà¢òòð¢òòò&Vö÷7F†öö²öâ7–6ÆR×G&–vvW"ÂW&–öF–2Â÷"WfVçBÐ¢òòò7V'67&—F–öâ†æFÆW"—26–ÆVçFÇ”Ö—4Æ÷vW'6¢cVÖ—G2F†R†æFÆW ¢òòòv—F‚F†R†öö²6–FRD•44$DTBÂ'—FRÖ–FVçF–6ÆÇ’FòF†R6ÖR†æFÆW ¢òòòw&—GFVâv—F†÷WB—BâF†RW6W"6·2f÷"&R÷÷7B÷&FW&–æræBvWG2F†P¢òòòFVfVÇBà¢òòð¢òòòæöâÖFVfVÇB„4R—2æ÷BF†R6ÖRâc–×ÆVÖVçG2—B(	@¢òòò†6R÷7EöWfÆVÖ—G2÷÷7EöWfÅ÷6W'f–6W2çW6…ö&6¶v†W&RF†P¢òòòFVfVÇBVÖ—G2ö6†V6¶W'2çW6…ö&6¶(	B6òc—2&VÂW66R†F6€¢òòòæBF†B&Ò¶VW2Vç7W÷'FVFà¢òòð¢òòòWfW'’'—FRÖ–FVçF—G’&VÆ÷r—2—&VBv—F‚—G2õtâæ6†÷"‡F†R6ÖP¢òòò×WFF–öâv—F‚F†R†æFÆW"&VÖ÷fVB&F†W"F†âF†R†öö²FFVB’â¢òòò6†&VBæ6†÷"v÷VÆBæ÷BFó¢—B†öÆG2f÷"öæRG&–vvW"¶–æBæB6—0¢òòòæ÷F†–ær&÷WBF†R÷F†W'2ÂæB–ââVæ–ç7FçF–FVB6ö×öæVçB—B—0¢òòòf7V÷W6Ç’G'VRf÷"ÆÂöbF†VÒà¢5·FW7EÐ¦fâö†æFÆW%ö†ööµö—5öG&÷VEö'WEöö†æFÆW%÷†6Uö—5ö–×ÆVÖVçFVB‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&vVçE÷W&–öF–5÷FW7Bæ†&2"“°¢6öç7BU$”ôD”3¢g7G"Ò"öâ7–6ÆW2#° ¢òò&WÆ6RF†R†æFÆW"w2E$”ttU"Æ–æRÂæB6W&FVÇ’&öGV6RF†P¢òò6ÖR6÷W&6Rv—F‚F†Rv†öÆR†æFÆW"FVÆWFVB(	BF†Ræ6†÷"F†@¢òòÖ¶W2ÆFW"'—FRÖ–FVçF—G’ÖVâ'F†RÖöF–f–W"v2G&÷VB ¢òò&F†W"F†â'F†R†æFÆW"v2–æW'B"à¢ÆWB7F'BÒf—‡GW&Ræf–æB…U$”ôD”2’æW‡V7B‚&f—‡GW&R6†R6†ævVB"“°¢ÆWBVæBÒf—‡GW&U·7F'BâåÐ¢æf–æB‚&VæBöâ"¢æÖ‡Æ—Â7F'B²’²&VæBöåÆâ"æÆVâ‚’¢æW‡V7B‚&f—‡GW&R6†R6†ævVB"“°¢ÆWBv—F†÷WBÒf÷&ÖB‚'·×·Ò"Âff—‡GW&U²âç7F'EÒÂff—‡GW&U¶VæBâåÒ“°¢ÆWBc÷v—F†÷WBÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gv—F†÷WB’’æW‡V7B‚'cVÖ—G2"“° ¢òò76W'B†öö¶—2G&÷VB&VÆF—fRFò7FÆÂv—F‚7FÆ—G6VÆ`¢òòæ6†÷&VBv–ç7BF†R†æFÆW"Ög&VR6÷W&6Râ&÷F‚×WFF–öç2F–ffW ¢òòg&öÒ7FÆ–âW†7FÇ’öæRFö¶Vâà¢ÆWB6†V6µöG&÷VBÒÆ7FÅ÷G&–vvW#¢g7G"Â†ööµ÷G&–vvW#¢g7G"Â6öç7G'V7C¢g7G'Â°¢ÆWB7FÂÒf—‡GW&Rç&WÆ6Vâ…U$”ôD”2Â7FÅ÷G&–vvW"Â“°¢ÆWBcö7FÂÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f7FÂ’’æW‡V7B‚'cVÖ—G2F†R6öçG&öÂ"“°¢76W'EöæR€¢cö7FÂÂc÷v—F†÷WBÀ¢&¶7FÅ÷G&–vvW'Ö×W7BvVçV–æVÇ’6öçG&–'WFRÂ÷"F†R–FVçF—G’&VÆ÷r—2f7V÷W2 ¢“°¢ÆWB†öö¶VBÒf—‡GW&Rç&WÆ6Vâ…U$”ôD”2Â†ööµ÷G&–vvW"Â“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚f†öö¶VB’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2†6öç7G'V7B’Â'¶×6wÒ"“°¢76W'EöW€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f†öö¶VB’’æW‡V7B‚'cVÖ—G2"’À¢cö7FÂÀ¢'cVÖ—G2¶†ööµ÷G&–vvW'Öv—F‚F†R†öö²F—66&FVB ¢“°¢cö7FÀ¢Ó° ¢6†V6µöG&÷VB…U$”ôD”2Â"öâ7–6ÆW2&R"Â&öâÄãâ7–6ÆW6†æFÆW""“°¢òòF†R5”4ÄRÕE$”ttU"6öçG&öÂ6'&–W2—G2÷vâæ6†÷"â&WW6–ærF†P¢òòW&–öF–2öæRv÷VÆB6†ævRF†RG&–vvW"¶–æBäBF†RÖöF–f–W"@¢òòöæ6RÂv†–6‚ÖFR&÷F‚&×2&VB2&F–ffW'2"æB†–BF†R7Æ—Bà¢ÆWBcö5ö7FÂÒ6†V6µöG&÷VB€¢"öâ&VG2â"À¢"öâ&VG2â&R"À¢&7–6ÆR×G&–vvW"öæ†æFÆW""À¢“° ¢òòF†R„4RÂ'’6öçG&7BÂc–×ÆVÖVçG2à¢ÆWB5÷†6RÒf—‡GW&Rç&WÆ6Vâ…U$”ôD”2Â"öâ&VG2â†6R÷7EöWfÂ"Â“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚f5÷†6R’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&æöâÖFVfVÇB×†6R"’Â'¶×6wÒ"“°¢ÆWBc÷†6RÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f5÷†6R’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢c÷†6Ræ6öçF–ç2‚%÷÷7EöWfÅ÷6W'f–6W2çW6…ö&6²"’À¢&†6R÷7EöWfÆ6VÆV7G2F†R÷7BÖWfÂF—7F6‚fV7F÷" ¢“°¢76W'B€¢cö5ö7FÂæ6öçF–ç2‚%ö6†V6¶W'2çW6…ö&6²"¢bbcö5ö7FÂæ6öçF–ç2‚%÷÷7EöWfÅ÷6W'f–6W2çW6…ö&6²"’À¢&æBF†RFVfVÇB&Vv—7FW'2–çFòö6†V6¶W'6öæÇ’Â6òF†R†6R—2æ÷BæòÖ÷ ¢“°§Ð ¢òòòF†RF†—&B†öö²&ÒÂöâF†RWfVçB×7V'67&—F–öâFƒ¢öâWb‡B’&Và¢òòò&ö&W2–FVçF–6ÆÇ’FòF†R÷F†W"Gvò(	BcVÖ—G2F†R7V'67&—F–öâv—F€¢òòòF†R†öö²6–FRF—66&FVB(	B6ò—B6'&–W2F†R6ÖR6–ÆVçFÇ”Ö—4Æ÷vW'6 ¢òòòfW&F–7B&F†W"F†âF†RÒÖ6öFVvVâc7VvvW7F–öâ—BW6VBFòà¢5·FW7EÐ¦fâåöWfVçE÷7V'67&—F–öåö†ööµö—5öG&÷VEö'•÷c÷Föò‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&†V'F&VEö–FÆU÷FW7Bæ†&2"“°¢6öç7B5T#¢g7G"Ò"öâ–åöWb‡B’#° ¢ÆWBcö7FÂÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚ff—‡GW&R’’æW‡V7B‚'cVÖ—G2F†R6öçG&öÂ"“°¢ÆWB7F'BÒf—‡GW&Ræf–æB…5T"’æW‡V7B‚&f—‡GW&R6†R6†ævVB"“°¢ÆWBVæBÒf—‡GW&U·7F'BâåÐ¢æf–æB‚&VæBöâ"¢æÖ‡Æ—Â7F'B²’²&VæBöåÆâ"æÆVâ‚’¢æW‡V7B‚&f—‡GW&R6†R6†ævVB"“°¢ÆWBv—F†÷WBÒf÷&ÖB‚'·×·Ò"Âff—‡GW&U²âç7F'EÒÂff—‡GW&U¶VæBâåÒ“°¢76W'EöæR€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gv—F†÷WB’’æW‡V7B‚'cVÖ—G2"’À¢cö7FÂÀ¢'F†R7V'67&—F–öâvVçV–æVÇ’6öçG&–'WFW2Â6òF†R–FVçF—F–W2&VÆ÷rÖVâ6öÖWF†–ær ¢“° ¢f÷"†öö²–â²'&R"Â'÷7B%Ò°¢ÆWB†öö¶VBÒf—‡GW&Rç&WÆ6Vâ…5T"Âff÷&ÖB‚"öâ–åöWb‡B’¶†öö·Ò"’Â“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚f†öö¶VB’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚&&Vö÷7F†öö²öæ†æFÆW""’Â'¶×6wÒ"“°¢76W'EöW€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f†öö¶VB’’æW‡V7B‚'cVÖ—G2"’À¢cö7FÂÀ¢'cVÖ—G2F†R7V'67&—F–öâv—F‚F†R¶†öö·Ö†öö²F—66&FVB ¢“°¢Ð§Ð ¢òòòF†R7–6ÆR×G&–vvW"†öö²&Òw2õD„U"–çWC¢7V2*srã2ÖWF†öB†öö°¢òòòw&—GFVâ–â6ö×öæVçB&öG’–ç7FVBöbBFW7B66÷Râ—B—2æ÷Bà¢òòòWfVçB7V'67&—F–öâæBæ÷B†æG6†¶RÖöæ—F÷"Â6ò—BÆæG2–âF†P¢òòò6ÖR&Ò27G&’&Vöâ&ööÂW‡&W76–öâ(	B'WBcf–Ç2—@¢òòòF–ffW&VçFÇ’ÂG&÷–ærF†R†öö²äBVFvRÖFWFV7F–æröâ7G'V7BÖVÖ&W ¢òòòF†BFöW2æ÷BW†—7Bà¢òòð¢òòòF†BÖ¶W2F†R&Òw2–çWB76Ræöâ×Væ–f÷&Òâ6–ÆVçFÇ”Ö—4Æ÷vW'6—0¢òòò7F–ÆÂF†R†öæW7BÆ&VÂ†—B—2F†Rv÷'6RöbF†RGvò÷WF6öÖW2’Â'WBF†P¢òòòÖW76vR×W7Bæ÷B6Æ–Ò'—FRÖ–FVçF—G’Âv†–6‚†öÆG2öæÇ’f÷"F†R÷F†W ¢òòò6†Rà¢5·FW7EÐ¦fâöÖWF†öEö†ööµö–åöö6ö×öæVçEö&öG•÷&V6†W5÷F†Uö7–6ÆU÷G&–vvW%ö&Ò‚’°¢6öç7B$4S¢g7G"Ò"2 ¦FöÖ–â7—4FöÖ–à¢g&WöÖ‡£¢ ¦VæBFöÖ–â7—4FöÖ–à §G&ç67F–öâ&Vt÷ ¢FG"¢V–çCÃƒà¢fÇVR¢V–çCÃ3#à¦VæBG&ç67F–öâ&Vt÷  §G&ç67F÷"6VæFW ¢GWB¢F÷  ¢v†Vâ7F—fP¢†öö¶&ÆR6VæB‡C¢&Vt÷¢GWBæVâÒ¢VæB6Væ@¢VæBv†Và¦VæBG&ç67F÷"6VæFW  ¦Vçb†öö´Vç`¢2¢6VæFW"7F—fP¤„ôô°¦VæBVçb†öö´Vç` §FW7BÖWF†öD†ööµFW7@¢ÆWBGWB¢F÷ ¢ÆWBR¢†öö´Vç` ¢6Æö6²6Æ²Ò7—4FöÖ–à ¢'Và¢Rç2æGWBÒGW@¢v—B"7–6ÆW0¢VæB'Và¦VæBFW7BÖWF†öD†ööµFW7@¢"3°¢ÆWB7FÂÒ$4Rç&WÆ6R‚$„ôôµÆâ"Â""“°¢ÆWB†öö¶VBÒ$4Rç&WÆ6R€¢$„ôô²"À¢"öâ2ç6VæB&UÆâÆör†–æfòÂÂ'&V†ööµÂ"•ÆâVæBöâ"À¢“° ¢òòF†R&Ò—BÆæG2–âÂæBF†RÖW76vR—B×W7BäõBÖ¶Rà¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚f†öö¶VB’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚&7–6ÆR×G&–vvW"öæ†æFÆW""’Â'¶×6wÒ"“° ¢òòcFöW2äõBG&÷F†R†öö²6–ÆVçFÇ’†W&S¢F†R÷WGWBF–ffW'2g&öÐ¢òòF†R6öçG&öÂÂæBv†B—BFG2—27–6ÆRG&–vvW"&VF–æp¢òòRç2ç6VæF(	BÖVÖ&W"F†RVÖ—GFVB7G'V7B6VæFW&FöW2æ÷B†fRÀ¢òò6òF†R2²²FöW2æ÷B6ö×–ÆRà¢ÆWBcö7FÂÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f7FÂ’’æW‡V7B‚'cVÖ—G2F†R6öçG&öÂ"“°¢ÆWBcö†öö¶VBÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f†öö¶VB’’æW‡V7B‚'cVÖ—G2"“°¢76W'EöæR€¢cö7FÂÂcö†öö¶VBÀ¢'F†R'—FRÖ–FVçF—G’F†B†öÆG2f÷"7G&’&VFöW2æ÷B†öÆB†W&R ¢“°¢76W'B€¢cö†öö¶VBæ6öçF–ç2‚"†&ööÂ’†Rç2ç6VæB’"’À¢'cÆ÷vW'2F†RÖWF†öB†öö²27–6ÆRG&–vvW"öâF†RÖWF†öBF‚ ¢“°¢ÆWB6VæFW%÷7G'V7BÒcö†öö¶V@¢ç7Æ—Eööæ6R‚'7G'V7B6VæFW"²"¢ææE÷F†Vâ‡Â…òÂ&W7B—Â&W7Bç7Æ—Eööæ6R‚'Ó²"’¢æÖ‡Â†&öG’Âò—Â&öG’çFõ÷7G&–ær‚’¢æW‡V7B‚'cVÖ—G26VæFW&7G'V7B"“°¢76W'B€¢6VæFW%÷7G'V7Bæ6öçF–ç2‚'6VæB"’À¢&æò6VæFÖVÖ&W"Â6ò†&ööÂ’†Rç2ç6VæB–6ææ÷B6ö×–ÆS¢·6VæFW%÷7G'V7GÒ ¢“°§Ð ¢òòòF†R&W7BöbF†RöâÆö&£âãÆÖWF†öCâ&R÷÷7F†öö²fÖ–Ç’Âv†–6€¢òòò7ç2f÷W"÷6—F–öç2&F†W"F†âF†RöæR6ö×öæVçG2ç'6÷vç2â¢òòòW6W"w&—FW2F†R6ÖR6öç7G'V7B–âV6ƒ²cFöW2F‡&VRF–ffW&Vç@¢òòòF†–æw2Â6òF†RF‡&VR7W'f—f–ær6—FW26Æ76–g’F‡&VRv—2à¢òòð¢òòò„ôô¶Ö&·2F†RFW7F&Væ6‚FV6Æ&F–öâÂ”ÕÆF†RFW7B&öG’æ@¢òòò$ôE–7FFVÖVçB÷6—F–öâ–ç6–FR'Væ(	BF†RF‡&VRÆ6VÖVçG2¢òòòW6W"v÷VÆBG'’gFW"6ö×öæVçB&öG’&V¦V7G2F†R†öö²à¦6öç7B„ôôµõõ4•D”ôå5õ5$3¢g7G"Ò"2 ¦FöÖ–â7—4FöÖ–à¢g&WöÖ‡£¢ ¦VæBFöÖ–â7—4FöÖ–à §G&ç67F–öâ&Vt÷ ¢FG"¢V–çCÃƒà¦VæBG&ç67F–öâ&Vt÷  §G&ç67F÷"6VæFW ¢GWB¢F÷ ¢v†Vâ7F—fP¢†öö¶&ÆR6VæB‡C¢&Vt÷¢GWBæVâÒ¢VæB6Væ@¢VæBv†Và¦VæBG&ç67F÷"6VæFW  ¦vVçBvF6†W ¢6VVâ¢V–çCÃ3#âFVfVÇB ¢Wb¢WfVçCÅ&Vt÷à¢†öö¶&ÆRæ÷FR‡C¢&Vt÷¢6VVâÒ6VVâ²¢VæBæ÷FP¢gVæ7F–öâÆ–â‡C¢&Vt÷¢6VVâÒ6VVâ²¢VæBÆ–à¦VæBvVçBvF6†W  ¦Vçb†öÆFW ¢–ææW"¢vF6†W ¦VæBVçb†öÆFW  §FW7F&Væ6‚†ööµF ¢GWB¢F÷ ¢2¢6VæFW"7F—fP¢r¢vF6†W ¢R¢†öÆFW ¤„ôô°¦VæBFW7F&Væ6‚†ööµF  ¦–×Â†ööµFW7Bf÷"†ööµF ¢6Æö6²6Æ²Ò7—4FöÖ–à¢ÆWBö²¢V–çCÃ3#âÒ ¤”ÕÀ¢'Và¢2æGWBÒGW@¤$ôE¢v—B"7–6ÆW0¢VæB'Và¦VæB–×Â†ööµFW7@¢"3° ¢òòòf–ÆÂF†RF‡&VRÆ6V†öÆFW'2ÂG&÷–ærF†RöæW2ÆVgBV×G’à¦fâ†ööµ÷÷6—F–öç2‡F#¢g7G"Â–×¢g7G"Â&öG“¢g7G"’Óâ7G&–ær°¢ÆWBf–ÆÂÒÇ3¢g7G"Â¶W“¢g7G"Âc¢g7G'Â°¢–bbæ—5öV×G’‚’°¢2ç&WÆ6R‚ff÷&ÖB‚'¶¶W—ÕÆâ"’Â""¢ÒVÇ6R°¢2ç&WÆ6R†¶W’Âb¢Ð¢Ó°¢ÆWB2Òf–ÆÂ„„ôôµõõ4•D”ôå5õ5$2Â$„ôô²"ÂF"“°¢ÆWB2Òf–ÆÂ‚g2Â$”ÕÂ"Â–×“°¢f–ÆÂ‚g2Â$$ôE’"Â&öG’§Ð ¦fâ&Uö†ööµööâ‡F&vWC¢g7G"Â–æFVçC¢g7G"’Óâ7G&–ær°¢f÷&ÖB‚'¶–æFVçGÖöâ·F&vWGÒ&UÆç¶–æFVçGÒÆör†–æfòÂÂ'&V†ööµÂ"•Æç¶–æFVçGÖVæBöâ"§Ð ¢òòò†öö²w&—GFVâ–âF†RFW7F&Væ6†DT4Ä$D”ôâÂv†–6‚—2F†R÷6—F–öà¢òòòW6W"ÆæG2–âgFW"–×Æ66÷R&V¦V7G2æW7FVBF&vWBâ6ÖP¢òòòGvòÖ–çWB6†R2F†R6ö×öæVçBÖ&öG’&ÒæBF†R6ÖRfW&F–7C ¢òòòcG&÷2F†R†öö²æBÆ÷vW'2F†RG&–vvW"2Æ–â7–6ÆRG&–vvW"À¢òòò'—FRÖ–FVçF–6ÆÇ’f÷"&ööÂW‡&W76–öâæBVæ6ö×–Æ&Ç’f÷"¢òòòÖWF†öBF‚à¢5·FW7EÐ¦fâ÷FW7F&Væ6…÷66÷VEö†æFÆW%ö†ööµö—5öG&÷VEö'•÷c‚’°¢ÆWBæöæRÒ†ööµ÷÷6—F–öç2‚""Â""Â""“°¢ÆWBcöæöæRÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fæöæR’’æW‡V7B‚'cVÖ—G2"“° ¢òò–çWB¢7G&’&VöâvVçV–æR7–6ÆRG&–vvW"Âv–ç7B¢òòöæR×Fö¶Vâ6öçG&öÂF†B—2—G6VÆbæ6†÷&VBv–ç7Bæò†æFÆW"à¢ÆWB7FÂÒ†ööµ÷÷6—F–öç2€¢"öâGWBæVââÆâÆör†–æfòÂÂ'&V†ööµÂ"•ÆâVæBöâ"À¢""À¢""À¢“°¢ÆWBcö7FÂÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f7FÂ’’æW‡V7B‚'cVÖ—G2"“°¢76W'EöæR€¢cö7FÂÂcöæöæRÀ¢'F†R†æFÆW"×W7B6öçG&–'WFRÂ÷"F†R–FVçF—G’&VÆ÷r—2f7V÷W2 ¢“°¢ÆWB&ööÅö†öö²Ò†ööµ÷÷6—F–öç2‚g&Uö†ööµööâ‚&GWBæVââ"Â""’Â""Â""“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚f&ööÅö†öö²’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚'FW7F&Væ6‚×66÷VB"’Â'¶×6wÒ"“°¢76W'EöW€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f&ööÅö†öö²’’æW‡V7B‚'cVÖ—G2"’À¢cö7FÂÀ¢'cG&÷2F†R†öö²æBVÖ—G2F†R&&R7–6ÆRG&–vvW" ¢“° ¢òò–çWB#¢ÖWF†öBF‚â6ÖR&ÒÂ'WBcVFvRÖFWFV7G2öâÖVÖ&W ¢òòF†RVÖ—GFVB7G'V7BFöW2æ÷B†fRà¢ÆWBF…ö†öö²Ò†ööµ÷÷6—F–öç2‚g&Uö†ööµööâ‚'2ç6VæB"Â""’Â""Â""“°¢76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚gF…ö†öö²’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢ÆWBc÷F‚Ò7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gF…ö†öö²’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢c÷F‚æ6öçF–ç2‚"†&ööÂ’…÷F"ç2ç6VæB’"’À¢'cÆ÷vW'2F†RÖWF†öBF‚27–6ÆRG&–vvW" ¢“°¢76W'B€¢c÷F‚æ6öçF–ç2‚%6VæFW%÷6VæE÷&RçW6…ö&6²"’À¢&æB&Vv—7FW'2æò†öö²Â6òF†R÷&FW&–ær—2Æ÷7BV—F†W"v’ ¢“°§Ð ¢òòò†öö²–â5DDTÔTåB÷6—F–öâÂ'’6öçG&7BÂc&VÆÇ’FöW2–×ÆVÖVçC ¢òòò—BVÖ—G2F†R6ÖR6VæFW%÷6VæE÷&RçW6…ö&6¶&Vv—7G&F–öâF†P¢òòòv÷&¶–ærFW7B×66÷RÆ6VÖVçBvWG2â6òF†—2öæR¶VW2Vç7W÷'FVF(	@¢òòòæB—G27VvvW7F–öâ×W7BæÖRFW7F–æF–öâF†Bv÷&·2Âv†–6‚F†RGvð¢òòòö'f–÷W2&VF–æw2öb'F†R6ö×öæVçB÷"FW7F&Væ6‚"Fòæ÷Bà¢5·FW7EÐ¦fâ÷7FFVÖVçE÷÷6—F–öåö†ööµö¶VW5ö—G5÷c÷7VvvW7F–öåöæEö÷v÷&¶–æuöFW7F–æF–öâ‚’°¢ÆWB7F×BÒ†ööµ÷÷6—F–öç2‚""Â""Âg&Uö†ööµööâ‚'2ç6VæB"Â""’“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚g7F×B’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚'7FFVÖVçB÷6—F–öâ"’Â'¶×6wÒ"“° ¢òòc—2&VÂW66R†F6ƒ¢—BVÖ—G2F†R6ÖR&Vv—7G&F–öâF†P¢òòFW7B×66÷RÆ6VÖVçBvWG2Âv†–6‚F&—"—G6VÆbÆ÷vW'2âæ÷@¢òò'—FRÖ–FVçF–6Â(	B7FFVÖVçB×÷6—F–öâ†öö²&Vv—7FW'2B—G2÷và¢òòö–çB–âF†R'Vâ&öG’&F†W"F†â†VBöbF†R6WGW76–væÖVçG2À¢òòv†–6‚—2F†Rv†öÆR&V6öâF†RGvòÆ6VÖVçG2&RF—7F–æwV—6†&ÆR(	@¢òò6òF†R6Æ–Ò—2&÷WBF†R&Vv—7G&F–öâÂæ÷BF†Rf–ÆRà¢ÆWBFW7E÷66÷RÒ†ööµ÷÷6—F–öç2‚""Âg&Uö†ööµööâ‚'2ç6VæB"Â""’Â""“°¢Æ÷vW%÷7&2‚gFW7E÷66÷R’æW‡V7B‚'F†RFW7B×66÷RÆ6VÖVçBÆ÷vW'2VæFW"F&—""“°¢ÆWBcöæöæRÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f†ööµ÷÷6—F–öç2‚""Â""Â""’’’æW‡V7B‚'cVÖ—G2"“°¢ÆWBc÷7F×BÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7F×B’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢cöæöæRæ6öçF–ç2‚%6VæFW%÷6VæE÷&RçW6…ö&6²"’À¢'F†Ræ6†÷#¢æò†öö²Âæò&Vv—7G&F–öâ ¢“°¢f÷"†Æ&VÂÂ÷WB’–â°¢‚'7FFVÖVçB÷6—F–öâ"Âgc÷7F×B’À¢€¢'FW7B66÷R"À¢f7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gFW7E÷66÷R’’æW‡V7B‚'cVÖ—G2"’À¢’À¢Ò°¢76W'B€¢÷WBæ6öçF–ç2‚%6VæFW%÷6VæE÷&RçW6…ö&6²"’À¢'c&Vv—7FW'2F†R¶Æ&VÇÒ†öö²Â6òÒÖ6öFVvVâc—2†öæW7B ¢“°¢Ð ¢òòF†RFW7F–æF–öâF†RÖW76vRæÖW2×W7B&RF†RöæRF†Bv÷&·2â—@¢òòW6VBFò6’'F†R6ö×öæVçB÷"FW7F&Væ6‚#²†öö²–â6ö×öæVç@¢òò&öG’æB†öö²–âFW7F&Væ6†FV6Æ&F–öâ$õD‚f–Âà¢76W'B€¢×6ræ6öçF–ç2‚'FW7B66÷R"’bb×6ræ6öçF–ç2‚'G&ç67F÷""’À¢'F†R7VvvW7F–öâ×W7BæÖRF†RÆ6VÖVçBF†B7GVÆÇ’Æ÷vW'3¢¶×6wÒ ¢“°¢òòV6‚&V¦V7FVBÆ6VÖVçB—26†V6¶VBv–ç7B•E2õtâ†öö²Ög&VP¢òò6öçG&öÂâ76W'F–æröæÇ’—5öW'"‚–v÷VÆB72öâ6÷W&6RF†@¢òòf–Ç2f÷"âVç&VÆFVB&V6öâÂv†–6‚—2†÷rÆ6VÖVçBvWG0¢òò&V6öÖÖVæFVB'’66–FVçBà¢ÆWB6ö×öæVçEö&öG’ÒÆ†öö³¢g7G'Â°¢†ööµ÷÷6—F–öç2‚""Â""Â""’ç&WÆ6R€¢"†öö¶&ÆRæ÷FR‡C¢&Vt÷’"À¢ff÷&ÖB‚'¶†öö·Ò†öö¶&ÆRæ÷FR‡C¢&Vt÷’"’À¢¢Ó°¢f÷"†Æ&VÂÂ&BÂ6öçG&öÂ’–â°¢€¢'FW7F&Væ6‚FV6Æ&F–öâ"À¢†ööµ÷÷6—F–öç2‚g&Uö†ööµööâ‚'2ç6VæB"Â""’Â""Â""’À¢†ööµ÷÷6—F–öç2‚""Â""Â""’À¢’À¢€¢&6ö×öæVçB&öG’"À¢6ö×öæVçEö&öG’‚"öâ2ç6VæB&UÆâÆör†–æfòÂÂ'Â"•ÆâVæBöåÆâ"’À¢6ö×öæVçEö&öG’‚""’À¢’À¢Ò°¢Æ÷vW%÷7&2‚f6öçG&öÂ’çVçw&ö÷%öVÇ6R‡ÆWÂ°¢æ–2‚'F†R¶Æ&VÇÒ6öçG&öÂ×W7BÆ÷vW"Â÷"—G2&V¦V7F–öâ&÷fW2æ÷F†–æs¢¶S£÷Ò"¢Ò“°¢ÆWBW'"ÒÆ÷vW%÷7&2‚f&B¢æW'"‚¢çVçw&ö÷%öVÇ6R‡ÇÂæ–2‚'F†R¶Æ&VÇÒÆ6VÖVçB×W7Bæ÷B&Rv÷&¶–ærFW7F–æF–öâ"’“°¢76W'B€¢W'"çFõ÷7G&–ær‚’æ6öçF–ç2‚&†öö²"’À¢&æB—B×W7B&RF†R„ôô²F†B—2&V¦V7FVBF†W&RÂæ÷B6öÖWF†–ærVÇ6S¢¶W'#£÷Ò ¢“°¢Ð§Ð ¢òòòF†R7FFVÖVçB×÷6—F–öâ&Ò—2Ö—†VBÂ&V6W6Rcw2ÖWF†öBÖ†öö°¢òòò&W6öÇfW"—2v†BWfW'’†öö¶VBöævöW2F‡&÷Vv‚âF‚G&–vvW"—@¢òòòv—&W3²ç—F†–ærVÇ6R—B&VgW6W2÷WG&–v‡BÂ6òö–çF–ærF†÷6R@¢òòòÒÖ6öFVvVâcv÷VÆB†æBF†RW6W"6V6öæBW'&÷"à¢òòð¢òòòF†RÆ7Bf÷W"G&–vvW'2&Rv‡’F†R&VF–6FR—2†æB×w&—GFVâvÆ°¢òòò&F†W"F†â6ö×öæVçG3£¦F÷GFVE÷F†Âv†–6‚ÆV¶VBGv–6S¢—B&WGW&ç0¢òòò6öÖVf÷"&&R–FVçF–f–W"ÂæB—BVçw&2&VæBUdU%’ÆWfVÂÀ¢òòò6òwV&F–ærF†RF÷ÖÆWfVÂæöFRöæÇ’Ö÷fVBF†RÆV²öæR6VvÖVç@¢òòò–çv&Bâöâ‡2ç6VæB’&VæBöâ‡2’ç6VæB&V&RV6‚öæP¢òòò6†&7FW"g&öÒ&öw&ÒF†Bv÷&·2ÂæBc&VgW6W2&÷F‚à¢5·FW7EÐ¦fâöæöå÷F…ö†ööµ÷G&–vvW%ö–å÷7FFVÖVçE÷÷6—F–öåö—5÷&VgW6VEö'•÷c÷Föò‚’°¢f÷"G&–vvW"–â°¢&GWBæVââ"À¢#R7–6ÆW2"À¢'ræWb‡‚’"À¢&ö²"À¢"‡2ç6VæB’"À¢"‡2’ç6VæB"À¢"†Ræ–ææW"’ææ÷FR"À¢òò&&R–FVçF–f–W"F†B•2FW7F&Væ6‚f–VÆBâF†R–×ÂÖf÷ ¢òòFW7Vv&W"&Ww&—FW2—BFò÷F"ç6Â6òÆ–âÆVæwF‚FW7@¢òò6÷VçG2F†R7–çF†WF–2&ö÷BæB&VG2—B2Æö&£âãÆÖWF†öCæà¢'2"À¢Ò°¢ÆWB7&2Ò†ööµ÷÷6—F–öç2‚""Â""Âg&Uö†ööµööâ‡G&–vvW"Â""’“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’ÂÆ÷vW#£¥c7FGW3£¥&V¦V7G2“°¢76W'B†×6ræ6öçF–ç2‚&æöâÖÖWF†öB×F‚"’Â'¶×6wÒ"“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’æ—5öW'"‚’À¢'c×W7B&VgW6Röâ·G&–vvW'Ò&VÂ÷"&V¦V7G6—2w&öær ¢“°¢òòF†Ræ6†÷#¢F†R4ÔRG&–vvW"v—F†÷WB†öö²6–FR—2f–æRÂ6ð¢òòF†R&V¦V7F–öâ—2&÷WBF†R†öö²æBæ÷B&÷WBF†RG&–vvW"à¢òòæ6†÷&VBv†W&RF†R†öö²Ög&VRf÷&ÒÆ÷vW'2Âv†–6‚—2v†@¢òòÖ¶W2F†R&V¦V7F–öâ&÷fR&÷WBF†R„ôô²&F†W"F†â&÷W@¢òòF†RG&–vvW"â6¶—VBf÷"'2&†&&Rf–VÆB&VfW&Væ6R’æ@¢òòf÷"F†R&VçF†W6—6VBæB6ÆÂf÷&×2ÂæöæRöbv†–6‚Æ÷vW"öà¢òòF†V—"÷vâ(	Bâæ6†÷"F†W&Rv÷VÆBÖV7W&Ræ÷F†–ærâF†@¢òòÆVfW2GWBæVââÂR7–6ÆW6æBö¶6''––ær—Bà¢–bG&–vvW"Ò'2"bbG&–vvW"æ6öçF–ç2‚r‚r’°¢ÆWB7FÂÒ†ööµ÷÷6—F–öç2€¢""À¢""À¢ff÷&ÖB‚"öâ·G&–vvW'ÕÆâÆör†–æfòÂÂ'Â"•ÆâVæBöâ"’À¢“°¢Æ÷vW%÷7&2‚f7FÂ’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚&öâ·G&–vvW'ÖÆöæR×W7BÆ÷vW#¢¶S£÷Ò"’“°¢Ð¢Ð§Ð ¢òòò†6R÷7EöWfÆÖöF–f–W"GW&ç2v—&VB†öö²–çFò&VgW6VBöæRÀ¢òòòBWfW'’÷6—F–öâF†B6'&–W2†öö²â—B—2F†RöæR†—2F†BfÆ—0¢òòòF†RfW&F–7Bv—F†÷WBF÷V6†–ærF†RG&–vvW"Â6ò—B—26ÆÆVB÷WBv—F€¢òòò—G2÷vâÖW76vR&F†W"F†â&ÆÖVBöâF†RF‚6†Rà¢5·FW7EÐ¦fâ÷†6UöÖöF–f–W%ööåööÖWF†öEö†ööµö—5÷&VgW6VEö'•÷c‚’°¢ÆWB†6Uö†öö²ÒÆ–æC¢g7G'Â°¢f÷&ÖB‚'¶–æGÖöâ2ç6VæB&R†6R÷7EöWfÅÆç¶–æGÒÆör†–æfòÂÂ'Â"•Æç¶–æGÖVæBöâ"¢Ó°¢f÷"†Æ&VÂÂ7&2’–â°¢€¢'7FFVÖVçB÷6—F–öâ"À¢†ööµ÷÷6—F–öç2‚""Â""Âg†6Uö†öö²‚""’’À¢’À¢‚'FW7B66÷R"Â†ööµ÷÷6—F–öç2‚""Âg†6Uö†öö²‚""’Â""’’À¢Ò°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’ÂÆ÷vW#£¥c7FGW3£¥&V¦V7G2“°¢76W'B†×6ræ6öçF–ç2‚'†6R÷7EöWfÂ"’Â'¶Æ&VÇÓ¢¶×6wÒ"“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’æ—5öW'"‚’À¢'c×W7B&VgW6RF†R†6RÖöF–f–W"BF†R¶Æ&VÇÒÂ÷"&V¦V7G6—2w&öær ¢“°¢Ð¢òòF†Ræ6†÷#¢F†R6ÖR†öö²t•D„õUBF†RÖöF–f–W"—2v—&VB'’c@¢òò&÷F‚÷6—F–öç2Â6ò—B—2F†R†6RF†B—2&V–ær&VgW6VBà¢f÷"7&2–â°¢†ööµ÷÷6—F–öç2‚""Â""Âg&Uö†ööµööâ‚'2ç6VæB"Â""’’À¢†ööµ÷÷6—F–öç2‚""Âg&Uö†ööµööâ‚'2ç6VæB"Â""’Â""’À¢Ò°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’¢æW‡V7B‚'cVÖ—G2"¢æ6öçF–ç2‚%6VæFW%÷6VæE÷&RçW6…ö&6²"’À¢'F†R†öö²Ög&VRÖöb×†6R6öçG&öÂ×W7B&Rv—&VB ¢“°¢Ð§Ð ¢òòòF†R6ÖR7Æ—BBFW7B66÷RÂv†W&RF†RF&vWB&W6öÇfW"F¶W2öæÇ¢òòòÆf–VÆCâãÆÖWF†öCæâDTUU"F‚—27V'6WBv(	BcvÆ·2F†P¢òòòv†öÆRF‚æBv—&W2—B(	Bv†–ÆRæöâ×F‚G&–vvW"—2&VgW6VB'’cà¢5·FW7EÐ¦fâöæW7FVEö†ööµ÷F…ö—5öövö'WEööæöå÷F…÷G&–vvW%ö—5÷&VgW6VB‚’°¢ÆWBæW7FVBÒ†ööµ÷÷6—F–öç2‚""Âg&Uö†ööµööâ‚&Ræ–ææW"ææ÷FR"Â""’Â""“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚fæW7FVB’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&æW7FVB6ö×öæVçBF‚"’Â'¶×6wÒ"“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fæW7FVB’¢æW‡V7B‚'cVÖ—G2"¢æ6öçF–ç2‚%vF6†W%öæ÷FU÷&RçW6…ö&6²"’À¢'cvÆ·2F†RæW7FVBF‚Â6òF†RÒÖ6öFVvVâc7VvvW7F–öâ—2†öæW7B ¢“° ¢f÷"G&–vvW"–â°¢&GWBæVââ"À¢#R7–6ÆW2"À¢'ræWb‡‚’"À¢&ö²"À¢"‡2ç6VæB’"À¢"‡2’ç6VæB"À¢"†Ræ–ææW"’ææ÷FR"À¢òòæò'2&†W&S¢BFW7B66÷RF†RFW7Vv&W"&Ww&—FW2—BFð¢òò÷F"ç6Âv†–6‚&W6öÇfUöÖWF†öEö†ööµ÷F&vWF44UE20¢òòÆf–VÆCâãÆÖWF†öCæÂ6ò—BÆæG2–âF†RF&vWBÖÆöö·W&Òæ@¢òò—2ç7vW&VB–çfÆ–F(	B6÷'&V7FÇ’Â6–æ6Rc&VgW6W2—BFöòà¢òòöæÇ’F†R7FFVÖVçB÷6—F–öâ&V6†W2F†—27Æ—Bv—F‚—Bà¢Ò°¢ÆWB7&2Ò†ööµ÷÷6—F–öç2‚""Âg&Uö†ööµööâ‡G&–vvW"Â""’Â""“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’ÂÆ÷vW#£¥c7FGW3£¥&V¦V7G2“°¢76W'B†×6ræ6öçF–ç2‚&æöâÖÖWF†öB×F‚"’Â'¶×6wÒ"“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’æ—5öW'"‚’À¢'c×W7B&VgW6Röâ·G&–vvW'Ò&VBFW7B66÷RÂ÷"&V¦V7G6—2w&öær ¢“°¢Ð§Ð ¢òòòF†R†öö²×F&vWB&W6öÇfW"W6VBFòç7vW"WfW'’æöâ×G&ç67F÷"f–VÆ@¢òòòv—F‚–çfÆ–F(	B'–÷W"&öw&Ò—2'&ö¶VâVæFW"WfW'’&6¶VæB"âF†@¢òòò—2fÇ6Rf÷"âvVçBòVçbòÖWF†öBÖ&V&–ær66÷&V&ö&Bf–VÆBÂv†–6€¢òòòcv—&W2–çFòv÷&¶–ærÅG—SåóÆÖWF†öCå÷&VfV7F÷"âF†R6—FRæ÷p¢òòò7Æ—G2öâcw2÷vâ6öæF—F–öâÂæBöæÇ’F†R†ÆbcÇ6ò&VgW6W0¢òòò7F—2–çfÆ–Fà¢5·FW7EÐ¦fâö†ööµööåööæöå÷G&ç67F÷%öf–VÆEö—5ö÷7V'6WEövöæ÷Eö÷&öw&ÕöW'&÷"‚’°¢ÆWB†öö²ÒÇF&vWC¢g7G'Â†ööµ÷÷6—F–öç2‚""Âg&Uö†ööµööâ‡F&vWBÂ""’Â""“° ¢òòcv—&W2F†W6RÂ6òF†W’&R7V'6WBv2æBÒÖ6öFVvVâc—2†öæW7Bà¢f÷"‡F&vWBÂfV7F÷"’–â°¢‚'rææ÷FR"Â%vF6†W%öæ÷FU÷&RçW6…ö&6²"’À¢‚'2ç6VæB"Â%6VæFW%÷6VæE÷&RçW6…ö&6²"’À¢Ò°¢ÆWB7&2Ò†öö²‡F&vWB“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’æW‡V7B‚'cVÖ—G2"“°¢76W'B‡cæ6öçF–ç2‡fV7F÷"’Â'c×W7B&Vv—7FW"·F&vWGÖ"“°¢Ð¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚f†öö²‚'rææ÷FR"’’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&æöâ×G&ç67F÷"6ö×öæVçBf–VÆB"’Â'¶×6wÒ"“°¢òòF†RG&ç67F÷"66R—2F†R6öçG&öÃ¢—B—2æ÷BvBÆÂà¢Æ÷vW%÷7&2‚f†öö²‚'2ç6VæB"’’æW‡V7B‚&G&ç67F÷"f–VÆBÆ÷vW'2"“° ¢òò&&Rf–VÆBv—F‚æòÖWF†öC¢F†RFW7Vv&W"&Ww&—FW2—BFð¢òò÷F"ç6Â6òF†R&W6öÇfW"66WG2F†R4„RæBF†RÆöö·WF†Và¢òòf–Ç2öâF†R7–çF†WF–2&ö÷Bâc&VgW6W2—BÂ6ò–çfÆ–F—0¢òò&–v‡B(	B'WBF†RÖW76vR×W7Bæ÷BV÷FR&6²÷F&F†RW6W ¢òòæWfW"G—VBà¢ÆWB&&RÒ†ööµ÷÷6—F–öç2‚""Âg&Uö†ööµööâ‚'2"Â""’Â""“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f&&R’’æ—5öW'"‚’À¢'c×W7B&VgW6R†öö²v—F‚æòÖWF†öB ¢“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚f&&R’çVçw&öW'"‚’“°¢òò—B×W7BÆVBv—F‚v†BF†RW6W"u$õDRÂæ÷Bv—F‚F†RFW7Vv&V@¢òòf÷&Òâ÷F&Ö’7F–ÆÂV"ÆFW"Â–âF†R&VçF†WF–6ÂF†@¢òò6÷fW'2Æ—FW&Â6÷W&6R÷F&(	BF†RGvò&R–æF—7F–æwV—6†&ÆR'¢òòF†RF–ÖRF†RÆöö·Wf–Ç2Â6òF†RÖW76vR6÷fW'2&÷F‚&F†W ¢òòF†â76W'F–ærv†–6‚†VæVBà¢76W'B€¢×6rç7F'G5÷v—F‚‚&öâ6†öö³¢"’À¢&×W7BV÷FRF†R6÷W&6RFW‡BÂæ÷B÷F"ç6¢¶×6wÒ ¢“°¢76W'B†×6ræ6öçF–ç2‚&æÖW2ÖWF†öBFòw&"’Â'¶×6wÒ"“° ¢òòc&VgW6W2F†W6R—G6VÆbÂ6òF†W’&VÆÇ’&R&öw&ÒW'&÷'2à¢f÷"F&vWB–â²'rçÆ–â"Â&æ÷7V6‚ç6VæB"Â&GWBç6VæB"Â'2ææ÷7V6‚%Ò°¢ÆWB7&2Ò†öö²‡F&vWB“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’æ—5öW'"‚’À¢'c×W7B&VgW6R·F&vWGÖÂ÷"–çfÆ–F÷fW"Ö6Æ–×2 ¢“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’“°¢òòæÖRF†Rv†öÆRF&vWBÂæ÷B§W7B—G2†VB(	B2ææ÷7V6†æ@¢òòrçÆ–æv÷VÆB÷F†W'v—6RÖF6‚öâöæRÖ6†&7FW"æVVFÆRà¢76W'B†×6ræ6öçF–ç2‚ff÷&ÖB‚&öâ·F&vWGÖ"’’Â'¶×6wÒ"“°¢Ð§Ð ¢òòòF†Ræ–çF‚6—FRÂ–â66÷&V&ö&G2ç'6Âv†–6‚ç7vW'2WfW'¢òòò6öææV7Fööæ–â66÷&V&ö&B&öG’v—F‚öæRfW&F–7BâF†R„ôô´T@¢òòò†Æböb—B—2Væ–f÷&Ó¢cG&÷2F†R†öö²Â'—FRÖ–FVçF–6ÆÇ’FòF†P¢òòò6ÖR†æFÆW"v—F†÷WBöæRÂB&÷F‚G&–vvW"6†W2à¢òòð¢òòòF†RVæ†öö¶VB†Æb—2FVÆ–&W&FVÇ’ÆVgBv†öÆRâ—B—2Ö—†VBÂ'WBv†@¢òòò6W&FW2—G2–çWG2—2F†R4ôåD”äU"F†R66÷&V&ö&B—2–ç7FçF–FV@¢òòò–â(	Bæ÷B¶æ÷v&ÆRv†W&RFV6Æ&F–öâ—2Æ÷vW&VB(	BæBÂv—F†–âöæP¢òòò6öçF–æW"ÂæÖR&W6öÇWF–öâ–âF†RVÖ—GFVB2²²&F†W"F†â7–çFƒ ¢òòòöâGWBæVæ6ö×–ÆW2æBöârç6VVââFöW2æ÷BÂFW7—FR&V–ær¢òòòF‚æBâW‡&W76–öâ&W7V7F—fVÇ’â7–çF7F–27Æ—Bv2w&—GFVà¢òòòæB&WfW'FVC²6VRF†R6öÖÖVçBBF†R6—FRæBF†R6–&Æ–ærFW7Bà¢5·FW7EÐ¦fâö†öö¶VE÷66÷&V&ö&Eö†æFÆW%ö—5öG&÷VEö'•÷c‚’°¢ÆWB66÷&V&ö&BÒÆ&öG“¢g7G'Â°¢„ôôµõõ4•D”ôå5õ5$0¢ç&WÆ6R‚$„ôôµÆâ"Â""¢ç&WÆ6R‚$”ÕÅÆâ"Â""¢ç&WÆ6R‚$$ôE•Æâ"Â""¢ç&WÆ6R€¢&VæBvVçBvF6†W""À¢ff÷&ÖB€¢&VæBvVçBvF6†W%ÆåÆç66÷&V&ö&B&ö&EÆâ†—G2¢V–çCÃ3#âFVfVÇBÆåÀ¢¶&öG—ÖVæB66÷&V&ö&B&ö&B ¢’À¢¢ç&WÆ6R‚"R¢†öÆFW""Â"R¢†öÆFW%Æâ"¢&ö&B"¢Ó°¢ÆWB†æFÆW"ÒÇG&–vvW#¢g7G"Â†öö³¢g7G'Â°¢f÷&ÖB‚"öâ·G&–vvW'×¶†öö·ÕÆâÆör†–æfòÂÂ'Â"•ÆâVæBöåÆâ"¢Ó° ¢òòF†Ræ6†÷"f÷"WfW'’'—FRÖ–FVçF—G’&VÆ÷s¢F†R66÷&V&ö&Bv—F‚æð¢òò†æFÆW"BÆÂF–ffW'2g&öÒV6‚6öçG&öÂà¢ÆWBcöæöæRÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g66÷&V&ö&B‚""’’’æW‡V7B‚'cVÖ—G2"“° ¢f÷"G&–vvW"–â²&†—G2â"Â'rææ÷FR%Ò°¢ÆWB7FÂÒ66÷&V&ö&B‚f†æFÆW"‡G&–vvW"Â""’“°¢ÆWBcö7FÂÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f7FÂ’’æW‡V7B‚'cVÖ—G2"“°¢76W'EöæR€¢cö7FÂÂcöæöæRÀ¢&öâ·G&–vvW'Ö×W7B6öçG&–'WFRÂ÷"F†R–FVçF—G’&VÆ÷r—2f7V÷W2 ¢“°¢òòF†R6öçG&öÂ—G6VÆb—27F–ÆÂ7V'6WBvÂv†–6‚—2v†BÖ¶W0¢òòF†R†öö²F†RöæÇ’F†–ærVæFW"FW7B†W&Râ—B6'&–W2F†P¢òòVæ†öö¶VB&Òw2fW&F–7BÂÖV7W&VB–âF†R6–&Æ–ærFW7B&VÆ÷rà¢76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚f7FÂ’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“° ¢ÆWB†öö¶VBÒ66÷&V&ö&B‚f†æFÆW"‡G&–vvW"Â"&R"’“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚f†öö¶VB’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢òòæVVFÆRF†R„ôô²Âæ÷BF†R6†&VBöâ66÷&V&ö&BÆ&ö&EÆ ¢òò7Vff—ƒ¢6–æ6RF†RVæ†öö¶VB&Ò&VÆ÷r6'&–W2F†R6ÖR7FGW0¢òòæBF†R6ÖR7Vff—‚ÂÖF6†–ærF†R7Vff—‚v÷VÆBÆWBF†—2FW7@¢òò7F’w&VVâv—F‚F†R†öö¶VB&ÒFVÆWFVB(	BF†R–çWBv÷VÆ@¢òò6–×Ç’fÆÂF‡&÷Vv‚FòF†RVæ†öö¶VB&Òà¢76W'B€¢×6ræ6öçF–ç2‚&†öö²öââöæ†æFÆW"öâ66÷&V&ö&B&ö&F"’À¢'¶×6wÒ ¢“°¢76W'EöW€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f†öö¶VB’’æW‡V7B‚'cVÖ—G2"’À¢cö7FÂÀ¢'cG&÷2F†R†öö²öâöâ·G&–vvW'Ò&V ¢“°¢Ð§Ð ¢òòòF†RVæ†öö¶VB6öææV7Fööæ&Ò–â66÷&V&ö&B&öG’ç7vW'2ôäP¢òòòfW&F–7Bf÷"—G2v†öÆR–çWB76RÂæBF†BfW&F–7B—2æ÷rÖV7W&V@¢òòò&F†W"F†â&÷f—6–öæÂà¢òòð¢òòò7–çF7F–27Æ—Bv2w&—GFVâ†W&RæBVæFöæRâF†—2FW7B–ç2v‡“ ¢òòò—B76W'G2F†RGvò&÷w2F†R7Æ—Bv÷B$4µt$E2(	BöâGWBæVæ—2¢òòòGvò×6VvÖVçBD‚F†Bc6ö×–ÆW2ÂæBöârç6VVââ—2à¢òòòU…$U54”ôâF†BcFöW2æ÷Bâæò&VF–6FR÷fW"G&–vvW"7–çF‚6à¢òòò6W&FRF†—2&Òà¢òòð¢òòòv†BDôU26W&FRF†R66W2—2F†R4ôåD”äU"ÂæBF†B—2æ÷@¢òòò¶æ÷v&ÆR†W&S¢Æ÷vW%÷66÷&V&ö&FÆ÷vW'2FV6Æ&F–öâÂæBF†R6ÖP¢òòò66÷&V&ö&BG—R6â&R–ç7FçF–FVB2G&ç67F÷"f–VÆB÷"¢òòòFW7F&Væ6‚f–VÆBâÖV7W&VB7&÷72&÷F‚†F—fW&vVæ6Rs“ ¢òòð¢òòò¢G&ç67F÷"f–VÆB(	BcVÖ—G2÷WGWB'—FRÖ–FVçF–6ÂFòF†R6ÖP¢òòò&öw&Òv—F‚F†Rv—&–ærDTÄUDTBÂ6ò—B6–ÆVçFÇ’G&÷2—C°¢òòò¢FW7F&Væ6‚f–VÆB(	B6öææV7FVÖ—G2âVæ6ö×–Æ&ÆRçW6…ö&6¶ ¢òòòöâ66Æ"†V–çCcE÷F¢7÷V–çEöf÷%÷v–GF†v–FVç2WfW'¢òòò66Æ"(šBcB&—G2Âv†FWfW"F†R6÷W&6RFV6Æ&W2’Âv†–ÆRöæ ¢òòòVÖ—G2v÷&¶–ær6†V6¶W"à¢òòð¢òòòâ&Òw27FGW2—2F†Rv÷'7BF†–ærcFöW2ç—v†W&RVæFW"—BÂæB¢òòò6–ÆVçBG&÷—2F†Rv÷'7BöbF†RF‡&VRâ†Væ6RöæP¢òòò6–ÆVçFÇ”Ö—4Æ÷vW'6Âæ÷BöæRVç7W÷'FVFà¢òòð¢òòòF†—2FW7B6÷fW'2F†RDU5D$Tä4‚6öçF–æW"öæÇ’(	B&ö&FÆæG22¢òòòf–VÆBöbFW7F&Væ6‚†ööµF&âF†RG&ç67F÷"6öçF–æW"Âv†–6‚—0¢òòòv†BÖ¶W2F†R7FGW26–ÆVçFÇ”Ö—4Æ÷vW'6BÆÂÂ—2–ææV@¢òòò6W&FVÇ’&VÆ÷rà¢5·FW7EÐ¦fâå÷Væ†öö¶VE÷66÷&V&ö&Eö†æFÆW%ö—5ööæU÷fW&F–7Eöf÷%ö—G5÷v†öÆUö–çWE÷76R‚’°¢ÆWB66÷&V&ö&BÒÆ&öG“¢g7G'Â°¢„ôôµõõ4•D”ôå5õ5$0¢ç&WÆ6R‚$„ôôµÆâ"Â""¢ç&WÆ6R‚$”ÕÅÆâ"Â""¢ç&WÆ6R‚$$ôE•Æâ"Â""¢ç&WÆ6R€¢&VæBvVçBvF6†W""À¢ff÷&ÖB€¢&VæBvVçBvF6†W%ÆåÆç66÷&V&ö&B&ö&EÆâ†—G2¢V–çCÃ3#âFVfVÇBÆåÀ¢¶&öG—ÖVæB66÷&V&ö&B&ö&B ¢’À¢¢ç&WÆ6R‚"R¢†öÆFW""Â"R¢†öÆFW%Æâ"¢&ö&B"¢Ó°¢ÆWB†æFÆW"Ð¢ÇG&–vvW#¢g7G'Âf÷&ÖB‚"öâ·G&–vvW'ÕÆâÆör†–æfòÂÂ'Â"•ÆâVæBöåÆâ"“° ¢f÷"G&–vvW"–â°¢&†—G2â"À¢'rç6VVââ"À¢&GWBæVâ"À¢'rææ÷FR"À¢"‡rææ÷FR’"À¢'rææ÷FR7–6ÆW2"À¢'rææ÷FR†6R÷7EöWfÂ"À¢Ò°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g66÷&V&ö&B‚f†æFÆW"‡G&–vvW"’’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B€¢×6ræ6öçF–ç2‚&WfVçBv—&–ær"’À¢&öâ·G&–vvW'Ö×W7B7F–ÆÂ&V6‚F†RöæRvVæW&–2&Ó¢¶×6wÒ ¢“°¢Ð ¢òòæBF†R&V6öâ7–çF7F–27Æ—B6ææ÷Bv÷&³¢F†RF‚6ö×–ÆW0¢òòæBF†RW‡&W76–öâFöW2æ÷BÂv†–6‚—2F†R÷÷6—FRöbv†@¢òòG&–vvW"6†Rv÷VÆB&VF–7Bà¢ÆWBF‚Ò7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g66÷&V&ö&B‚f†æFÆW"‚&GWBæVâ"’’’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢F‚æ6öçF–ç2‚"†&ööÂ’††&5÷'C£¦†&5÷&VB†GWBÓæVâ’’"’À¢&Gvò×6VvÖVçBF‚F†B&W6öÇfW2v–ç7BF†REUBæB6ö×–ÆW2 ¢“°¢ÆWBW‡"Ò7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g66÷&V&ö&B‚f†æFÆW"‚'rç6VVââ"’’’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢W‡"æ6öçF–ç2‚"†&ööÂ’‡rç6VVââ’"’À¢&âW‡&W76–öâæÖ–ær4”$Ä”ärFW7F&Væ6‚f–VÆBÂVçVÆ–f–VB ¢“°¢76W'B€¢W‡"æ6öçF–ç2‚%÷F"çrç6VVâ"’À¢'cFöW2æ÷BVÆ–g’—BÂ6òF†W&R—2æòv–âF†R6†V6¶W"ÆÖ&Fw266÷R ¢“°§Ð ¢òòòF†RE$å45Dõ"6öçF–æW"(	BF†R&÷rF†BÆöæR§W7F–f–W0¢òòò6–ÆVçFÇ”Ö—4Æ÷vW'6ÂæBF†RöæRF†RGvòFW7G2&÷fRæWfW"&V6‚À¢òòò&V6W6R&÷F‚–ç7FçF–FR&ö&F2f–VÆBöbFW7F&Væ6‚†ööµF&à¢òòð¢òòò†VÆB–ç7FVB2f–VÆBöbG&ç67F÷"6VæFW&Âcw2÷WGWBf÷"¢òòò66÷&V&ö&B6''––æröæv—&–ærÂ÷"6öææV7Fv—&–ærÂ—2%•DRÐ¢òòò”DTåD”4ÂFòF†R6ÖR&öw&Òv†÷6R66÷&V&ö&B&öG’—2V×G’âF†P¢òòòv—&–ær6öçG&–'WFW2æ÷F†–æs¢F†R66÷&V&ö&Bö'6W'fW2æòG&ff–2Âæ@¢òòò6†V6²F†B6†÷VÆB6F6‚Ö—6ÖF6‚76W2w&VVâà¢òòð¢òòòF†—2—2F†Rv†öÆR&6—2f÷"F†R&Òw27FGW2Â6ò—B—2–ææV@¢òòòF—&V7FÇ’&F†W"F†â–æfW'&VBg&öÒF†RFW7F&Væ6‚&÷w2(	BæB—@¢òòò6÷fW'26öææV7FÂv†–6‚æV—F†W"6–&Æ–ærW†W&6—6W2BÆÂWfVâF†÷Vv€¢òòòF†R&Òw2W6W"Öf6–ærFWF–ÂÖ¶W26Æ–Ò&÷WB—Bà¢5·FW7EÐ¦fâ÷G&ç67F÷%ö†VÆE÷66÷&V&ö&Eö†5ö—G5÷v—&–æuöG&÷VEö'•÷c‚’°¢ÆWB66÷&V&ö&BÒÆ&öG“¢g7G'Â°¢„ôôµõõ4•D”ôå5õ5$0¢ç&WÆ6R‚$„ôôµÆâ"Â""¢ç&WÆ6R‚$”ÕÅÆâ"Â""¢ç&WÆ6R‚$$ôE•Æâ"Â""¢ç&WÆ6R€¢&VæBvVçBvF6†W""À¢ff÷&ÖB€¢&VæBvVçBvF6†W%ÆåÆç66÷&V&ö&B&ö&EÆâ†—G2¢V–çCÃ3#âFVfVÇBÆåÀ¢¶&öG—ÖVæB66÷&V&ö&B&ö&B ¢’À¢¢òòF†R6öçF–æW"VæFW"FW7C¢E$å45Dõ"f–VÆBÂæ÷BF†P¢òòFW7F&Væ6‚f–VÆBF†R6–&Æ–ærFW7G2W6Rà¢ç&WÆ6R€¢'G&ç67F÷"6VæFW%ÆâGWB¢F÷Æâ"À¢'G&ç67F÷"6VæFW%ÆâGWB¢F÷Æâ"¢&ö&EÆâ"À¢¢Ó° ¢òòçF’×f7V—G“¢F†R66÷&V&ö&B—2&VÆÇ’ÖFW&–Æ—¦VB–ç6–FRF†P¢òòG&ç67F÷"Â6ò&–FVçF–6Â"&VÆ÷r—2æ÷BGvò6÷–W2öb&öw&Ð¢òòF†BG&÷VBF†Rv†öÆRf–VÆBà¢ÆWBV×G’Ò7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g66÷&V&ö&B‚""’’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢V×G’æ6öçF–ç2‚'7G'V7B&ö&B²"’bbV×G’æ6öçF–ç2‚"&ö&B#²"’À¢'F†RG&ç67F÷"×W7B7GVÆÇ’†öÆBF†R66÷&V&ö&B ¢“° ¢f÷"v—&–ær–â°¢"öâ†—G2âÆâÆör†–æfòÂÂ'Â"•ÆâVæBöåÆâ"À¢"6öææV7EÆâ†—G2ÓâWc%ÆâVæB6öææV7EÆâ"À¢Ò°¢ÆWB7&2Ò66÷&V&ö&B‡v—&–ær“°¢òòD"Ô•"&VgW6W2ÂæB6—26òv—F†÷WBöffW&–ærcà¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚&WfVçBv—&–ær"’Â'¶×6wÒ"“° ¢76W'EöW€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’æW‡V7B‚'cVÖ—G2"’À¢V×G’À¢'cG&÷2G&ç67F÷"Ö†VÆBv—&–æs¢·v—&–æwÖ ¢“°¢Ð§Ð ¢òòòW&–öBFöW2æ÷B6†ævRv†BcFöW2v—F‚†öö¶VBÖWF†öBFƒ¢—G0¢òòò†öö²'&æ6‚æWfW"6öç7VÇG2‚çW&–öF–6ââV&Æ–W"‚çW&–öF–6 ¢òòò6öæ§Væ7B–âF†R&VF–6FRÖFRöâ2ç6VæB7–6ÆW2&V&V¦V7G6–à¢òòò7FFVÖVçB÷6—F–öâv†–ÆRF†RFW7B×66÷R&ÒÆ÷vW&VBF†R6ÖR6÷W&6Rà¢5·FW7EÐ¦fâ÷W&–öEööåöö†öö¶VEöÖWF†öE÷F…öFöW5öæ÷Eö6†ævU÷F†U÷fW&F–7B‚’°¢ÆWB7F×BÒ†ööµ÷÷6—F–öç2‚""Â""Âg&Uö†ööµööâ‚'2ç6VæB7–6ÆW2"Â""’“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚g7F×B’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚'7FFVÖVçB÷6—F–öâ"’Â'¶×6wÒ"“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7F×B’¢æW‡V7B‚'cVÖ—G2"¢æ6öçF–ç2‚%6VæFW%÷6VæE÷&RçW6…ö&6²"’À¢'cv—&W2—BÂ6òF†R7VvvW7F–öâ—2†öæW7B ¢“°¢Æ÷vW%÷7&2‚f†ööµ÷÷6—F–öç2€¢""À¢g&Uö†ööµööâ‚'2ç6VæB7–6ÆW2"Â""’À¢""À¢’¢æW‡V7B‚&æBF†RFW7B×66÷R&ÒÆ÷vW'2F†R6ÖR6÷W&6R"“°§Ð ¢òòòæÖVB&wVÖVçG2–â6ö×öæVçBÖWF†öB6ÆÂâæò4ôDTtTâ6—FR–âc¢òòò&VG2â&wVÖVçBäÔS¢öbF†R36ÆÄ&s£¤æÖVFÖF6†W2–à¢òòò7÷F"ç'6Â#R&R²fÇVRÂââÖæBöæR—2²fÇVS¢RÂââÖ(	@¢òòòÆÂ#bG&÷F†RæÖR(	BæBF†RBF†B&–æBæÖV&R5B×&Ww&—FP¢òòò76W2F†B&V6öç7G'V7BF†RæöFRâ&–æF–ær—2'’÷6—F–öâWfW'—v†W&Rà¢òòð¢òòòv—F‚Gvò&wVÖVçG2F†B—26–ÆVçB7v¢F†RW6W"w&—FW2F†RæÖW0¢òòò&V6—6VÇ’6òF†R÷&FW"FöW2æ÷BÖGFW"ÂæBcFöW2F†RöæRF†–æp¢òòòF†BÖ¶W2F†R÷&FW"ÖGFW"â—B6ö×–ÆW2æB—B'Vç2à¢òòð¢òòòv—F‚ôäR&wVÖVçBF†W&R—2æò÷F†W"÷6—F–öâf÷"F†RfÇVRFòÆæ@¢òòò–âÂ6òcVÖ—G2W†7FÇ’F†R÷6—F–öæÂ6ÆÂâF†B—26Æ–Ò&÷W@¢òòòF†R6ÆÂÂæ÷B&÷WBF†R6ÆÆVR(	B6VRF†RVæFW"×7WÇ’66RBF†P¢òòòVæBà¢òòð¢òòòD"Ô•"æ÷r7Æ—G2F†RF‡&VR66W2&F†W"F†â&VgW6–ærÆÂæÖV@¢òòò&wVÖVçG3¢–âFV6Æ&F–öâ÷&FW"Æ÷vW'2Â&V÷&FW&VB—0¢òòò6–ÆVçFÇ”Ö—4Æ÷vW'6ÂæBæÖRÖF6†–æræò&ÖWFW"—2–çfÆ–Fà¢5·FW7EÐ¦fâæÖVEö&wVÖVçG5ö&Uö&÷VæEö'•÷÷6—F–öåö'•÷c‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&†–Æ—FUöVçe÷FW7Bæ†&2"“°¢6öç7B4ÄÃ¢g7G"Ò&VçbæG'bæ†–Å÷w&—FR‡BæFG"ÂBçfÇVR’#°¢ÆWB6ÆÂÒÆ&w3¢g7G'Âf—‡GW&Rç&WÆ6Vâ„4ÄÂÂff÷&ÖB‚&VçbæG'bæ†–Å÷w&—FR‡¶&w7Ò’"’Â“° ¢òòF†R6öçG&öÂÂæBF†R6†RöbF†RVÖ—GFVB6ÆÂ—B&öGV6W2à¢Æ÷vW%÷7&2‚ff—‡GW&R’æW‡V7B‚'F†R÷6—F–öæÂf÷&ÒÆ÷vW'2"“°¢ÆWBcö7FÂÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚ff—‡GW&R’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢cö7FÂæ6öçF–ç2‚$†–Å†7F÷%ö†–Å÷w&—FR…÷F"æVçbæG'bÂBæFG"ÂBçfÇVR’"’À¢&6öçG&öÂ&–æG2FG"F†VâfÇVR ¢“° ¢òò6ÖR÷&FW#¢c†Vç2Fò&R&–v‡BÂv†–6‚—2v†BÖ¶W2F†P¢òò&WfW'6VB66R&VÆ÷rFævW&÷W2&F†W"F†âÖW&VÇ’'&ö¶Vâà¢76W'EöW€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f6ÆÂ‚&FG"ÒBæFG"ÂFFÒBçfÇVR"’’’æW‡V7B‚'cVÖ—G2"’À¢cö7FÂÀ¢&æÖW2–âFV6Æ&F–öâ÷&FW"VÖ—BF†R6ÖR6ÆÂ ¢“° ¢òò&WfW'6VC¢F†RfÇVW25tÂ6–ÆVçFÇ’à¢ÆWBc÷&WbÐ¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f6ÆÂ‚&FFÒBçfÇVRÂFG"ÒBæFG""’’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢c÷&Wbæ6öçF–ç2‚$†–Å†7F÷%ö†–Å÷w&—FR…÷F"æVçbæG'bÂBçfÇVRÂBæFG"’"’À¢'c&–æG2'’÷6—F–öâÂ6òFFÒâæÆæG2–âFG&æBf–6RfW'6 ¢“° ¢òòæÖRF†BÖF6†W2æò&ÖWFW"—266WFVBv—F†÷WBv÷&Bà¢76W'EöW€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f6ÆÂ‚&æ÷7V6‚ÒBæFG"ÂFFÒBçfÇVR"’’’æW‡V7B‚'cVÖ—G2"’À¢cö7FÂÀ¢&Ö—77VÆÆVB&ÖWFW"æÖR&öGV6W2æòF–væ÷7F–2BÆÂ ¢“° ¢òòD"Ô•"§VFvW2F†W6R'’t„U$RF†RæÖR—2w&—GFVâÂæ÷B'’F†Rf7@¢òòF†B—B—2æÖVBâF†—2&ÒW6VBFò&VgW6RÆÂf÷W"Æ–¶RÂ&V6W6P¢òò6ö×öæVçDÖWF†öE66†VÖ6'&–VB&ÖWFW"4õTåBæBF†R6VÒ†@¢òòæòæÖW2Fò6ö×&Rv–ç7C²—B6'&–W2&ÕöæÖW6æ÷rà¢òð¢òò–âFV6Æ&F–öâ÷&FW"F†RæÖR—2–æW'B(	BcVÖ—G2F†R6öçG&öÂ6ÆÀ¢òò'—FRÖf÷"Ö'—FRÂ76W'FVB&÷fR(	B6ò&VgW6–ær—B&VgW6VBv÷&¶–æp¢òò&öw&Òà¢Æ÷vW%÷7&2‚f6ÆÂ‚&FG"ÒBæFG"ÂFFÒBçfÇVR"’’æW‡V7B‚&æÖW2–â÷&FW"Æ÷vW""“°¢Æ÷vW%÷7&2‚f6ÆÂ‚'BæFG"ÂFFÒBçfÇVR"’’æW‡V7B‚&G&–Æ–ær–âÖ÷&FW"æÖRÆ÷vW'2"“° ¢òò&V÷&FW&VB—2F†R6–ÆVçB7và¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚f6ÆÂ‚&FFÒBçfÇVRÂFG"ÒBæFG""’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B€¢×6ræ6öçF–ç2‚&FF—2&ÖWFW""†W&R'WBv2w&—GFVâ–â÷6—F–öâ"’À¢'¶×6wÒ ¢“° ¢òòæÖRÖF6†–æræò&ÖWFW"—2&öw&ÒW'&÷#¢cVÖ—G2F†P¢òò6öçG&öÂ6ÆÂÂ6òæ÷F†–ær—2Ö—2ÖÆ÷vW&VB(	BF†W&R—26–×Ç’æð¢òò&6¶VæBF†B6÷VÆB†öæ÷W"F†RæÖRà¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚f6ÆÂ‚&æ÷7V6‚ÒBæFG"ÂFFÒBçfÇVR"’’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&æ÷7V6†æÖW2æò&ÖWFW"öb"’Â'¶×6wÒ"“° ¢òò&—G’¢æò6Æ÷BFò7vv—F‚Â6òc—2&VÂW66R†F6‚à¢òò†–Å÷&VB†FG"–Æ—fW2–âF†R6ÖRf—‡GW&Rà¢ÆWBöæRÒÆ&s¢g7G'Â°¢f—‡GW&Rç&WÆ6Vâ€¢4ÄÂÀ¢ff÷&ÖB‚'´4ÄÇÕÆâÆWB÷&"ÒVçbæG'bæ†–Å÷&VB‡¶&wÒ’"’À¢À¢¢Ó°¢ÆWBcööæUö7FÂÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚föæR‚'BæFG""’’’æW‡V7B‚'cVÖ—G2"“°¢òòF†Ræ6†÷#¢F†R–æ¦V7FVB6ÆÂ&V6†W2cw2÷WGWBÂ6ò'—FP¢òò–FVçF—G’&VÆ÷r—2F†RäÔR&V–ær–væ÷&VB&F†W"F†â&÷F‚6–FW0¢òòG&÷–ærF†R6ÆÂFövWF†W"à¢76W'EöW€¢cööæUö7FÀ¢æÖF6†W2‚$†–Å†7F÷%ö†–Å÷&VB…÷F"æVçbæG'bÂBæFG"’"¢æ6÷VçB‚’À¢cö7FÀ¢æÖF6†W2‚$†–Å†7F÷%ö†–Å÷&VB…÷F"æVçbæG'bÂBæFG"’"¢æ6÷VçB‚¢²À¢'F†R–æ¦V7FVB†–Å÷&VF6ÆÂ×W7BFB6ÆÂ6—FR ¢“°¢òò&—G’ÂæÖR–â—G2÷vâ÷6—F–öã¢–æW'BÂæB—BÆ÷vW'2âcVÖ—G0¢òòW†7FÇ’F†R÷6—F–öæÂ6ÆÂÂ76W'FVB&VÆ÷r(	B6òF†RöÆB&Ææ¶W@¢òò&VgW6Âöb&æÖVB&wVÖVçB–â6–ævÆRÖ&wVÖVçB6ö×öæVçB6ÆÂ ¢òòv2&VgW6–ærv÷&¶–ær&öw&Òà¢Æ÷vW%÷7&2‚föæR‚&FG"ÒBæFG""’’æW‡V7B‚&6÷'&V7FÇ’ÖæÖVB6–ævÆR&wVÖVçBÆ÷vW'2"“°¢òòæÖRÖF6†–æræò&ÖWFW"—27F–ÆÂ&öw&ÒW'&÷"à¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚föæR‚&æ÷7V6‚ÒBæFG""’’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&æ÷7V6†æÖW2æò&ÖWFW"öb"’Â'¶×6wÒ"“°¢f÷"&r–â²&FG"ÒBæFG""Â&æ÷7V6‚ÒBæFG"%Ò°¢76W'EöW€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚föæR†&r’’’æW‡V7B‚'cVÖ—G2"’À¢cööæUö7FÂÀ¢&†–Å÷&VB‡¶&wÒ–VÖ—G2W†7FÇ’F†R÷6—F–öæÂ6ÆÂ ¢“°¢Ð ¢òòF†R7Æ—B¶W—2öâF†R$uTÔTåB6÷VçBÂæ÷BF†R6ÆÆVRw2&ÖWFW ¢òò6÷VçBÂæBF†RÖW76vR×W7Bæ÷B&R&VB26Æ–Ò&÷WBF†P¢òòÆGFW"â6–ævÆRæÖVB&wVÖVçBFòF†REtò×&ÖWFW"ÖWF†öBVÖ—G0¢òòW†7FÇ’v†BF†RWV—fÆVçB÷6—F–öæÂ6ÆÂVÖ—G2(	B&÷F‚&P¢òòVæFW"×7WÆ–VBæBæV—F†W"6ö×–ÆW2Â'WBF†B—2&RÖW†—7F–æp¢òò&—G’v‡F&—"Æ÷vW'2†–Å÷w&—FR‡BçfÇVR–Föò’&F†W"F†à¢òò6öÖWF†–æræÖ–ærF†R&wVÖVçB6W6VBà¢òò6–ævÆRæÖVB&wVÖVçBFòF†REtò×&ÖWFW"ÖWF†öB—0¢òòTäDU"Õ5UÄ”TBâFFæÖW2&VÂ&ÖWFW"Â'WBv—F‚öæP¢òò&wVÖVçB7WÆ–VBF†R÷6—F–öç2Fòæ÷B6÷'&W7öæBÂ6òF†W&R—2æð¢òò7vFòFW67&–&R(	B6Æ–Ö–æröæRv÷VÆB&RfÇ6RW‡ÆæF–öâöb¢òò&RÖW†—7F–ær&—G’vâ—BÆ÷vW'2ÂW†7FÇ’2F†R÷6—F–öæÀ¢òòVæFW"×7WÇ’FöW2à¢Æ÷vW%÷7&2‚f6ÆÂ‚&FFÒBçfÇVR"’’æW‡V7B‚&æÖVBVæFW"×7WÇ’Æ÷vW'2"“°¢Æ÷vW%÷7&2‚f6ÆÂ‚'BçfÇVR"’’æW‡V7B‚'F†R÷6—F–öæÂVæFW"×7WÇ’Ç6òÆ÷vW'2FöF’"“°¢ÆWBcöæÖVE÷6†÷'BÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f6ÆÂ‚&FFÒBçfÇVR"’’’æW‡V7B‚'cVÖ—G2"“°¢òòæ6†÷"f—'7C¢F†RVæFW"×7WÆ–VB6ÆÂ&VÆÇ’—2–âcw2÷WGWBÀ¢òò6òF†R–FVçF—G’&VÆ÷r—2æ÷BGvò'6Væ6W2ÖF6†–ærà¢76W'B€¢cöæÖVE÷6†÷'Bæ6öçF–ç2‚$†–Å†7F÷%ö†–Å÷w&—FR…÷F"æVçbæG'bÂBçfÇVR’"’À¢'F†RVæFW"×7WÆ–VB6ÆÂ&V6†W2cw2÷WGWB ¢“°¢76W'EöW€¢cöæÖVE÷6†÷'BÀ¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f6ÆÂ‚'BçfÇVR"’’’æW‡V7B‚'cVÖ—G2"’À¢'F†RæÖR6†ævW2æ÷F†–ær&÷WBv†BcVÖ—G2 ¢“°§Ð ¢òòòF†R6–ævÆRÖ&wVÖVçB†V'F&VBæBV–W66R&VF–6FW26†&RF†P¢òòòæÖVBÖ&wVÖVçB6öç7G'V7B'WBæ÷B—G2†¦&BÂf÷"F†R6ÖR&V6öâF†P¢òòòöæRÖ&wVÖVçBÖWF†öB6ÆÂFöW2æ÷B†fR—BâF†W’¶VWVç7W÷'FVFà¢5·FW7EÐ¦fâöæÖVEö&wVÖVçE÷FõöööæUö&wVÖVçE÷&VF–6FUö—5ö÷&VÅ÷cöW66Uö†F6‚‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&vVçEööåö†æFÆW%÷FW7Bæ†&2"“°¢6öç7B”DÄS¢g7G"Ò&76W'BFvvW"æ–FÆUö–âƒB•Æâ#°¢76W'B†f—‡GW&Ræ6öçF–ç2„”DÄR’Â&f—‡GW&R6†R6†ævVB"“°¢ÆWBcö7FÂÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚ff—‡GW&R’’æW‡V7B‚'cVÖ—G2"“°¢òòF†Ræ6†÷#¢F†R&VF–6FRw2&wVÖVçB—2f—6–&ÆR–âF†R÷WGWBÂ6ð¢òòÆFW"'—FRÖ–FVçF—G’—2F†RäÔR&V–ær–væ÷&VB&F†W"F†âF†P¢òò6ÆÂfæ—6†–ærà¢76W'B€¢cö7FÂæ6öçF–ç2‚'FvvW"åöÆ7Eö–åö7–6ÆR’ãÒ‡V–çCcE÷B’ƒB’"’À¢'F†R7–6ÆR6÷VçB&V6†W2F†RVÖ—GFVB&VF–6FR ¢“° ¢f÷"&r–â²&âÒB"Â&æ÷7V6‚ÒB%Ò°¢ÆWB7&2Òf—‡GW&Rç&WÆ6Vâ„”DÄRÂff÷&ÖB‚&76W'BFvvW"æ–FÆUö–â‡¶&wÒ•Æâ"’Â“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&æÖVB&wVÖVçBFò–FÆUö–æ"’Â'¶×6wÒ"“°¢76W'EöW€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’æW‡V7B‚'cVÖ—G2"’À¢cö7FÂÀ¢'cG&÷2F†RæÖRæBVÖ—G2F†R6ÖR&VF–6FR ¢“°¢Ð ¢òòV–W66VF—26W&FR&Òv—F‚—G2÷vâÖW76vRÂæBF†P¢òòFö26öÖÖVçB6Æ–ÖVB—Bv—F†÷WBW†W&6—6–ær—Bà¢ÆWBV–W66RÒÆ&s¢g7G'Â°¢f—‡GW&Rç&WÆ6Vâ€¢”DÄRÀ¢ff÷&ÖB‚&76W'BFvvW"æ–FÆUö–âƒB•Æâ76W'BFvvW"çV–W66VB‡¶&wÒ•Æâ"’À¢À¢¢Ó°¢ÆWBc÷ö7FÂÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gV–W66R‚#B"’’’æW‡V7B‚'cVÖ—G2"“°¢Æ÷vW%÷7&2‚gV–W66R‚#B"’’æW‡V7B‚'F†R÷6—F–öæÂV–W66Rf÷&ÒÆ÷vW'2"“°¢òòF†R6ÖRæ6†÷"—G26–&Æ–ærW6W3¢F†R$uTÔTåB—2f—6–&ÆR–âF†P¢òòVÖ—GFVB&VF–6FRâ76W'EöæRv–ç7BF†R6öçG&öÂv÷VÆB&P¢òò6F—6f–VB'’ç’FFVBÆ–æRBÆÂà¢76W'B€¢c÷ö7FÂæ6öçF–ç2‚%öÆ7Eö÷WEö7–6ÆR’ãÒ‡V–çCcE÷B’ƒB’"¢ÇÂc÷ö7FÂæ6öçF–ç2‚%öÆ7Eö–åö7–6ÆR’ãÒ‡V–çCcE÷B’ƒB’"’À¢'F†RV–W66R7–6ÆR6÷VçB&V6†W2F†RVÖ—GFVB&VF–6FR ¢“°¢f÷"&r–â²&âÒB"Â&æ÷7V6‚ÒB%Ò°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚gV–W66R†&r’’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&æÖVB&wVÖVçBFòV–W66VF"’Â'¶×6wÒ"“°¢76W'EöW€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gV–W66R†&r’’’æW‡V7B‚'cVÖ—G2"’À¢c÷ö7FÂÀ¢'cG&÷2F†RæÖRæBVÖ—G2F†R6ÖRV–W66R&VF–6FR ¢“°¢Ð§Ð ¢òòòF†R6ö×öæVçB×&ÖWFW"6öç7G'V7B†2dõU"ÆæF–æw2æBF†W’ÆÀ¢òòòw&VS¢cG&÷2F†RÆ—7BÂæB&VfW&Væ6RVÖ—GFVBgFW"—G0¢òòòf–ÆR×66÷R6öç7G26–ÆVçFÇ’–6·2öæRWà¢òòð¢òòòF†R66÷&V&ö&B&Òv2'&–VfÇ’Æ&VÆÆVB'VærÆ÷vW"ÂöâF†R&wVÖVç@¢òòòF†BFFÖöæÇ’66÷&V&ö&B†2öæÇ’f–VÆG2æB6òæòVÖ—76–öà¢òòò÷6—F–öâgFW"F†R6öç7G2â66÷&V&ö&Eö—5ö6ö×öæVçF&÷WFW2öà¢òòò†öö¶&ÆVÆöæRÂ6ò66÷&V&ö&Bv—F‚âöæ†æFÆW"7F—0¢òòòFFÖöæÇ’æB†2öæRâ&÷F‚÷6—F–öç2&R–ææVB&VÆ÷r(	BFW7BF†@¢òòòW†W&6—6VBöæÇ’F†Rf–VÆBFVfVÇB—2v†BÆWBF†Rw&öærÆ&VÂ72à¢5·FW7EÐ¦fâF†Uö÷F†W%÷Gvõö6ö×öæVçE÷&ÖWFW%öÆæF–æw5öFõöæ÷Eöw&VR‚’°¢òò)H)HG&ç67F÷'2ç'6¢â÷&F–æ'’EUB×ö¶–ærG&ç67F÷"à¢ÆWBbÒf—‡GW&R‚&†–Æ—FUö†öö·5÷FW7Bæ†&2"“°¢6öç7BEƒ¢g7G"Ò'G&ç67F÷"†ööµ†7F÷"#°¢6öç7B5E$#¢g7G"Ò&GWBæ†–Å÷u÷7G&"ÒR#°¢76W'B†bæ6öçF–ç2…E‚’bbbæ6öçF–ç2…5E$"’Â&f—‡GW&R6†R6†ævVB"“° ¢ÆWBv—F…÷&ÒÒf÷&ÖB€¢&6öç7BâÒUÆåÆç·Ò"À¢bç&WÆ6Vâ…E‚Â'G&ç67F÷"†ööµ†7F÷"2„ã¢–çBÒ2’"Â¢ç&WÆ6Vâ…5E$"Â&GWBæ†–Å÷u÷7G&"Òâ"Â¢“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚gv—F…÷&Ò’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚&vVæW&–2&ÖWFW'2"’Â'¶×6wÒ"“° ¢ÆWBc÷&ÒÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gv—F…÷&Ò’’æW‡V7B‚'cVÖ—G2"“°¢ÆWBÆ–æUööbÒÆ÷WC¢g7G"ÂæVVFÆS¢g7G'Â°¢÷WBæÆ–æW2‚¢ç÷6—F–öâ‡ÆÇÂÂæ6öçF–ç2†æVVFÆR’¢çVçw&ö÷%öVÇ6R‡ÇÂæ–2‚&¶æVVFÆWÖÖ—76–ærg&öÒcw2÷WGWB"’¢Ó°¢òòF†R6öç7B&V6VFW2F†RÖWF†öBÖ&öG’W6RÂ6òF†—2öæR6ö×–ÆW2à¢76W'B€¢Æ–æUööb‚gc÷&ÒÂ'7FF–26öç7FW‡"–çCcE÷BâÒS²"¢ÂÆ–æUööb‚gc÷&ÒÂ&†–Å÷u÷7G&"Ââ’"’À¢'F†R6öç7B&V6VFW2F†RW6R ¢“°¢òòæB2„ã¢–çBÒ2–—2–çf—6–&ÆS¢F†R6ÖR6÷W&6Rv—F‚æð¢òò&ÖWFW"BÆÂVÖ—G2'—FRÖ–FVçF–6ÆÇ’à¢ÆWB6öç7EööæÇ’Òf÷&ÖB€¢&6öç7BâÒUÆåÆç·Ò"À¢bç&WÆ6Vâ…5E$"Â&GWBæ†–Å÷u÷7G&"Òâ"Â¢“°¢Æ÷vW%÷7&2‚f6öç7EööæÇ’’æW‡V7B‚'F†R6öç7BÖöæÇ’f÷&ÒÆ÷vW'2"“°¢76W'EöW€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f6öç7EööæÇ’’’æW‡V7B‚'cVÖ—G2"’À¢c÷&ÒÀ¢'F†R&ÖWFW"6†ævW2æ÷F†–ærÂ6òF†RG&ç67F÷"'Vç2v—F‚R ¢“° ¢òò)H)H66÷&V&ö&G2ç'6¢w&VW2ÂæBF†R÷6—F–öâF†BFV6–FW2—B—0¢òòäõBF†Rf–VÆBFVfVÇBâf—'7B72Æ&VÆÆVBF†—2&Ð¢òòVÖ—G5Væ6ö×–Æ&ÆV&V6W6RFFÖöæÇ’66÷&V&ö&B&öæÇ’†0¢òòf–VÆG2"Âv†÷6RFVfVÇG2&RVÖ—GFVB–ç6–FRF†R7G'V7B†VBö`¢òòWfW'’6öç7Bâ'WB66÷&V&ö&Eö—5ö6ö×öæVçF&÷WFW2FòF†P¢òò6ö×÷6—FRF&ÆRöâ†öö¶&ÆVÄôäRÂ6ò66÷&V&ö&Bv—F€¢òòf–VÆG2ÇW2âöæ†æFÆW"7F—2FFÖöæÇ’æB&V6†W2F†—0¢òò&Ò(	BæBcVÖ—G2F†B†æFÆW"w2G&–vvW"ÆöæreDU"F†R6öç7Bà¢òð¢òò&÷F‚÷6—F–öç2&R76W'FVBÂ&V6W6R–ææ–æröæÇ’F†Rf–VÆ@¢òòFVfVÇB—2v†BÆWBF†Rw&öærÆ&VÂ72à¢6öç7B4%õ5$3¢g7G"Ò"2&6öç7BâÒP ¦FöÖ–â7—4FöÖ–à¢g&WöÖ‡£¢ ¦VæBFöÖ–â7—4FöÖ–à §66÷&V&ö&B&ö&DDT4À¢†—G2¢V–çCÃ3#âFVfVÇB„•E0¤ôä„äDÄU ¦VæB66÷&V&ö&B&ö&@ §FW7F&Væ6‚6%F ¢GWB¢F÷ ¢"¢&ö&D”å5@¦VæBFW7F&Væ6‚6%F  ¦–×Â6%FW7Bf÷"6%F ¢6Æö6²6Æ²Ò7—4FöÖ–à¢'Và¢GWBç'7BÒ¢v—B"7–6ÆW0¢VæB'Và¦VæB–×Â6%FW7@¢"3°¢ÆWB6"ÒÆFV6Ã¢g7G"Â–ç7C¢g7G"Â†—G3¢g7G"Âöã¢g7G'Â°¢ÆWB2Ò4%õ5$0¢ç&WÆ6R‚$&ö&DDT4Â"ÂFV6Â¢ç&WÆ6R‚$&ö&D”å5B"Â–ç7B¢ç&WÆ6R‚$„•E2"Â†—G2“°¢–böâæ—5öV×G’‚’°¢2ç&WÆ6R‚$ôä„äDÄU%Æâ"Â""¢ÒVÇ6R°¢2ç&WÆ6R‚$ôä„äDÄU""Âöâ¢Ð¢Ó°¢6öç7Bôåôã¢g7G"Ò"öâ†—G2âåÆâ†—G2Ò†—G2²ÆâVæBöâ#° ¢òòF†R†æFÆW"×G&–vvW"÷6—F–öã¢VÖ—GFVBgFW"F†R6öç7BÂ6ò—@¢òò6ö×–ÆW2æBF†R66÷&V&ö&B6–ÆVçFÇ’'Vç2v—F‚F†R6öç7Bw2Rà¢ÆWB6–ÆVçBÒ6"‚$&ö&B2„ã¢–çBÒ2’"Â$&ö&B2ƒr’"Â#"Âôåôâ“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g6–ÆVçB’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚'&ÖWFW'2öâ66÷&V&ö&B"’Â'¶×6wÒ"“°¢ÆWBc÷6–ÆVçBÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g6–ÆVçB’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢Æ–æUööb‚gc÷6–ÆVçBÂ'7FF–26öç7FW‡"–çCcE÷BâÒS²"¢ÂÆ–æUööb‚gc÷6–ÆVçBÂ%÷F"æ"æ†—G2ââ"’À¢'F†R6öç7B&V6VFW2F†R†æFÆW"G&–vvW"Â6òF†—26ö×–ÆW2 ¢“°¢òòWVÂÖÆVæwF‚&wVÖVçG2Â6ò6÷W&6Röfg6WG2Fòæ÷B6†–gBæBF†P¢òò–FVçF—G’—2F†R$uTÔTåB&V–ær–çf—6–&ÆR&F†W"F†âæö—6Rà¢76W'EöW€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g6"€¢$&ö&B2„ã¢–çBÒ2’"À¢$&ö&B2ƒ‚’"À¢#"À¢ôåôà¢’’¢æW‡V7B‚'cVÖ—G2"’À¢c÷6–ÆVçBÀ¢&2ƒr–æB2ƒ‚–VÖ—B–FVçF–6ÆÇ’Â6òF†R&wVÖVçBFöW2æ÷F†–ær ¢“° ¢òòF†Rf–VÆBÖFVfVÇB÷6—F–öâÂv†–6‚—2v†W&RF†Rw&öærÆ&VÂ6ÖP¢òòg&öÓ¢VÖ—GFVB–ç6–FRF†R7G'V7BÂ†VBöbF†R6öç7Bà¢ÆWB'•öFVfVÇBÒ6"‚$&ö&B2„ã¢–çBÒ2’"Â$&ö&B2ƒr’"Â$â"Â""“°¢76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚f'•öFVfVÇB’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢ÆWBcöFVfVÇBÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f'•öFVfVÇB’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢Æ–æUööb‚gcöFVfVÇBÂ'V–çCcE÷B†—G2Òã²"¢ÂÆ–æUööb‚gcöFVfVÇBÂ'7FF–26öç7FW‡"–çCcE÷BâÒS²"’À¢'F†Rf–VÆB–æ—F–Æ—¦W"&V6VFW2F†R6öç7BÂ6òF†B÷6—F–öâFöW2æ÷B6ö×–ÆR ¢“°§Ð ¢òòò2‚âââ–&ÖWFW'2öâ6ö×öæVçBâcæWfW"&VG2F†RÆ—7BÂæ@¢òòòD…$TRF†–æw2föÆÆ÷rÂöæÇ’F†RÆ7Böbv†–6‚V&ç2F†RÆ&VÂà¢òòð¢òòòFV6Æ&VB'WBVçW6VBÂcw2÷WGWB—2'—FRÖ–FVçF–6ÂFòF†R6ÖP¢òòò6ö×öæVçBw&—GFVâv—F†÷WBF†R&ÖWFW"(	BæB2ƒB–&wVÖVçB@¢òòòF†R–ç7FçF–F–öâfæ—6†W2v—F‚—BÂ6òF†R¶æö"F†RW6W"FFVBFöW0¢òòòæ÷F†–ærâ&VfW&Væ6VBv—F‚æòæÖRFòfÆÂ&6²öâÂFVfVÇBæVÖ—G0¢òòòV–çCcE÷BÆ–Ö—BÒã¶v—F‚æFV6Æ&VBæ÷v†W&Râ&VfW&Væ6VBg&öÒ¢òòò„äDÄU"$ôE’v†–ÆRf–ÆR×66÷R6öç7BæW†—7G2ÂF†R&VfW&Væ6P¢òòò6–ÆVçFÇ’&W6öÇfW2FòF†R6öç7BæBF†R&öw&Ò'Vç2v—F‚F†Rw&öæp¢òòòfÇVRà¢òòð¢òòòF†R÷6—F–öâöbF†R&VfW&Væ6R—2v†B6W&FW2F†RÆ7BGvòÂæB—@¢òòò—2æ÷B–çGV—F–öã¢cVÖ—G2F†R6öç7BeDU"F†R6ö×öæVçB7G'V7BÂ6ò¢òòòf–VÆBFVfVÇB7F–ÆÂf–Ç2Fò6ö×–ÆRWfVâv—F‚F†R6öç7B&W6VçBà¢òòò&÷F‚vW&R6†V6¶VB'’7Æ–6–ærF†RVÖ—GFVB&Vv–öâ–çFòr²²v—F‚F†P¢òòòvVæW&FVBf–ÆRw2†VFW"6WBÂv–ç7B6öçG&öÂF†BÖ÷fW2öæÇ’F†P¢òòò6öç7Bà¢òòð¢òòò&ö&VBB&÷F‚6ö×öæVçG2ç'6ÆæF–æw2(	BF†RæÇ—6—2×6÷W&6P¢òòò‡G&ç67F÷"’&ÒæBF†RVçbövVçB÷6WVVæ6W"6ö×÷6—FR&ÒâF†R÷F†W ¢òòòGvòÂ–âG&ç67F÷'2ç'6æB66÷&V&ö&G2ç'6Â†fRF†V—"÷vâFW7Bà¢5·FW7EÐ¦fâ6ö×öæVçE÷&ÖWFW'5ö&UöG&÷VEö'•÷c‚’°¢6öç7B5$3¢g7G"Ò"2 ¦FöÖ–â7—4FöÖ–à¢g&WöÖ‡£¢ ¦VæBFöÖ–â7—4FöÖ–à §G&ç67F–öâF–ç•G†à¢Fr¢V–çCÃƒà¦VæBG&ç67F–öâF–ç•G†à ¤´”äEtõ$BFvvW$DT4À¢–åöWb¢WfVçCÅF–ç•G†ãà¢6VVâ¢V–çCÃ3#âFVfVÇB ¢Æ–Ö—B¢V–çCÃ3#âFVfVÇBÄ”Ô•@ ¢öâ–åöWb‡B¢6VVâÒ6VVâ²¢VæBöà¦VæB´”äEtõ$BFvvW  §FW7F&Væ6‚&ÕF ¢GWB¢F÷ ¢Fr¢FvvW$”å5BÔôDP¦VæBFW7F&Væ6‚&ÕF  ¦–×Â&ÕFW7Bf÷"&ÕF ¢6Æö6²6Æ²Ò7—4FöÖ–à¢'Và¢GWBç'7BÒ¢v—B"7–6ÆW0¢VæB'Và¦VæB–×Â&ÕFW7@¢"3°¢òòöæR6†RÂGvòÆæF–æw3¢vVçF&V6†W2F†R6ö×÷6—FRÖ6ö×öæVç@¢òò&ÒæBG&ç67F÷"âââ76—fVF†RæÇ—6—2×6÷W&6RöæRâF†—0¢òòG&ç67F÷"†2öæÇ’âÇv—2ÖöâæÇ—6—27W&f6RÂ6ò7F—fVv÷VÆ@¢òòf–öÆFRF†RæÇ—6—2×6÷W&6RÖöFR6öçG&7B&Vf÷&RF†R&ÖWFW ¢òòÆæF–ærVæFW"FW7B—2&V6†VBà¢f÷"†¶–æBÂÖöFRÂ6öç7G'V7B’–â°¢‚&vVçB"Â""Â'&ÖWFW'2öâFvvW&"’À¢€¢'G&ç67F÷""À¢'76—fR"À¢&vVæW&–2&ÖWFW'2öâæÇ—6—2×6÷W&6RFvvW&"À¢’À¢Ò°¢ÆWBÖ²ÒÆFV6Ã¢g7G"Â–ç7C¢g7G"ÂÆ–Ö—C¢g7G'Â°¢5$2ç&WÆ6R‚$´”äEtõ$B"Â¶–æB¢ç&WÆ6R‚%FvvW$DT4Â"ÂFV6Â¢ç&WÆ6R‚%FvvW$”å5BÔôDR"Âff÷&ÖB‚'¶–ç7GÒ¶ÖöFWÒ"’¢ç&WÆ6R‚$Ä”Ô•B"ÂÆ–Ö—B¢Ó°¢ÆWB7FÂÒÖ²‚%FvvW""Â%FvvW""Â#r"“°¢Æ÷vW%÷7&2‚f7FÂ’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'¶¶–æGÒ6öçG&öÂÆ÷vW'3¢¶S£÷Ò"’“°¢ÆWBcö7FÂÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f7FÂ’’æW‡V7B‚'cVÖ—G2"“° ¢òòGvòæ6†÷'2âF†R6ö×öæVçB6öçG&–'WFW2BÆÂÂæBF†Rf–VÆ@¢òòFVfVÇF—B6'&–W2—2f—6–&ÆR–âF†R÷WGWB(	B6ò'—FP¢òò–FVçF—G’&VÆ÷r—2F†R$ÔUDU"&V–ærG&÷VB&F†W"F†âF†P¢òòv†öÆR6ö×öæVçB&V–ær–æW'Bà¢ÆWBv—F†÷WBÒ7FÂç&WÆ6R‚ff÷&ÖB‚"Fr¢FvvW"¶ÖöFWÕÆâ"’Â""“°¢76W'EöæR€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gv—F†÷WB’’æW‡V7B‚'cVÖ—G2"’À¢cö7FÂÀ¢'¶¶–æGÓ¢F†R6ö×öæVçB×W7B6öçG&–'WFR ¢“°¢76W'B€¢cö7FÂæ6öçF–ç2‚'V–çCcE÷BÆ–Ö—BÒs²"’À¢'¶¶–æGÓ¢F†Rf–VÆBFVfVÇB&V6†W2F†R÷WGWB ¢“° ¢òòVçW6VB&ÖWFW#¢G&÷VBÂæB6ò—2F†R–ç7FçF–F–öâ&rà¢f÷"†FV6ÂÂ–ç7B’–â°¢‚%FvvW"2„ã¢–çB’"Â%FvvW""’À¢‚%FvvW"2„ã¢–çB’"Â%FvvW"2ƒB’"’À¢‚%FvvW"2„ã¢–çBÒ2’"Â%FvvW""’À¢Ò°¢ÆWB7&2ÒÖ²†FV6ÂÂ–ç7BÂ#r"“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2†6öç7G'V7B’Â'¶×6wÒ"“°¢76W'EöW€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’æW‡V7B‚'cVÖ—G2"’À¢cö7FÂÀ¢'¶¶–æGÓ¢¶FV6ÇÖò¶–ç7GÖVÖ—G22–bF†R&ÖWFW"vW&Ræ÷BF†W&R ¢“°¢Ð ¢òò&VfW&Væ6VB&ÖWFW#¢âVæFV6Æ&VBæÖR–âF†R÷WGWBà¢ÆWBW6VBÒÖ²‚%FvvW"2„ã¢–çBÒ2’"Â%FvvW""Â$â"“°¢76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚gW6VB’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢ÆWBc÷W6VBÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gW6VB’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢c÷W6VBæ6öçF–ç2‚'V–çCcE÷BÆ–Ö—BÒã²"’À¢'¶¶–æGÓ¢F†R&ÖWFW"æÖR7W'f—fW2–çFòF†R–æ—F–Æ—¦W" ¢“°¢76W'B€¢c÷W6V@¢æÆ–æW2‚¢æç’‡ÆÇÂÂçG&–Õ÷7F'B‚’ç7F'G5÷v—F‚‚"òò"’bbÂæ6öçF–ç2‚&–çCcE÷Bâ"’’À¢'¶¶–æGÓ¢æBæ—2FV6Æ&VBæ÷v†W&RÂ6ò—BFöW2æ÷B6ö×–ÆR ¢“° ¢òòF†R66RF†BT$å26–ÆVçFÇ”Ö—4Æ÷vW'6&F†W"F†à¢òòVÖ—G5Væ6ö×–Æ&ÆVÂæB—B—2äõBF†Rf–VÆBFVfVÇBâcVÖ—G0¢òòF†Rf–ÆR×66÷R6öç7BeDU"F†R6ö×öæVçB7G'V7BÂ6ò¢òòFVfVÇBæ–æ—F–Æ—¦W"—27F–ÆÂW6RÖ&Vf÷&RÖFV6Æ&F–öâWfVà¢òòv†VâF†R6öç7BW†—7G2â„äDÄU"Ô$ôE’&VfW&Væ6R—2VÖ—GFVBf ¢òòÆFW"æB&W6öÇfW2(	BF†Bf–ÆR6ö×–ÆW2ÂæBF†R6ö×öæVç@¢òò'Vç2v—F‚’–ç7FVBöbF†RBF†R–ç7FçF–F–öâ76VBà¢òð¢òò&÷F‚÷&FW&–æw2&R76W'FVB&VÆ÷rÂ&V6W6RF†Rf—'7BfW'6–öà¢òòöbF†—2FW7B6†V6¶VBöæÇ’F†BF†RGvò7G&–æw2vW&R$U4Tå@¢òòæB6öæ6ÇVFVBF†Rw&öæröæR6ö×–ÆW2à¢ÆWB6†F÷rÒÆÆ–Ö—C¢g7G"Â&öG“¢g7G'Â°¢f÷&ÖB€¢&6öç7BâÒ•Æç·Ò"À¢Ö²‚%FvvW"2„ã¢–çBÒ2’"Â%FvvW"2ƒB’"ÂÆ–Ö—B¢ç&WÆ6R‚'6VVâÒ6VVâ²"Âff÷&ÖB‚'6VVâÒ6VVâ²¶&öG—Ò"’¢¢Ó°¢ÆWBÆ–æUööbÒÆ÷WC¢g7G"ÂæVVFÆS¢g7G'Â°¢÷WBæÆ–æW2‚¢ç÷6—F–öâ‡ÆÇÂÂæ6öçF–ç2†æVVFÆR’¢çVçw&ö÷%öVÇ6R‡ÇÂæ–2‚'¶¶–æGÓ¢¶æVVFÆWÖÖ—76–ærg&öÒcw2÷WGWB"’¢Ó° ¢òòf–VÆBFVfVÇC¢6öç7BÆæG2gFW"F†R7G'V7BÂ6ò—BFöW2æ÷B†VÇà¢ÆWB'•öFVfVÇBÒ6†F÷r‚$â"Â#"“°¢76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚f'•öFVfVÇB’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢ÆWBcöFVfVÇBÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f'•öFVfVÇB’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢Æ–æUööb‚gcöFVfVÇBÂ'V–çCcE÷BÆ–Ö—BÒã²"¢ÂÆ–æUööb‚gcöFVfVÇBÂ'7FF–26öç7FW‡"–çCcE÷BâÒ“²"’À¢'¶¶–æGÓ¢F†R–æ—F–Æ—¦W"&V6VFW2F†R6öç7BÂ6ò—B6ææ÷B6ö×–ÆR ¢“° ¢òò†æFÆW"&öG“¢VÖ—GFVBgFW"F†R6öç7BÂ6ò—B&W6öÇfW2à¢ÆWB'•ö&öG’Ò6†F÷r‚#r"Â$â"“°¢76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚f'•ö&öG’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢ÆWBcö&öG’Ò7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f'•ö&öG’’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢Æ–æUööb‚gcö&öG’Â'7FF–26öç7FW‡"–çCcE÷BâÒ“²"’ÂÆ–æUööb‚gcö&öG’Â'6VVâ²â"’À¢'¶¶–æGÓ¢F†R6öç7B&V6VFW2F†RW6RÂ6òF†—2öæR6ö×–ÆW2 ¢“°¢òòæBF†Ræ6†÷"F†BÖ¶W2—BÔ•2ÖÆ÷vW&–ær&F†W"F†â¢òò6ö–æ6–FVæ6S¢F†R6ÖR6÷W&6Rv—F‚æò&ÖWFW"BÆÂVÖ—G2F†P¢òò–FVçF–6ÂW6RÂ6ò2ƒB–6†ævVBæ÷F†–ær&÷WBF†RfÇVRW6VBà¢ÆWBæõ÷&ÒÒf÷&ÖB€¢&6öç7BâÒ•Æç·Ò"À¢Ö²‚%FvvW""Â%FvvW""Â#r"’ç&WÆ6R‚'6VVâÒ6VVâ²"Â'6VVâÒ6VVâ²â"¢“°¢Æ÷vW%÷7&2‚fæõ÷&Ò¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'¶¶–æGÓ¢F†R6öç7BÖöæÇ’f÷&ÒÆ÷vW'3¢¶S£÷Ò"’“°¢76W'EöW€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fæõ÷&Ò’’æW‡V7B‚'cVÖ—G2"’À¢cö&öG’À¢'¶¶–æGÓ¢2„ã¢–çBÒ2–æB2ƒB–&R&÷F‚–çf—6–&ÆR–âF†R÷WGWB ¢“°¢Ð§Ð ¢òòòF†Rd”eD‚ÆæF–æröbF†R6ö×öæVçB×&ÖWFW"6öç7G'V7BÂöà¢òòòG&ç67F–öæâ—Bw&VW2v—F‚F†R÷F†W"f÷W"ÂæB—G26–ÆVçB÷6—F–öà¢òòò—26–ÆVçB–âv÷'6Rv“¢¶VW6öç7G&–çBFöW2æ÷BVÖ—BF†P¢òòò&ÖWFW"w2æÖRBÆÂÂ—B4ôå5BÔdôÄE2—Bv–ç7B6ÖRÖæÖV@¢òòòf–ÆR×66÷R6öç7FæB&¶W2F†BfÇVR–çFòF†R£26ÆÂà¢òòð¢òòòF†—2&Òv2ÆVgBVæ6Æ76–f–VBf÷"öæR6öÖÖ—BöâF†Rw&÷VæG2F†@¢òòòF‡&VR&ö&VB÷6—F–öç2ÆÂVÖ—GFVB†VBöbcw26öç7G2Âv—F‚F†P¢òòò¶VW&V6†–ær&öæÇ’Æör7G&–ær"âF†RÆörÆ–æR—2&VÂæBF†—'G¢òòòÆ–æW2$TÄõrF†R6öÇfW"Æ–æRF†BFöW2F†RföÆF–ærà¢5·FW7EÐ¦fâ÷G&ç67F–öå÷&ÖWFW%ö—5ö6öç7EöföÆFVE÷Fõ÷F†U÷w&öæuö&÷VæB‚’°¢6öç7B5$3¢g7G"Ò"2&6öç7BâÒP ¦FöÖ–â7—4FöÖ–à¢g&WöÖ‡£¢ ¦VæBFöÖ–â7—4FöÖ–à §G&ç67F–öâF–ç•G†äDT4À¢Fr¢V–çCÃƒâDTdTÅ@ ¢¶VWFrÂ´TU ¦VæBG&ç67F–öâF–ç•G†à §FW7F&Væ6‚&V5F ¢GWB¢F÷ ¦VæBFW7F&Væ6‚&V5F  ¦–×Â&V5FW7Bf÷"&V5F ¢6Æö6²6Æ²Ò7—4FöÖ–à¢'Và¢ÆWBB¢F–ç•G†à¢&æFöÖ—¦R‡B¢GWBç'7BÒ¢v—B"7–6ÆW0¢VæB'Và¦VæB–×Â&V5FW7@¢"3°¢ÆWBÖ²ÒÆFV6Ã¢g7G"ÂFVfVÇC¢g7G"Â¶VW¢g7G'Â°¢5$2ç&WÆ6R‚%F–ç•G†äDT4Â"ÂFV6Â¢ç&WÆ6R‚"DTdTÅB"ÂFVfVÇB¢ç&WÆ6R‚$´TU"Â¶VW¢Ó°¢ÆWBÆ–æUööbÒÆ÷WC¢g7G"ÂæVVFÆS¢g7G'Â°¢÷WBæÆ–æW2‚¢ç÷6—F–öâ‡ÆÇÂÂæ6öçF–ç2†æVVFÆR’¢çVçw&ö÷%öVÇ6R‡ÇÂæ–2‚&¶æVVFÆWÖÖ—76–ærg&öÒcw2÷WGWB"’¢Ó° ¢òòF†R6–ÆVçB÷6—F–öã¢¶VWFrÂæföÆG2FòF†R4ôå5Bw2Rà¢ÆWBföÆFVBÒÖ²‚%F–ç•G†â2„ã¢–çBÒ2’"Â""Â$â"“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚fföÆFVB’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚'&ÖWFW'2öâG&ç67F–öâ"’Â'¶×6wÒ"“°¢ÆWBcöföÆFVBÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fföÆFVB’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢cöföÆFVBæ6öçF–ç2‚%÷2æFB‡£3£§VÇB…÷¥÷FrÂö7G‚æ'e÷fÂ‚‡V–çCcE÷B“RÂcB’’’"’À¢'F†R6öÇfW"vWG2F†R6öç7Bw2R ¢“°¢òòæ÷BF†R&ÖWFW"w2÷vâFVfVÇBÂæBæòæ7W'f—fW2Fòæ÷F–6Rà¢76W'B€¢cöföÆFVBæ6öçF–ç2‚%ö7G‚æ'e÷fÂ‚‡V–çCcE÷B“2ÂcB’"’À¢'F†R&ÖWFW"w2FVfVÇB2&V6†W2F†R6öÇfW"æ÷v†W&R ¢“°¢òòF†R&V6÷&FVB&ö&ÆVÒFW67&—F÷"6'&–W2F†RföÆFVBfÇVRFöòÂ6ð¢òòF†R6öç7G&–çBF†R'VçF–ÖR&W÷'G2—2FrÂVÂæ÷BFrÂæà¢76W'B€¢cöföÆFVBæ6öçF–ç2‚"‡Fs§S‚ÂS§S‚“¦&ööÂ"’À¢'F†R&ö&ÆVÒF&ÆR&V6÷&G2F†RföÆFVB&÷VæB ¢“°¢òò¶VWFrÂææB¶VWFrÂVF–ffW"ôäÅ’–âF†Rd”ÂÆöp¢òòÆ–æRÂv†–6‚V6†öW2F†R6÷W&6RFW‡BfW&&F–ÒâWfW'—F†–ærF†P¢òò&öw&Ò7GVÆÇ’W†V7WFW2—2–FVçF–6Âà¢ÆWBÆ—FW&ÂÐ¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fÖ²‚%F–ç•G†â2„ã¢–çBÒ2’"Â""Â#R"’’’æW‡V7B‚'cVÖ—G2"“°¢ÆWB7G&—öÆörÒÆó¢g7G'Â°¢òæÆ–æW2‚¢æf–ÇFW"‡ÆÇÂÂæ6öçF–ç2‚''F–6—FVB–âF†R6öÇfR"’¢æ6öÆÆV7C££ÅfV3Åóãâ‚¢æ¦ö–â‚%Æâ"¢Ó°¢76W'EöW€¢7G&—öÆör‚gcöföÆFVB’À¢7G&—öÆör‚fÆ—FW&Â’À¢&÷WG6–FRF†RV6†öVBÆörFW‡BÂ¶VWFrÂæ•2¶VWFrÂV ¢“°¢òòF†RÆörÆ–æRF†B†–BF†—3¢—B—2$TÄõrF†R6öÇfW"Æ–æRà¢76W'B€¢Æ–æUööb€¢gcöföÆFVBÀ¢%÷2æFB‡£3£§VÇB…÷¥÷FrÂö7G‚æ'e÷fÂ‚‡V–çCcE÷B“RÂcB’’’ ¢’ÂÆ–æUööb‚gcöföÆFVBÂ''F–6—FVB–âF†R6öÇfR"’À¢'&VF–ærF†RÆörÆ–æRÆöæRÖ—76W2F†RföÆB&÷fR—B ¢“° ¢òòF†RVæ6ö×–Æ&ÆR÷6—F–öâÂf÷"6öçG&7C¢f–VÆBFVfVÇBVÖ—G2F†P¢òòæÖRfW&&F–ÒÂ–ç6–FRF†R7G'V7BæB†VBöbF†R6öç7Bà¢ÆWB'•öFVfVÇBÒÖ²‚%F–ç•G†â2„ã¢–çBÒ2’"Â"FVfVÇBâ"Â#r"“°¢76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚f'•öFVfVÇB’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢ÆWBcöFVfVÇBÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f'•öFVfVÇB’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢Æ–æUööb‚gcöFVfVÇBÂ'V–çCcE÷BFrÒã²"¢ÂÆ–æUööb‚gcöFVfVÇBÂ'7FF–26öç7FW‡"–çCcE÷BâÒS²"’À¢'F†Rf–VÆB–æ—F–Æ—¦W"&V6VFW2F†R6öç7BÂ6òF†B÷6—F–öâFöW2æ÷B6ö×–ÆR ¢“°§Ð ¢òòòF†R4•…D‚ÆæF–ærÂæBF†RöæÇ’öæRv†÷6R7W&f6R7–çF‚—2&Và¢òòò&×2†FW7BB„ã¢–çBÒ2–’&F†W"F†â2‚âââ–â'6U÷FW7F ¢òòò66WG2F†VÒÂ6ò—B—2&V6†&ÆS²–×Â‚f÷"F&†&BÖ6öFW2à¢òòòV×G’Æ—7Bà¢5·FW7EÐ¦fâ÷FW7E÷&ÖWFW%ö—5öG&÷VEö'•÷cöÆ–¶UöWfW'•ö÷F†W%÷&ÖWFW%öÆ—7B‚’°¢6öç7B5$3¢g7G"Ò"2$4ôå5@¦FöÖ–â7—4FöÖ–à¢g&WöÖ‡£¢ ¦VæBFöÖ–â7—4FöÖ–à §FW7B&ÕFW7DDT4À¢ÆWBGWB¢F÷  ¢6Æö6²6Æ²Ò7—4FöÖ–à ¢'Và¢GWBç'7BÒU4P¢v—B"7–6ÆW0¢VæB'Và¦VæBFW7B&ÕFW7@¢"3°¢ÆWBÖ²ÒÆ77C¢g7G"ÂFV6Ã¢g7G"ÂW6Uó¢g7G'Â°¢5$2ç&WÆ6R‚$4ôå5EÆâ"Â77B¢ç&WÆ6R‚%&ÕFW7DDT4Â"ÂFV6Â¢ç&WÆ6R‚%U4R"ÂW6Uò¢Ó° ¢òò6†F÷vVC¢F†R&VfW&Væ6R&–æG2FòF†R6öç7BÂæBF†R&ÖWFW"w0¢òò÷vâFVfVÇB2&V6†W2æ÷F†–ærà¢ÆWB6†F÷vVBÒÖ²‚&6öç7BâÒ•ÆåÆâ"Â%&ÕFW7B„ã¢–çBÒ2’"Â$â"“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g6†F÷vVB’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚'FW7B&ÖWFW'2"’Â'¶×6wÒ"“° ¢ÆWBc÷6†F÷vVBÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g6†F÷vVB’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢c÷6†F÷vVBæ6öçF–ç2‚'7FF–26öç7FW‡"–çCcE÷BâÒ“²"¢bbc÷6†F÷vVBæ6öçF–ç2‚&†&5÷'C£¦†&5ö76–vâ†GWBÓç'7BÂâ“²"’À¢'F†R6öç7B—2VÖ—GFVBæBF†R&VfW&Væ6R&–æG2Fò—B ¢“°¢òòF†Ræ6†÷#¢F†R6ÖRFW7Bv—F‚äò&ÖWFW"Æ—7BVÖ—G0¢òò–FVçF–6ÆÇ’Â6òF†R&ÖWFW"—2&÷f&Ç’–çf—6–&ÆRà¢ÆWBæõ÷&ÒÒÖ²‚&6öç7BâÒ•ÆåÆâ"Â%&ÕFW7B"Â$â"“°¢Æ÷vW%÷7&2‚fæõ÷&Ò’æW‡V7B‚'F†R6öç7BÖöæÇ’f÷&ÒÆ÷vW'2"“°¢76W'EöW€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fæõ÷&Ò’’æW‡V7B‚'cVÖ—G2"’À¢c÷6†F÷vVBÀ¢&„ã¢–çBÒ2–6†ævW2æ÷F†–ærÂ6òF†RFW7B'Vç2v—F‚F†R6öç7Bw2’ ¢“° ¢òòVç6†F÷vVC¢æ÷F†–ærFò&–æBFòÂ6òF†R&VfW&Væ6R—2VæFV6Æ&VBà¢ÆWBVç6†F÷vVBÒÖ²‚""Â%&ÕFW7B…t”DS¢–çBÒ2’"Â%t”DR"“°¢76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚gVç6†F÷vVB’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢ÆWBc÷Vç6†F÷vVBÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gVç6†F÷vVB’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢c÷Vç6†F÷vVBæ6öçF–ç2‚&†&5÷'C£¦†&5ö76–vâ†GWBÓç'7BÂt”DR“²"’À¢'F†R&ÖWFW"æÖR7W'f—fW2–çFòF†R76–væÖVçB ¢“°¢76W'B€¢c÷Vç6†F÷vV@¢æÆ–æW2‚¢æç’‡ÆÇÂÂçG&–Õ÷7F'B‚’ç7F'G5÷v—F‚‚"òò"’bbÂæ6öçF–ç2‚&–çCcE÷Bt”DR"’’À¢&æBt”DV—2FV6Æ&VBæ÷v†W&RÂ6ò—BFöW2æ÷B6ö×–ÆR ¢“° ¢òòæBF†R6öçG&öÃ¢v—F†÷WB&ÖWFW"ÂF†R6ÖRFW7BÆ÷vW'2à¢Æ÷vW%÷7&2‚fÖ²‚""Â%&ÕFW7B"Â#"’’æW‡V7B‚'F†RVç&ÖWFW&—¦VBFW7BÆ÷vW'2"“°§Ð ¢òòòF†R4UdTåD‚ÆæF–ærÂæBF†RöæÇ’öæRF†Bv2†öÆR&F†W"F†â¢òòòÖ—6Æ&VÃ¢æ÷F†–ær&V¦V7FVBFW7F&Væ6‚F"2„ã¢–çBÒ2–BÆÂÂ6ð¢òòòD"Ô•"6–ÆVçFÇ’Ö—2ÖÆ÷vW&VB—BW†7FÇ’2cFöW2à¢òòð¢òòò6ö×öæVçDFV6Æ†2FW7F&Væ6†¶–æBÂæB—BW66W2WfW'’÷F†W ¢òòò&ÖWFW"6†V6²(	B6ö×÷6÷W&6W6FÖ—G2—FVÓ£¤VçföæÇ’v†VâF†P¢òòò¶–æB—2VçfÂ6òFW7F&Væ6‚æWfW"&V6†W2F†R6ö×÷6—FR&Òâv—F€¢òòòf–ÆR×66÷R6öç7FFò6†F÷rÂF†R&VfW&Væ6R&÷VæBFòF†R6öç7Bæ@¢òòòF†Rv†öÆR&öw&ÒÆ÷vW&VBÂfW&–f–VBäBVÖ—GFVBâv—F†÷WBöæRÂF†P¢òòòVç&W6öÇfVBÖæÖRF‚Ç&VG’6Vv‡B—BÂv†–6‚—2v‡’öæÇ’†ÆbF†P¢òòò6†RÆV¶VBæBv‡’&ö&–æröæÇ’F†RVç6†F÷vVBf÷&Òv÷VÆB†fP¢òòòf÷VæBæ÷F†–ærà¢5·FW7EÐ¦fâ÷FW7F&Væ6…÷&ÖWFW%÷v5÷6–ÆVçFÇ•öÖ—5öÆ÷vW&VEö'•÷F&—%÷Föò‚’°¢6öç7B5$3¢g7G"Ò"2$4ôå5@¦FöÖ–â7—4FöÖ–à¢g&WöÖ‡£¢ ¦VæBFöÖ–â7—4FöÖ–à §FW7F&Væ6‚F$DT4À¢GWB¢F÷ ¦VæBFW7F&Væ6‚F  ¦–×ÂF%FW7Bf÷"F ¢6Æö6²6Æ²Ò7—4FöÖ–à¢'Và¢GWBç'7BÒU4P¢v—B"7–6ÆW0¢VæB'Và¦VæB–×ÂF%FW7@¢"3°¢ÆWBÖ²ÒÆ77C¢g7G"ÂFV6Ã¢g7G"ÂW6Uó¢g7G'Â°¢5$2ç&WÆ6R‚$4ôå5EÆâ"Â77B¢ç&WÆ6R‚%F$DT4Â"ÂFV6Â¢ç&WÆ6R‚%U4R"ÂW6Uò¢Ó° ¢òòF†R6†RF†BÆV¶VC¢6†F÷vVB'’f–ÆR×66÷R6öç7Bà¢ÆWB6†F÷vVBÒÖ²‚&6öç7BâÒ•ÆåÆâ"Â%F"2„ã¢–çBÒ2’"Â$â"“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g6†F÷vVB’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚'&ÖWFW'2öâFW7F&Væ6‚F&"’Â'¶×6wÒ"“° ¢òòc&–æG2—BFòF†R6öç7BÂæBF†R&ÖWFW"w2÷vâFVfVÇB0¢òò&V6†W2æ÷F†–ær(	BF†R6ÖR6†R2F†R÷F†W"6—‚ÆæF–æw2à¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g6†F÷vVB’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢cæ6öçF–ç2‚'7FF–26öç7FW‡"–çCcE÷BâÒ“²"¢bbcæ6öçF–ç2‚&†&5÷'C£¦†&5ö76–vâ†GWBÓç'7BÂâ“²"’À¢'c&–æG2F†R&VfW&Væ6RFòF†R6öç7B ¢“°¢ÆWBæõ÷&ÒÒÖ²‚&6öç7BâÒ•ÆåÆâ"Â%F""Â$â"“°¢Æ÷vW%÷7&2‚fæõ÷&Ò’æW‡V7B‚'F†R6öç7BÖöæÇ’f÷&ÒÆ÷vW'2"“°¢76W'EöW€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fæõ÷&Ò’’æW‡V7B‚'cVÖ—G2"’À¢cÀ¢&2„ã¢–çBÒ2–6†ævW2æ÷F†–ærVæFW"cV—F†W" ¢“° ¢òòF†R†ÆbF†BæWfW"ÆV¶VC¢v—F‚æ÷F†–ærFò6†F÷rÂF†P¢òòVç&W6öÇfVBÖæÖRF‚6F6†W2—B&Vf÷&RF†—2&Ò6âà¢ÆWBVç6†F÷vVBÒÖ²‚""Â%F"2…t”DS¢–çBÒ2’"Â%t”DR"“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚gVç6†F÷vVB’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚'&ÖWFW'2öâFW7F&Væ6‚F&"’Â'¶×6wÒ"“° ¢òòæBF†R6öçG&öÃ¢v—F†÷WB&ÖWFW"Æ—7BÂF†RFW7F&Væ6‚Æ÷vW'2à¢Æ÷vW%÷7&2‚fÖ²‚""Â%F""Â#"’’æW‡V7B‚'F†RVç&ÖWFW&—¦VBFW7F&Væ6‚Æ÷vW'2"“°§Ð ¢òòòF†RæÖVBÖ&wVÖVçB6öç7G'V7B†2F†R6ÖR6†R2F†R&ÖWFW"öæS ¢òòò&×2F†B&W÷'B—BÂæB6—FW2F†B6–ÆVçFÇ’Dò—Bâ'W2ç'6æ@¢òòò&Vv&Æö6²ç'6V6‚6'&–VB6ÆÅö&v†VÇW"F†BFöö²fÇVVæ@¢òòòG&÷VBæÖVÂ6òD"Ô•"—G6VÆb&÷VæB&V÷&FW&VBæÖVB&wVÖVçG2'¢òòò÷6—F–öâ(	B'W2çrç6VæB‡7G&"ÒRÂFFÒBçfÇVR–VÖ—GFV@¢òòò†–Å÷uöFFÒVæB†–Å÷u÷7G&"ÒBçfÇVVà¢òòð¢òòòF†RwV&B6†V6·2æÖW2v–ç7BF†RDT4Ä$D”ôâ&F†W"F†â6÷VçF–æp¢òòò&wVÖVçG2â—G2f—'7BfW'6–öâ¶W–VBöâ&—G’ÆöæRæB6ò&VgW6V@¢òòò'W2çrç6VæB†FFÒBçfÇVRÂ7G&"ÒR–(	BæÖW2–âFV6Æ&F–öâ÷&FW"À¢òòòv†–6‚&÷F‚&6¶VæG2Æ÷vW"6÷'&V7FÇ’(	Bv†–ÆRFVÆÆ–ærF†RW6W"c¢òòò'6–ÆVçFÇ’VÖ—G26öÖWF†–ærVÇ6R"âWfW'’6ÆÆW"†W&R†2F†RFV6Æ&V@¢òòòæÖW2–â†æBÂ6ò&÷F‚†ÇfW2&R–ææVB&VÆ÷rà¢5·FW7EÐ¦fâF&—%ö&–æG5öæÖVEö&wVÖVçG5ö'•öæÖUö÷%÷&VgW6W5÷Fõö&–æE÷F†VÒ‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&†–Æ—FUö&÷VæEöÖöå÷FW7Bæ†&2"“°¢6öç7B4ÄÃ¢g7G"Ò&'W2çrç6VæB‡BçfÇVRÂR’#°¢76W'B†f—‡GW&Ræ6öçF–ç2„4ÄÂ’Â&f—‡GW&R6†R6†ævVB"“°¢ÆWB6ÆÂÒÆ&w3¢g7G'Âf—‡GW&Rç&WÆ6Vâ„4ÄÂÂff÷&ÖB‚&'W2çrç6VæB‡¶&w7Ò’"’Â“° ¢ÆWBv—F…ö'W2ÒÇ7&3¢g7G'Â°¢ÆWBbÒ'6U÷6÷W&6R‡7&2’æW‡V7B‚''6W2"“°¢ÆWB'W5÷7&2Ò7FC£¦g3£§&VE÷Fõ÷7G&–ær€¢Fƒ£¦æWr†Vçb‚$4$tõôÔä”dU5EôD•""’’æ¦ö–â‚'7FFÆ–"ô'W4†”Æ—FRæ&6‚"’À¢¢æW‡V7B‚'7FFÆ–"'W2&VF&ÆR"“°¢ÆWB"Ò'6U÷6÷W&6R‚f'W5÷7&2’æW‡V7B‚&'W2'6W2"“°¢ÖW&vS£¦ÖW&vUöf÷%÷6–Ò‡fV2¶bÂ%ÒÂæöæR’æW‡V7B‚&ÖW&vR"¢Ó°¢ÆWBVÖ—E÷F&—"ÒÇ7&3¢g7G'Â°¢ÆWBÒÒv—F…ö'W2‡7&2“°¢ÆWBÒÆ÷vW#£¦Æ÷vW%÷&öw&Ò‚fÒ“ó°¢fW&–g“£§fW&–g•÷&öw&Ò‚g’æW‡V7B‚'fW&–f–W2"“°¢ö³££ÅòÂÆ÷vW#£¤Æ÷vW$W'&÷#â‡F&—#£¦VÖ—B‚gÂfÒÂf7÷F#£¤VÖ—D÷G3£¦FVfVÇB‚’’æW‡V7B‚&VÖ—G2"’¢Ó° ¢òòF†R6öçG&öÂ&–æG2–âFV6Æ&F–öâ÷&FW"à¢ÆWB7FÂÒVÖ—E÷F&—"‚ff—‡GW&R’æW‡V7B‚'F†R÷6—F–öæÂf÷&ÒÆ÷vW'2"“°¢76W'B€¢7FÂæ6öçF–ç2‚&†&5÷'C£¦†&5ö76–vâ†GWBÓæ†–Å÷uöFFÂBçfÇVR“²"¢bb7FÂæ6öçF–ç2‚&†&5÷'C£¦†&5ö76–vâ†GWBÓæ†–Å÷u÷7G&"ÂR“²"’À¢&6öçG&öÂ&–æG2FFF†Vâ7G&" ¢“° ¢òòæÖW2–âDT4Ä$D”ôâõ$DU"7F–ÆÂÆ÷vW"ÂæB&–æBW†7FÇ’2F†P¢òò÷6—F–öæÂf÷&ÒFöW2(	BF†—2—2F†R†ÆbF†R&—G’ÖöæÇ’wV&@¢òò'&ö¶Râ„æ÷Bv†öÆRÖf–ÆR–FVçF—G“¢FF–ærFFÒ6†–gG26÷W&6P¢òòöfg6WG2Âv†–6‚V"–âvVæW&FVB7–Ö&öÂæÖW2â¢ÆWB–åö÷&FW"ÒVÖ—E÷F&—"‚f6ÆÂ‚&FFÒBçfÇVRÂ7G&"ÒR"’’æW‡V7B‚&–âÖ÷&FW"æÖW2Æ÷vW""“°¢76W'B€¢–åö÷&FW"æ6öçF–ç2‚&†&5÷'C£¦†&5ö76–vâ†GWBÓæ†–Å÷uöFFÂBçfÇVR“²"¢bb–åö÷&FW"æ6öçF–ç2‚&†&5÷'C£¦†&5ö76–vâ†GWBÓæ†–Å÷u÷7G&"ÂR“²"’À¢&æÖW2w&—GFVâv†W&RF†W’&VÆöær&–æBv†W&RF†W’&VÆöær ¢“° ¢òò$Tõ$DU$TBæÖW2&R&VgW6VBÂæBF†RÖW76vR6—2v†–6‚&ÖWFW ¢òòv2w&—GFVâv†W&R&F†W"F†â§W7BF†BæÖW2vW&RW6VBà¢ÆWBW'"ÒVÖ—E÷F&—"‚f6ÆÂ‚'7G&"ÒRÂFFÒBçfÇVR"’¢æW'"‚¢æW‡V7B‚&&V÷&FW&VB6ÆÂ×W7Bæ÷BÆ÷vW"6–ÆVçFÇ’"“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB‚fW'"ÂÆ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2“°¢76W'B†×6ræ6öçF–ç2‚&'W2çrç6VæB‚âââ––ÆöB"’Â'¶×6wÒ"“°¢76W'B€¢×6ræ6öçF–ç2‚&7G&&—2&ÖWFW""†W&R'WBv2w&—GFVâ–â÷6—F–öâ"’À¢'F†RÖW76vR×W7BæÖRF†RÖ—7Æ6VÖVçC¢¶×6wÒ ¢“° ¢òòÖ—†VB÷6—F–öæÂöæÖVB—26†V6¶VB'’÷6—F–öâFöó¢7G&&6—G2–à¢òò÷6—F–öâ"Âv†W&R—B&VÆöæw2Â6òF†—2Æ÷vW'2à¢ÆWBÖ—†VBÒVÖ—E÷F&—"‚f6ÆÂ‚'BçfÇVRÂ7G&"ÒR"’’æW‡V7B‚&6÷'&V7FÇ’×Æ6VBæÖRÆ÷vW'2"“°¢76W'B€¢Ö—†VBæ6öçF–ç2‚&†&5÷'C£¦†&5ö76–vâ†GWBÓæ†–Å÷u÷7G&"ÂR“²"’À¢&æB&–æG2v†W&RF†RæÖR6—2 ¢“° ¢òòæÖRÖF6†–æräò&ÖWFW"—2–çfÆ–FÂæ÷B7V'6WBv¢æð¢òò&6¶VæB6â†öæ÷W"—BÂæBf÷"G—ò–âfÆ–B÷6—F–öâcVÖ—G0¢òòW†7FÇ’F†R&–v‡B6öFRÂ6ò6Æ–Ö–ær—BÖ—2ÖÆ÷vW'2v÷VÆB&RF†P¢òò6ÖRfÇ6RW‡ÆæF–öâF†—2wV&Bv2&Ww&—GFVâFò7F÷Ö¶–ærà¢ÆWB×6rÒ76W'Eö–çfÆ–B€¢fVÖ—E÷F&—"‚f6ÆÂ‚&æ÷7V6‚ÒBçfÇVRÂ7G&"ÒR"’¢æW'"‚¢æW‡V7B‚&âVæ¶æ÷vâ&ÖWFW"æÖR×W7Bæ÷BÆ÷vW"6–ÆVçFÇ’"’À¢“°¢76W'B€¢×6ræ6öçF–ç2‚&æÖW2æò&ÖWFW""’bb×6ræ6öçF–ç2‚&FF"’À¢'F†RÖW76vR×W7BÆ—7BF†R&VÂ&ÖWFW"æÖW3¢¶×6wÒ ¢“° ¢òò„6–ævÆR&wVÖVçB6ææ÷B&RÔ•5Ä4TBÂ6òF†R7v†ÆböbF†P¢òòwV&B—2æòÖ÷F†W&R(	B'WB—G2Væ¶æ÷vâÖæÖR†Æb7F–ÆÂf—&W2À¢òòv†–6‚WfW'•öæÖVEö&wVÖVçEö6ÆÅ÷6—FUö—5öwV&FVFW†W&6—6W2öà¢òòf÷&²ÖVÒç&VB†æ÷7V6‚ÒR–âF†RöæRÖ&wVÖVçB7v7W&f6R6ææ÷@¢òò&R&V6†VBöâF†—26†ææVÂBÆÃ¢â&—G’6†V6²&÷fR&V¦V7G2¢òòöæRÖ&wVÖVçB'W2çrç6VæF÷WG&–v‡Bâ§Ð ¢òòòW"×6—FR–ç2f÷"F†R÷F†W"F‡&VR&V¦V7EöÖ—7Æ6VEöæÖVEö&w6 ¢òòò6ÆÆW'2âF†RwV&Bw26ö×&—6öâÆöv–2v2Ç&VG’–ææVB'¢òòòF&—%ö&–æG5öæÖVEö&wVÖVçG5ö'•öæÖUö÷%÷&VgW6W5÷Fõö&–æE÷F†VÖÂ'WB—G0¢òòò4ÄÂ4•DU2vW&Ræ÷C¢F‡&VRöbf÷W"6÷VÆB&RFVÆWFVBv—F‚F†R7V—FP¢òòò7F–ÆÂw&VVâÂv†–6‚—2†÷r&V6÷&E÷w&—FV6†—VBv—F‚â–çfVçFV@¢òòò&ÖWFW"Æ—7B†²'&Vr"Â'fÇVR%Öf÷"'V–ÇF–âv†÷6R&VÂ6–væGW&P¢òòò—2†FG"ÂFF–(	BæB6–æ6R&Vv—2ÆW†W"¶W—v÷&BÂ÷6—F–öâ¢òòò6÷VÆBæWfW"ÖF6‚Â6òF†R6—FRFVvVæW&FVB–çFò'&VgW6RWfW'’æÖV@¢òòòf—'7B&wVÖVçB"Â–æ6ÇVF–ærF†RFö7VÖVçFVBf÷&Ò’à¢5·FW7EÐ¦fâWfW'•öæÖVEö&wVÖVçEö6ÆÅ÷6—FUö—5öwV&FVB‚’°¢òò)H)H&V6÷&E÷w&—FR†FG"ÂFF–¢F†RFö7VÖVçFVBæÖVBf÷&Ò×W7@¢òòÆ÷vW"ÂæBöæÇ’vVçV–æR7vÖ’&R&VgW6VBà¢ÆWB&Vw2Òf—‡GW&R‚'&Vv&Æö6µ÷&V6÷&Eö•÷FW7Bæ†&2"“°¢6öç7Bu$•DS¢g7G"Ò'&Vw2ç&V6÷&E÷w&—FRƒƒ‚Â3SC“ƒ“b’#°¢76W'B‡&Vw2æ6öçF–ç2…u$•DR’Â&f—‡GW&R6†R6†ævVB"“°¢òòF†—2f—‡GW&RW6V27FFÆ–"'W2Âv†–6‚F†RVæ—B×FW7B†&æW72FöW0¢òòæ÷B&W6öÇfRöâ—G2÷vâà¢ÆWBÆ÷vW%÷v—F…ö'W2ÒÇ7&3¢g7G'Â°¢ÆWBbÒ'6U÷6÷W&6R‡7&2’æW‡V7B‚''6W2"“°¢ÆWB'W5÷7&2Ò7FC£¦g3£§&VE÷Fõ÷7G&–ær€¢Fƒ£¦æWr†Vçb‚$4$tõôÔä”dU5EôD•""’’æ¦ö–â‚'7FFÆ–"ô'W4†”Æ—FRæ&6‚"’À¢¢æW‡V7B‚'7FFÆ–"'W2&VF&ÆR"“°¢ÆWB"Ò'6U÷6÷W&6R‚f'W5÷7&2’æW‡V7B‚&'W2'6W2"“°¢Æ÷vW#£¦Æ÷vW%÷&öw&Ò‚fÖW&vS£¦ÖW&vUöf÷%÷6–Ò‡fV2¶bÂ%ÒÂæöæR’æW‡V7B‚&ÖW&vR"’¢Ó°¢ÆWBrÒÆ&w3¢g7G'Â&Vw2ç&WÆ6Vâ…u$•DRÂff÷&ÖB‚'&Vw2ç&V6÷&E÷w&—FR‡¶&w7Ò’"’Â“° ¢Æ÷vW%÷v—F…ö'W2‚g&Vw2’æW‡V7B‚'F†Rf—‡GW&R—G6VÆbÆ÷vW'2"“°¢Æ÷vW%÷v—F…ö'W2‚gr‚&FG"Òƒ‚ÂFFÒ3SC“ƒ“b"’¢æW‡V7B‚'F†RFö7VÖVçFVBæÖVBf÷&Ò×W7BÆ÷vW""“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷v—F…ö'W2‚gr‚&FFÒ3SC“ƒ“bÂFG"Òƒ‚"’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚&&V6÷&E÷w&—FR‚âââ–6ÆÂ"’Â'¶×6wÒ"“°¢76W'B€¢×6ræ6öçF–ç2‚&FF—2&ÖWFW""†W&R'WBv2w&—GFVâ–â÷6—F–öâ"’À¢'F†RÖW76vR×W7BW6RF†R$TÂ&ÖWFW"æÖW3¢¶×6wÒ ¢“°¢òòæBâ–çfVçFVBæÖR—2&W÷'FVBv–ç7BF†R&VÂÆ—7Bâæ÷FRF†P¢òò–çfVçFVBÆ—7B6÷VÆBæWfW"†fRÖF6†VB÷6—F–öâBÆÃ¢&Vv—0¢òòÆW†W"¶W—v÷&BæBFöW2æ÷BWfVâ'6R2â&wVÖVçBæÖRÂ6òF†P¢òò6—FR6–ÆVçFÇ’FVvVæW&FVB–çFò'&VgW6RWfW'’æÖVBf—'7@¢òò&wVÖVçB"(	B–æ6ÇVF–ærF†RFö7VÖVçFVBf÷&Ò76W'FVB&÷fRà¢76W'B€¢'6U÷6÷W&6R‚gr‚'&VrÒƒ‚ÂfÇVRÒ2"’’æ—5öW'"‚’À¢&&Vv—2¶W—v÷&BÂ6òF†R–çfVçFVBÆ—7Bv2VæÖF6†&ÆR ¢“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷v—F…ö'W2‚gr‚&æ÷7V6‚Òƒ‚ÂFFÒ2"’’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚&FG&"’bb×6ræ6öçF–ç2‚&FF"’À¢'F†RF–væ÷7F–2×W7BV÷FRF†R&VÂ6–væGW&S¢¶×6wÒ ¢“° ¢òò)H)HF†R$Äô4´”ärFÆÕöÖWF†öF6—FRÂv†÷6RFV6Æ&VBæÖW26öÖRg&öÐ¢òòÒæ&w6&F†W"F†â6†ææVÂ–ÆöBà¢ÆWBÖVÒÒf—‡GW&R‚&×6u÷7W7VæF–æuö6ÆÅ÷FW7Bæ†&2"“°¢6öç7Bô´S¢g7G"Ò&ÖVÒçö¶Rƒ‚Â#cB’#°¢76W'B†ÖVÒæ6öçF–ç2…ô´R’Â'ö¶Rf—‡GW&R6†R6†ævVB"“°¢ÆWBÒÆ&w3¢g7G'ÂÖVÒç&WÆ6Vâ…ô´RÂff÷&ÖB‚&ÖVÒçö¶R‡¶&w7Ò’"’Â“° ¢Æ÷vW%÷7&2‚fÖVÒ’æW‡V7B‚'F†Rö¶Rf—‡GW&RÆ÷vW'2"“°¢Æ÷vW%÷7&2‚g‚&FG"Ò‚ÂFFÒ#cB"’’æW‡V7B‚'F†R–âÖ÷&FW"æÖVBf÷&Ò×W7BÆ÷vW""“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g‚&FFÒ#cBÂFG"Ò‚"’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚&'W2çö¶R‚âââ–6ÆÂ"’Â'¶×6wÒ"“°¢76W'B€¢×6ræ6öçF–ç2‚&FF—2&ÖWFW""†W&R'WBv2w&—GFVâ–â÷6—F–öâ"’À¢'F†RÖW76vR×W7BW6RF†RFV6Æ&VBæÖW3¢¶×6wÒ ¢“° ¢òòF†Rdõ$´TBFÆÕöÖWF†öF6—FR6†&W2Òæ&w6'WB—26W&FP¢òò6ÆÂÂ6ò—BvWG2—G2÷vâ–ââf÷&¶æVVG2&WGW&æ–ærÖWF†öBà¢ÆWBf÷&¶VBÒf—‡GW&R‚'FÆÕöÖWF†öEö&Æö6¶–æuöf÷&µö'W5÷FW7Bæ†&2"“°¢6öç7Bdõ$³¢g7G"Ò&f÷&²ÖVÒç&VBƒR’#°¢76W'B†f÷&¶VBæ6öçF–ç2„dõ$²’Â&f÷&²f—‡GW&R6†R6†ævVB"“°¢Æ÷vW%÷7&2‚ff÷&¶VB’æW‡V7B‚'F†Rf÷&²f—‡GW&RÆ÷vW'2"“°¢òòöæR&wVÖVçB6ææ÷B&RÖ—7Æ6VBÂ6òæÖR—Bw&öævÇ’Fò&V6‚F†P¢òòwV&BBÆÂà¢ÆWB×6rÒ76W'Eö–çfÆ–B€¢fÆ÷vW%÷7&2‚ff÷&¶VBç&WÆ6Vâ„dõ$²Â&f÷&²ÖVÒç&VB†æ÷7V6‚ÒR’"Â’’çVçw&öW'"‚’À¢“°¢76W'B†×6ræ6öçF–ç2‚&æÖW2æò&ÖWFW""’Â'¶×6wÒ"“°§Ð ¢òòòÆövöÆövf&VBF†R6WfW&—G’æBÖW76vRõ4•D”ôäÄÅ’ÂÖF6†–æp¢òòò6ÆÄ&s£¤W‡&öæÇ’(	BæB6òFöW2câæÖVB&wVÖVçBF†W&Vf÷&P¢òòò†–FW2v†FWfW"—Bw&2ÂVæFW"&÷F‚&6¶VæG3 ¢òòð¢òòòÆör†ÆWfVÂÒfFÂÂ$$ôôÒ"–(i"6–ÕöÆöuöÆ–æR‚$”ädò"Â$$ôôÒ"– ¢òòòÆör†fFÂÂ×6rÒ$$ôôÒ"–(i"6–ÕöÆöuöÆ–æR‚$dDÂ"Â""– ¢òòð¢òòòF†Rf—'7B—2F†RFævW&÷W2öæRæB—2v‡’F†—2ÆVB—G2&F6ƒ¢¢òòòfFÆ6–ÆVçFÇ’&V6öÖW2â–æföÂ6òæ÷F†–ær'V×2F†Rf–ÇW&P¢òòò6÷VçFW"æBFW7BF†B6†÷VÆB&÷'B76W2w&VVââF†R6WfW&—G’wV&@¢òòò&VÆ÷r—B&V¦V7G2E•òf÷"W†7FÇ’F†—2&V6öâÂæBæÖVB6WfW&—G¢òòòvÆ¶VB7BF†BwV&Bà¢òòð¢òòòvFVBöâv†BF†RæÖR„”DU2Âæ÷BöâæÖVBÖæW73¢â&wVÖVçBF†P¢òòòW‡G&7F÷'2v÷VÆBæWfW"†fRÆöö¶VBB—2†&ÖÆW72VæFW"&÷F€¢òòò&6¶VæG2ÂæB&VgW6–ær—Bv÷VÆB&R&VgW6–ær6÷'&V7B&öw&Òà¢5·FW7EÐ¦fâöæÖVEöÆöuö&wVÖVçE÷F†Eö†–FW5ö÷6WfW&—G•ö÷%öÖW76vUö—5÷&VgW6VB‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&vVçE÷W&–öF–5÷FW7Bæ†&2"“°¢6öç7BÄôs¢g7G"Ò"2&Æör†–æfòÂ%53¢W&–öF–2†æFÆW"f—&VBG·F²æ&VG7ÒF–ÖW2"’"3°¢76W'B†f—‡GW&Ræ6öçF–ç2„Äôr’Â&f—‡GW&R6†R6†ævVB"“°¢ÆWBÆörÒÆ&w3¢g7G'Âf—‡GW&Rç&WÆ6Vâ„ÄôrÂff÷&ÖB‚&Æör‡¶&w7Ò’"’Â“° ¢òòF†R6öçG&öÂÂæBF†Ræ6†÷#¢÷6—F–öæÂfFÆ&VÆÇ’FöW0¢òò&V6‚F†RVÖ—GFVB6WfW&—G’Â6òÆFW"”ädöÖVç2—Bv2Æ÷7Bà¢ÆWBcö7FÂÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fÆör‡"2&fFÂÂ$$ôôÒ""2’’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢cö7FÂæ6öçF–ç2‡"2'6–ÕöÆöuöÆ–æR‚$dDÂ"Â$$ôôÒ"“²"2’À¢'F†R÷6—F–öæÂf÷&Ò6'&–W2&÷F‚6WfW&—G’æBÖW76vR ¢“°¢Æ÷vW%÷7&2‚fÆör‡"2&fFÂÂ$$ôôÒ""2’’æW‡V7B‚'F†R÷6—F–öæÂf÷&ÒÆ÷vW'2"“° ¢òòæÖVB4UdU$•E’—2&VgW6VB(	BæBcF÷væw&FW2—BFò”ädòà¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚fÆör‡"2&ÆWfVÂÒfFÂÂ$$ôôÒ""2’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚&ÆWfVÆ6''––ær6WfW&—G’"’Â'¶×6wÒ"“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fÆör‡"2&ÆWfVÂÒfFÂÂ$$ôôÒ""2’’¢æW‡V7B‚'cVÖ—G2"¢æ6öçF–ç2‡"2'6–ÕöÆöuöÆ–æR‚$”ädò"Â$$ôôÒ"“²"2’À¢'c6–ÆVçFÇ’F÷væw&FW2F†R6WfW&—G’Fò”ädò ¢“° ¢òòæÖVBÔU54tR—2&VgW6VB(	BæBcV×F–W2F†RÖW76vRà¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚fÆör‡"2&fFÂÂ×6rÒ$$ôôÒ""2’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚&×6v6''––ærF†RÖW76vR"’Â'¶×6wÒ"“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fÆör‡"2&fFÂÂ×6rÒ$$ôôÒ""2’’¢æW‡V7B‚'cVÖ—G2"¢æ6öçF–ç2‡"2'6–ÕöÆöuöÆ–æR‚$dDÂ"Â""“²"2’À¢'c6–ÆVçFÇ’V×F–W2F†RÖW76vR ¢“° ¢òòæÖVB&wVÖVçBv†÷6R÷6—F–öæÂ6Æ÷B—2Ç&VG’d”ÄÄTB—0¢òò–æW'B(	BF†RW‡G&7F÷'2F¶RF†Rf—'7B÷6—F–öæÂÖF6‚Â6òF†P¢òòæÖR6÷7G2F†RW6W"æ÷F†–æræB&÷F‚&6¶VæG2VÖ—BF†R&–v‡@¢òòÆ–æRâF†—2—2F†R†ÆbGvò7V66W76—fRfW'6–öç2öbF†RvFRv÷@¢òòw&öæs¢f—'7B'’–væ÷&–æræÖVBÖæW72VçF—&VÇ’ÂF†Vâ'’¶W––æröà¢òòF†RæÖVBdÅTRv†–ÆR–væ÷&–ærv†WF†W"F†R6Æ÷Bv2F¶Vâà¢f÷"†&w2ÂW‡V7FVB’–â°¢€¢"2&fFÂÂ$$ôôÒ"ÂW‡G&Ò"2À¢"2'6–ÕöÆöuöÆ–æR‚$dDÂ"Â$$ôôÒ"“²"2À¢’À¢€¢"2&æ÷7V6‚ÒÂfFÂÂ$$ôôÒ""2À¢"2'6–ÕöÆöuöÆ–æR‚$dDÂ"Â$$ôôÒ"“²"2À¢’À¢òòæÖVB7G&–ærÆöæw6–FR÷6—F–öæÂÖW76vRà¢€¢"2&fFÂÂ$$ôôÒ"ÂW‡G&Ò&æ÷FR""2À¢"2'6–ÕöÆöuöÆ–æR‚$dDÂ"Â$$ôôÒ"“²"2À¢’À¢òòæÖVB6WfW&—G’Æöæw6–FR÷6—F–öæÂöæRà¢€¢"2&fFÂÂ$$ôôÒ"ÂÇfÂÒv&â"2À¢"2'6–ÕöÆöuöÆ–æR‚$dDÂ"Â$$ôôÒ"“²"2À¢’À¢òòæÖVB6WfW&—G’d•%5B(	BF†Rõ4•D”ôäÂöæR7F–ÆÂv–ç2ÂVæFW ¢òò&÷F‚&6¶VæG2Â6òF†R&öw&Ò—266WFVBæBVÖ—G2U%$õ&à¢òòÖ&–wV÷W26÷W&6RÂ'WBæ÷B6öÖWF†–ærV—F†W"&6¶VæBvWG2w&öæp¢òò&VÆF—fRFòF†R÷F†W"Âv†–6‚—2v†BF†—27vVW6Æ76–f–W2à¢€¢"2&ÆWfVÂÒfFÂÂW'&÷"Â$$ôôÒ""2À¢"2'6–ÕöÆöuöÆ–æR‚$U%$õ""Â$$ôôÒ"“²"2À¢’À¢Ò°¢Æ÷vW%÷7&2‚fÆör†&w2’’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚&Æör‡¶&w7Ò–×W7BÆ÷vW#¢¶S£÷Ò"’“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fÆör†&w2’’¢æW‡V7B‚'cVÖ—G2"¢æ6öçF–ç2†W‡V7FVB’À¢&Æör‡¶&w7Ò–×W7BVÖ—B¶W‡V7FVGÖVæFW"cFöò ¢“°¢Ð ¢òòÆövf6†&W2F†RW‡G&7F÷'2ÂæB—G2D‚Æ—FW&Â×W7Bæ÷B&P¢òòÖ—7F¶Vâf÷"†–FFVâÖW76vRà¢ÆWBÆövbÒÆ&w3¢g7G'Âf—‡GW&Rç&WÆ6Vâ„ÄôrÂff÷&ÖB‚&Æövb‡¶&w7Ò’"’Â“°¢Æ÷vW%÷7&2‚fÆövb‡"2"'BæÆör"ÂW'&÷"Â$$ôôÒ""2’’æW‡V7B‚'F†R÷6—F–öæÂÆövbf÷&ÒÆ÷vW'2"“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚fÆövb‡"2"'BæÆör"ÂÆWfVÂÒW'&÷"Â$$ôôÒ""2’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚&6''––ær6WfW&—G’"’Â'¶×6wÒ"“°¢òò(
fæBæÖW2F†R6öç7G'V7BF†RW6W"7GVÆÇ’w&÷FRà¢76W'B†×6ræ6öçF–ç2‚&–âÆövf"’Â'¶×6wÒ"“° ¢òòæÖVB&wVÖVçB†VBöbF†RÆövbD‚—2–æW'Bv†Vâ$õD‚7G&–æp¢òò6Æ÷G2&Rf–ÆÆVB÷6—F–öæÆÇ’à¢Æ÷vW%÷7&2‚fÆövb‡"2'Ò&æÆör"Â'BæÆör"ÂW'&÷"Â$$ôôÒ""2’¢æW‡V7B‚&æÖVB&r&W6–FRGvò÷6—F–öæÂ7G&–æw2×W7BÆ÷vW""“° ¢òò'WBÆövfæVVG2Etò7G&–æw2ÂæBÖöFVÆÆ–æröæÇ’F†RÖW76vR6Æ÷@¢òòÖ—76VBF†C¢v—F‚öæR÷6—F–öæÂ7G&–ærÆVgBÂæÖVBF€¢òò&öÖ÷FW2F†RÔU54tRFòf–ÆVæÖR(	BF†—2&öw&ÒW6VBFòw&—FRFð¢òòf–ÆRÆ—FW&ÆÇ’6ÆÆVB$ôôÖà¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚fÆövb‡"2'F‚Ò'BæÆör"ÂW'&÷"Â$$ôôÒ""2’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚&6''––ærF‚÷"ÖW76vR"’Â'¶×6wÒ"“°¢òòæBæÖVBÖW76vRv—F‚öæÇ’F†RF‚÷6—F–öæÂà¢76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚fÆövb‡"2"'BæÆör"ÂW'&÷"Â×6rÒ$$ôôÒ""2’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“° ¢òòæÖVB–FVçBF†B—2äõB6WfW&—G’†–FW2æ÷F†–æs¢÷6—F–öæÆÇ¢òò—Bv÷VÆB†fR&VVâ&V¦V7FVB'’F†R6WfW&—G’wV&BÂæ÷B6–ÆVçFÇ¢òòW6VBÂ6ò6Æ–Ö–ærÆ÷72v÷VÆB&R6Æ–Ö–æröæRF†B6ææ÷B†Vâà¢Æ÷vW%÷7&2‚fÆör‡"2"$$ôôÒ"Âv†òÒæ÷7V6†–FVçB"2’’æW‡V7B‚&æÖVBæöâ×6WfW&—G’–FVçB×W7BÆ÷vW""“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fÆör‡"2"$$ôôÒ"Âv†òÒæ÷7V6†–FVçB"2’’¢æW‡V7B‚'cVÖ—G2"¢æ6öçF–ç2‡"2'6–ÕöÆöuöÆ–æR‚$”ädò"Â$$ôôÒ"“²"2’À¢&æBcVÖ—G2F†R6÷'&V7BFVfVÇB×6WfW&—G’Æ–æR ¢“° ¢òòF†RFWF–Â×W7Bæ÷B6''’F†R'Vç2öbÆ—FW&Â76W2†&B×w&V@¢òò7G&–ærÆ—FW&Âv—F†÷WBÆ6öçF–çVF–öâ&öGV6W2à¢76W'B€¢×6ræ6öçF–ç2‚""’À¢&FWF–Â†26öÆÆ6VBv†—FW76S¢¶×6wÒ ¢“°§Ð ¢òòò6öç7G&–çBÖÆ÷vW&–ærF–væ÷7F–72W6VBFò&RF‡&÷vâv’v†öÆW6ÆRà¢òòò'V–ÆE÷G—VE÷6öÇfW%÷&ö&ÆVÕ÷F&ÆV&V6÷&G2F†VÒ0¢òòòG—VE6öÇfW%&ö&ÆVÔ'V–ÆC£¤Æ÷vW$W'&÷&VçG&–W2ÂæBÆ÷vW%÷&öw&Öw0¢òòò6öç7VÖ–ærÆö÷&VBöæÇ’£6VçG&–W2(	B6ð¢òòò&æFöÖ—¦R‡"’v—F‚æõ7V6…&VÆF–öâ‡"–Æ÷vW&VB6ÆVâVæFW"D"Ô• ¢òòòv†–ÆRc&VgW6VB—B÷WG&–v‡Bà¢òòð¢òòòöæÇ’F†R$TÄD”ôâW'&÷'27W&f6RÂæBF†B7Æ—Bv2ÖV7W&VB&F†W ¢òòòF†â&V6öæVC¢ÆÂ“æ†&6f–ÆW2–âFW7G2öf—‡GW&W6vW&R'Và¢òòòF‡&÷Vv‚F†RF&ÆR'V–ÆFW"ƒƒBÖW&vR’âGvò&öGV6Ræöâ×&VÆF–öà¢òòòÆ÷vW$W'&÷&æB¤U$ò&öGV6R&VÆF–öâöæR(	B6òF†R7W&f6VB&×0¢òòò'&V²æ÷F†–ær–âF†R6÷'W2Âv†–ÆR7W&f6–ærWfW'’f&–çBv÷VÆ@¢òòò&V¦V7BV–çCcE÷Væ—VU÷&æFöÖ—¦U÷FW7FÂv†÷6R2ç6×ÆU³c3£3%ÒÒ ¢òòòG&—2F—6ÆÆ÷vVD–ä6öç7G&–çFâF†B—26&–Æ—G’v–âF†P¢òòò6öç7G&–çB•"Âæ÷B&B&öw&Ó¢&÷F‚&6¶VæG2Æ÷vW"—BæB—@¢òòò76W2G&6RWV—fÆVæ6Rà¢òòð¢òòò…F†R÷F†W"æöâ×&VÆF–öâf—‡GW&RÂ†•övVçFÂ—2&V¦V7FVB'’$õD€¢òòò&6¶VæG2f÷"âVç&VÆFVB6÷fW&w&÷W&V6öâÂ6òöæÇ’F†Rf—'7@¢òòò7GVÆÇ’7W÷'G2F†RF—66&BFV6—6–öâââV&Æ–W"w&—FR×W6–B'c¢òòòÆ÷vW'2&÷F‚"Âv†–6‚v2fÇ6Râ¢5·FW7EÐ¦fâ&VÆF–öåöW'&÷'5ö–å÷&æFöÖ—¦U÷v—F…öæ÷u÷&V6…÷F†U÷W6W"‚’°¢ÆWB7&2Òf—‡GW&R‚'&VÆF–öåö–æÆ–æ–æu÷FW7Bæ†&2"“°¢6öç7BÄ”3¢g7G"Ò'&VÆF–öâ†–v„FG"‡#¢&W’Ò"æFG"ãÒƒ#°¢6öç7B4ÄÃ¢g7G"Ò'&æFöÖ—¦R‡"’v—F‚&÷VæFVDæD†–v‚‡"’VæB&æFöÖ—¦R#°¢76W'B€¢7&2æ6öçF–ç2„Ä”2’bb7&2æ6öçF–ç2„4ÄÂ’À¢&f—‡GW&R6†R6†ævVB ¢“°¢ÆWBGvòÒf÷&ÖB€¢'´Ä”7ÕÆåÆç&VÆF–öâ&WGvVVâ‡#¢&WÂÆó¢–çBÂ†“¢–çB’Ò"æFG"ãÒÆòbb"æFG"ÃÒ†’ ¢“°¢ÆWBv—F‚ÒÆ6ÆÃ¢g7G'Â°¢7&2ç&WÆ6Vâ„Ä”2ÂgGvòÂ’ç&WÆ6Vâ€¢4ÄÂÀ¢ff÷&ÖB‚'&æFöÖ—¦R‡"’v—F‚¶6ÆÇÒVæB&æFöÖ—¦R"’À¢À¢¢Ó° ¢òòF†R6öçG&öÃ¢6÷'&V7B6ÆÂ7F–ÆÂÆ÷vW'2à¢Æ÷vW%÷7&2‚gv—F‚‚$&WGvVVâ‡"ÂcSS3bÂ3s"’"’’æW‡V7B‚'F†R6÷'&V7B6ÆÂÆ÷vW'2"“° ¢f÷"†6ÆÂÂæVVFÆR’–â°¢€¢$&WGvVVâ‡"ÂcSS3b’"À¢'F¶W22&wVÖVçB‡2’'WBv26ÆÆVBv—F‚""À¢’À¢€¢$æõ7V6…&VÆF–öâ‡"’"À¢&æÖW2æò&VÆF–öæFV6Æ&VB–âF†—2f–ÆR"À¢’À¢Ò°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚gv—F‚†6ÆÂ’’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2†æVVFÆR’Â'¶×6wÒ"“°¢Ð ¢òò6VÆb×&V7W'6—fR&VÆF–öââF†—2W6VBFò76W'BöâD"Ô•"öæÇ’æ@¢òòFVÆ–&W&FVÇ’æWfW"6ÆÂ7÷F#£¦VÖ—FÂ&V6W6Rc†BæòwV&@¢òò–âW‡æE÷&VÆF–öå÷7V'G&VVæB5D4²ÔõdU$dÄõtTBÂ&÷'F–ærF†P¢òò&ö6W72(	BFW7B6ææ÷B6F6‚4”t%%BâF—fW&vVæ6Rc"FFVBF†P¢òòwV&C²cöæõöÆöævW%ö&÷'G5ööåö÷&VÆF–öå÷F†EöW‡æG5öf÷&WfW& ¢òòæ÷r7FW2÷fW"F†B&÷VæF'’FVÆ–&W&FVÇ’à¢ÆWB&V7W'6—fRÒ7&2ç&WÆ6Vâ„Ä”2Â'&VÆF–öâ†–v„FG"‡#¢&W’Ò†–v„FG"‡"’"Â“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚g&V7W'6—fR’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&W‡æG2–çFò—G6VÆb"’Â'¶×6wÒ"“° ¢òòF†R6&–Æ—G’Övf&–çB×W7B5D”ÄÂÆ÷vW"(	BF†—2—2F†R†Æb¢òò&Ææ¶WB7W&f6–ærv÷VÆB†fR'&ö¶Vâà¢Æ÷vW%÷7&2‚ff—‡GW&R‚'V–çCcE÷Væ—VU÷&æFöÖ—¦U÷FW7Bæ†&2"’¢æW‡V7B‚&6öç7G&–çBF†R•"6ææ÷BW‡&W72—2æ÷B&B&öw&Ò"“°§Ð ¢òòòv—F‚F†RF–væ÷7F–727W&f6–ærÂF†RÖ—7Æ6VBÖæÖVBÖ&wVÖVçB6†V6°¢òòò6âf–æÆÇ’Fò6öÖWF†–ærâ&VÆF–öâ6ÆÇ2&–æB'’õ4•D”ôâæBG&÷ ¢òòòF†RæÖRÂ6ò&WGvVVâ‡"Â†’Ò3s"ÂÆòÒcSS3b––æÆ–æV@¢òòòFG"ãÒ3s"bbFG"ÃÒcSS3f(	BVç6F—6f–&ÆRÂ6–ÆVçFÇ’Âg&öÒ¢òòò&öw&Òw&—GFVâFòÖVâF†R÷÷6—FRà¢òòð¢òòòF†R6†V6²—G6VÆbv2w&—GFVâ&F6‚V&Æ–W"æB&WfW'FVB0¢òòò&FöW2æ÷Bf—&R"â—Bf—&VC²—G2W'&÷"v2F—66&FVBâF†R6öçG&öÂF†@¢òòò†–B—B6¶VBv†WF†W"F†R$ôu$ÒÆ÷vW'2Âv†–6‚6ææ÷BFVÆÂF†÷6RGvð¢òòò'B(	BöæÇ’6¶–ærF†RF&ÆR'V–ÆFW"F—&V7FÇ’6âà¢5·FW7EÐ¦fâöÖ—7Æ6VE÷&VÆF–öåö&wVÖVçEöæÖUö—5÷&VgW6VB‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚'&VÆF–öåö–æÆ–æ–æu÷FW7Bæ†&2"“°¢6öç7BÄ”3¢g7G"Ò'&VÆF–öâ†–v„FG"‡#¢&W’Ò"æFG"ãÒƒ#°¢6öç7B4ÄÃ¢g7G"Ò'&æFöÖ—¦R‡"’v—F‚&÷VæFVDæD†–v‚‡"’VæB&æFöÖ—¦R#°¢ÆWBGvòÒf÷&ÖB€¢'´Ä”7ÕÆåÆç&VÆF–öâ&WGvVVâ‡#¢&WÂÆó¢–çBÂ†“¢–çB’Ò"æFG"ãÒÆòbb"æFG"ÃÒ†’ ¢“°¢ÆWBv—F‚ÒÆ6ÆÃ¢g7G'Â°¢f—‡GW&Rç&WÆ6Vâ„Ä”2ÂgGvòÂ’ç&WÆ6Vâ€¢4ÄÂÀ¢ff÷&ÖB‚'&æFöÖ—¦R‡"’v—F‚¶6ÆÇÒVæB&æFöÖ—¦R"’À¢À¢¢Ó° ¢òòæÖW2w&—GFVâv†W&RF†W’&VÆöær&–æBv†W&RF†W’&VÆöærÂæBVÖ—@¢òòW†7FÇ’v†BF†R÷6—F–öæÂf÷&ÒVÖ—G2à¢ÆWB÷6—F–öæÂÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gv—F‚‚$&WGvVVâ‡"ÂcSS3bÂ3s"’"’’¢æW‡V7B‚'cVÖ—G2F†R÷6—F–öæÂf÷&Ò"“°¢76W'B€¢÷6—F–öæÂæ6öçF–ç2‚'£3£§VvR…÷¥öFG"Âö7G‚æ'e÷fÂ‚‡V–çCcE÷B“cSS3bÂcB’’"’À¢'F†R÷6—F–öæÂf÷&Ò&–æG2ÆóÓcSS3b ¢“°¢Æ÷vW%÷7&2‚gv—F‚‚$&WGvVVâ‡"ÂÆòÒcSS3bÂ†’Ò3s"’"’’æW‡V7B‚&–âÖ÷&FW"æÖW2Æ÷vW""“° ¢òò&V÷&FW&VBæÖW2&R&VgW6VBÂæBF†RÖW76vR6—2v†–6‚&ÖWFW ¢òòv2w&—GFVâv†W&RâäõB–çfÆ–FÂVæÆ–¶RF†RF‡&VR6–&Æ–æp¢òò&VÆF–öâW'&÷'3¢c44UE2F†—2öæRæBVÖ—G2v÷&¶–ær2²²v—F€¢òòF†RfÇVW27vVBÂ6ò&&öw&ÒW'&÷"VæFW"WfW'’&6¶VæB ¢òòv÷VÆB&RÆ—FW&ÆÇ’fÇ6Rà¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚gv—F‚‚$&WGvVVâ‡"Â†’Ò3s"ÂÆòÒcSS3b’"’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B€¢×6ræ6öçF–ç2‚&†–—2&ÖWFW"2'WBv2w&—GFVâ–â÷6—F–öâ""’À¢'¶×6wÒ ¢“°¢òòc&VÆÇ’FöW27vF†VÒÂv†–6‚—2v†BÖ¶W2F†—2v÷'F‚&VgW6–ærà¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gv—F‚‚$&WGvVVâ‡"Â†’Ò3s"ÂÆòÒcSS3b’"’’¢æW‡V7B‚'cVÖ—G2"¢æ6öçF–ç2‚'£3£§VvR…÷¥öFG"Âö7G‚æ'e÷fÂ‚‡V–çCcE÷B“3s"ÂcB’’"’À¢'c&–æG2Æò£Ò3s"ÂâVç6F—6f–&ÆR6öç7G&–çB ¢“° ¢òòæÖRÖF6†–æræò&ÖWFW"vWG2—G2÷vâ6VçFVæ6Rà¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚gv—F‚‚$&WGvVVâ‡"Âæ÷7V6‚ÒcSS3bÂ†’Ò3s"’"’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B€¢×6ræ6öçF–ç2‚&æ÷7V6†æÖW2æò&ÖWFW"öb&WGvVVæ"’À¢'¶×6wÒ ¢“°§Ð ¢òòò&æFöÖ—¦Râââv—F†w&—GFVâ–ç6–FR6ö×öæVçBÖWF†öB&öG’—0¢òòò&VgW6VBf÷"F†R6ÖR&V6öç2FW7BÖ&öG’öæR—2à¢òòð¢òòòF†R6†V6²&÷fR—B&VG2F†R6öÇfW"&ö&ÆVÒF&ÆRÂæBF†BF&ÆP¢òòòöæÇ’WfW"6öÆÆV7FVBFW7FæBG6W6—FW2(	B6òF†R–FVçF–6À¢òòò7vVB6ÆÂ–ç6–FRâvVçBw2öæ†æFÆW"Æ÷vW&VB6ÆVâæ@¢òòò&V6†VB2²²v—F‚F†R&÷VæG2&WfW'6VBâF†R6—FW2&Ræ÷B6¶—VB@¢òòòTÔ•54”ôã¢&÷F‚&6¶VæG2VÖ—BF†VÒF‡&÷Vv€¢òòò7÷F#£¦VÖ—E÷&æFöÖ—¦Uöf÷%÷6—FVÂv†–6‚Æ÷vW'2F†R6öç7G&–ç@¢òòò—G6VÆbÂ6òF†—2v2D"Ô•"6–ÆVçFÇ’Ö—2ÖÆ÷vW&–ærÂæ÷B§W7Bcà¢5·FW7EÐ¦fâö6ö×öæVçE÷66÷U÷&VÆF–öåö&wVÖVçE÷7vö—5÷&VgW6VB‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&6ö×öæVçEöÖWF†öE÷&æFöÖ—¦U÷FW7Bæ†&2"“°¢6öç7B$ôE“¢g7G"Ð¢""çfÇVRâÆâ"çfÇVRÂ#Æâ"æFG"ÓÒ#B#°¢76W'B†f—‡GW&Ræ6öçF–ç2„$ôE’’Â&f—‡GW&R6†R6†ævVB"“°¢òòF†R&æFöÖ—¦RF†—2&Ww&—FW2Æ—fW2–âvVçB&æFöÖ—¦W&w0¢òòöâ–åöWb‡B–†æFÆW"(	B6ö×öæVçBÖWF†öB&öG’Âæ÷BFW7B&öG’à¢ÆWBv—F‚ÒÆ6ÆÃ¢g7G'Â°¢f—‡GW&P¢ç&WÆ6Vâ€¢&vVçB&æFöÖ—¦W""À¢'&VÆF–öâ&æB‡#¢&Vt÷ÂÆó¢–çBÂ†“¢–çB’Ò"çfÇVRâÆòbb"çfÇVRÂ†•ÆåÀ¢ÆåÀ¢vVçB&æFöÖ—¦W""À¢À¢¢ç&WÆ6Vâ€¢$ôE’À¢ff÷&ÖB‚"¶6ÆÇÕÆâ"æFG"ÓÒ#B"’À¢À¢¢Ó° ¢òò6öçG&öÇ3¢F†R÷6—F–öæÂf÷&ÒæBF†R–âÖ÷&FW"æÖVBf÷&Ò&÷F€¢òò7F–ÆÂÆ÷vW"ÂæB&÷F‚VÖ—BF†R&÷VæG2F†R6÷W&6R6¶VBf÷"à¢f÷"6ÆÂ–â²$&æB‡"ÂÂ#’"Â$&æB‡"ÂÆòÒÂ†’Ò#’%Ò°¢ÆWB&öw&ÒÒÆ÷vW%÷7&2‚gv—F‚†6ÆÂ’’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚&¶6ÆÇÖÆ÷vW'3¢¶WÒ"’“°¢ÆWBVÖ—GFVBÒF&—#£¦VÖ—B€¢g&öw&ÒÀ¢fÖW&vVE÷7&2‚gv—F‚†6ÆÂ’’À¢f7÷F#£¤VÖ—D÷G3£¦FVfVÇB‚’À¢¢æW‡V7B‚&VÖ—G2"“°¢76W'B€¢VÖ—GFVBæ6öçF–ç2‚'£3£§VwB…÷¥÷fÇVRÂö7G‚æ'e÷fÂ‚‡V–çCcE÷B“ÂcB’’"’À¢&¶6ÆÇÖ&–æG2Æò£Ò ¢“°¢Ð ¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚gv—F‚‚$&æB‡"Â†’Ò#ÂÆòÒ’"’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B€¢×6ræ6öçF–ç2‚&†–—2&ÖWFW"2'WBv2w&—GFVâ–â÷6—F–öâ""’À¢'¶×6wÒ ¢“° ¢òòv†BÖFR—Bv÷'F‚&VgW6–æs¢cVÖ—G2F†R7vVB&÷VæG2ÂæB6ð¢òòF–BD"Ô•"&Vf÷&RF†—26†V6²W†—7FVB(	BfÇVRâ#bbfÇVRÀ¢òò†2æò6öÇWF–öâà¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gv—F‚‚$&æB‡"Â†’Ò#ÂÆòÒ’"’’¢æW‡V7B‚'cVÖ—G2"¢æ6öçF–ç2‚'£3£§VwB…÷¥÷fÇVRÂö7G‚æ'e÷fÂ‚‡V–çCcE÷B“#ÂcB’’"’À¢'c&–æG2Æò£Ò# ¢“° ¢òòF†R–çfÆ–F†ÆböbF†R6ÖR6†V6²&V6†W26ö×öæVçB66÷RFöòà¢ÆWBW'"ÒÆ÷vW%÷7&2‚gv—F‚‚$æõ7V6…&VÂ‡"’"’’çVçw&öW'"‚“°¢76W'B€¢f÷&ÖB‚'¶W''Ò"’æ6öçF–ç2‚&æõ7V6…&VÆæÖW2æò&VÆF–öæFV6Æ&VB–âF†—2f–ÆR"’À¢'¶W''Ò ¢“°§Ð ¢òòòWfW'’&öG’6†RF†B6â†÷7B&æFöÖ—¦V—2vÆ¶VBÂæBWfW'¢òòòv’6ö×öæVçB&öG’6âäÔRF†R&æFöÖ—¦RF&vWB—2&W6öÇfVBà¢òòð¢òòòF†R6V6öæB†Æb—2v†BF†Rf—'7B7vVWÖ—76VBâ&æFöÖ—¦RF&vW@¢òòò—2Æöö¶VBW'’æÖRÂ6ò&öG’v†÷6R66÷R—2V×G’6öçG&–'WFW0¢òòòæ÷F†–ær†÷vWfW"6&VgVÆÇ’—B—2vÆ¶VB(	BæB6ö×öæVçB&–æG2æÖW0¢òòòF‡&VRv—2FW7F&öG’FöW2æ÷C¢f–VÆBÂÖWF†öB&ÖWFW"Âæ@¢òòòâöæ†æFÆW"w2WfVçB–ÆöBâÆÂF‡&VR6öÆÆV7FVB¦W&ò6—FW0¢òòòVçF–ÂF†R66÷Rv26VVFVBà¢òòð¢òòòF†RFW7B&ö&W2F†R6öÆÆV7F÷"&F†W"F†âÆ÷vW%÷&öw&ÖÂ&V6W6P¢òòòÖ÷7BöbF†W6R6†W2&R&VgW6VBV&Æ–W"'’Vç&VÆFVBvFW2†à¢òòòVæ&÷VæBFW7F&Væ6†—2æ÷BÆ÷vW&VBBÆÂ’ÂæB&VgW6Âg&öÒöæP¢òòòöbF†÷6RvFW2v÷VÆB&÷fRæ÷F†–ær&÷WBv†WF†W"F†R6—FRv0¢òòò6öÆÆV7FVBâF†RVæB×FòÖVæB&VgW6Â—2–ææVB'¢òòòö6ö×öæVçE÷66÷U÷&VÆF–öåö&wVÖVçE÷7vö—5÷&VgW6VF&÷fRà¢5·FW7EÐ¦fâWfW'•ö6ö×öæVçEö&öG•÷F†Eö6åö†÷7Eö÷&æFöÖ—¦Uö—5÷vÆ¶VB‚’°¢6öç7B$TÅTDS¢g7G"Ò%À¦'W2 ¢FÆÕöÖWF†öBvò†FG#¢V–çCÃ3#â“¢&Æö6¶–æs°¦VæB'W2  §G&ç67F–öâ&Vt÷ ¢FG"¢V–çCÃƒâv—F‚·&ævRƒÂcB•Ð¢fÇVR¢V–çCÃ3#à¦VæBG&ç67F–öâ&Vt÷  §&VÆF–öâ&æB‡#¢&Vt÷ÂÆó¢–çBÂ†“¢–çB’Ò"çfÇVRâÆòbb"çfÇVRÂ†¢#°¢òòF†R7vVB6ÆÂÂw&—GFVâöæ6RæBG&÷VB–çFòV6‚6†Râ%¦ ¢òòFV6Æ&W2—G2÷vâF&vWC²%¥õ&æB%¥õF&æFöÖ—¦RæÖRF†P¢òòVæ6Æ÷6–ær&öG’—2W‡V7FVBFò†fR&÷VæBà¢6öç7B%£¢g7G"Ò"ÆWB"¢&Vt÷ÆåÀ¢Çƒ#&æFöÖ—¦R‡"’v—F‚&æB‡"Â†’Ò#ÂÆòÒ’VæB&æFöÖ—¦UÆâ#°¢6öç7B%¥õ#¢g7G"Ò"&æFöÖ—¦R‡"’v—F‚&æB‡"Â†’Ò#ÂÆòÒ’VæB&æFöÖ—¦UÆâ#°¢6öç7B%¥õC¢g7G"Ò"&æFöÖ—¦R‡B’v—F‚&æB‡BÂ†’Ò#ÂÆòÒ’VæB&æFöÖ—¦UÆâ#° ¢òòV6‚VçG'’æÖW2F†R6öÆÆV7F÷"&Ò—BW†W&6—6W2âF‡&VRöbF†W6P¢òòÆæBöâôäR&Ò(	BF†R'6W"Ö2&÷F‚†öö¶&ÆVæBgVæ7F–öæ ¢òò–âç’6ö×öæVçB&öG’Fò6ö×öæVçD—FVÓ£¤†öö¶&ÆV(	B6òF†R&×0¢òò&RfWvW"F†âF†R6†W2ÂæBF†R6†W2&Rv†BW6W'2w&—FRà¢ÆWB6†W3¢²‚g7G"Â7G&–ær“²ÒÒ°¢€¢&âöæ†æFÆW"´öä†æFÆW%Ò"À¢f÷&ÖB‚&vVçBÆâ–åöWb¢WfVçCÇV–çCÃƒãåÆâöâ–åöWb‡B•Æçµ%§ÒVæBöåÆæVæBvVçBÆâ"’À¢’À¢€¢&†öö¶&ÆRÖWF†öB´†öö¶&ÆUÒ"À¢f÷&ÖB‚&vVçBÆâ†öö¶&ÆRvò‚•Æçµ%§ÒVæBvõÆæVæBvVçBÆâ"’À¢’À¢€¢&66÷&V&ö&BÖWF†öB´—FVÓ£¥66÷&V&ö&EÒ"À¢f÷&ÖB‚'66÷&V&ö&B5Æâ†öö¶&ÆRvò‚•Æçµ%§ÒVæBvõÆæVæB66÷&V&ö&B5Æâ"’À¢’À¢€¢&6WVVæ6W"ÖWF†öB´—FVÓ£¥6WVVæ6W%Ò"À¢f÷&ÖB‚'6WVVæ6W"Æâ†öö¶&ÆRvò‚•Æçµ%§ÒVæBvõÆæVæB6WVVæ6W"Æâ"’À¢’À¢€¢&FW7F&Væ6‚gVæ7F–öæ´†öö¶&ÆUÒ"À¢f÷&ÖB‚'FW7F&Væ6‚F%ÆâGWB¢F÷ÆâgVæ7F–öâvò‚•Æçµ%§ÒVæBgVæ7F–öâvõÆæVæBFW7F&Væ6‚F%Æâ"’À¢’À¢€¢&FW7F&Væ6‚Æ–fV7–6ÆR†6R´Æ–fV7–6ÆUÒ"À¢f÷&ÖB‚'FW7F&Væ6‚F%ÆâGWB¢F÷Æâ6WGWÆçµ%§ÒVæB6WGWÆæVæBFW7F&Væ6‚F%Æâ"’À¢’À¢€¢&vF6†För&öG’µvF6†FöuÒ"À¢f÷&ÖB‚&vVçBÆâvF6†FöuÆâW&–öB7–6ÆW5Æçµ%§ÒVæBvF6†FöuÆæVæBvVçBÆâ"’À¢’À¢€¢&f–ÆR×66÷RgVæ7F–öæ´—FVÓ£¤gVæ7F–öåÒ"À¢f÷&ÖB‚&gVæ7F–öâvò‚•Æçµ%§ÖVæBgVæ7F–öâvõÆâ"’À¢’À¢€¢&G&ç67F÷"DÄÒF&vWBF‡&VBµF&vWEFÆÕF‡&VEÒ"À¢f÷&ÖB‚'G&ç67F÷"‚&÷VæBFò%ÆâF‡&VB'W2ævò†FG#¢V–çCÃ3#â•Æçµ%§ÒVæBF‡&VEÆæVæBG&ç67F÷"…Æâ"’À¢’À¢€¢&G&ç67F÷"v†Vâ7F—fVÖWF†öB·v†Våö7F—fUÒ"À¢f÷&ÖB€¢'G&ç67F÷"‚&÷VæBFò%Æâv†Vâ7F—fUÆâ†öö¶&ÆRvò‚•Æçµ%§ÒVæBvõÆâVæBv†VåÆæVæBG&ç67F÷"…Æâ ¢’À¢’À¢Ó° ¢òòF†RF‡&VRæÖR&–æF–æw26ö×öæVçB&öG’†2æBFW7B&öG’FöW0¢òòæ÷BâV6‚öbF†W6R6öÆÆV7FVBäõD„”är&Vf÷&RF†R66÷Rv26VVFVBà¢ÆWBF&vWG3¢²‚g7G"Â7G&–ær“²5ÒÒ°¢€¢&6ö×öæVçBf–VÆB2F†RF&vWB"À¢f÷&ÖB‚&vVçBÆâ"¢&Vt÷Æâ†öö¶&ÆRvò‚•Æçµ%¥õ'ÒVæBvõÆæVæBvVçBÆâ"’À¢’À¢€¢&ÖWF†öB&ÖWFW"2F†RF&vWB"À¢f÷&ÖB‚&vVçBÆâ†öö¶&ÆRvò‡#¢&Vt÷•Æçµ%¥õ'ÒVæBvõÆæVæBvVçBÆâ"’À¢’À¢€¢&âWfVçB–ÆöB2F†RF&vWB"À¢f÷&ÖB€¢&vVçBÆâ&W¢WfVçCÅ&Vt÷åÆâöâ&W‡B•Æçµ%¥õGÒVæBöåÆæVæBvVçBÆâ ¢’À¢’À¢Ó° ¢f÷"‡v†BÂ&öG’’–â6†W2æ—FW"‚’æ6†–â‡F&vWG2æ—FW"‚’’°¢ÆWB7&2Òf÷&ÖB‚'µ$TÅTDWÕÆç¶&öG—Ò"“°¢ÆWB'6VBÒ†&3£§'6W#£§'6U÷6÷W&6R‚g7&2’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·v†GÓ¢¶S£÷Ò"’“°¢ÆWBF&ÆRÒ†&3£§6öÇfW#£§&ö&ÆVÕ÷F&ÆS£¦'V–ÆEö6ö×öæVçE÷66÷U÷&ö&ÆVÕ÷F&ÆR‚g'6VB“°¢ÆWB×6w3¢fV3Å7G&–æsâÒF&ÆP¢æVçG&–W0¢æ—FW"‚¢æf–ÇFW%öÖ‡ÆWÂÖF6‚fRæ'V–ÆB°¢†&3£§6öÇfW#£§&ö&ÆVÕ÷F&ÆS£¥G—VE6öÇfW%&ö&ÆVÔ'V–ÆC£¤Æ÷vW$W'&÷"‡b’Óâ°¢6öÖR†f÷&ÖB‚'·c£÷Ò"’¢Ð¢òÓâæöæRÀ¢Ò¢æ6öÆÆV7B‚“°¢76W'EöW‡F&ÆRæVçG&–W2æÆVâ‚’ÂÂ'·v†GÓ¢6—FRæ÷B6öÆÆV7FVB"“°¢76W'B€¢×6w2æ—FW"‚’æç’‡Æ×ÂÒæ6öçF–ç2‚%&VÆF–öäæÖVD&tÖ—7Æ6VB"’’À¢'·v†GÓ¢¶×6w3£÷Ò ¢“°¢Ð ¢òòF†RræF—6&ÆVFwV&BöâF†RvF6†För&Ò—2&VÇBÖæBÖ'&6W2À¢òòæ÷BÆ—fRf–ÇFW"ÂæBF†—2&V6÷&G2v‡’&F†W"F†âÆVf–ær¢òò&VFW"Fò77VÖR—B—2ÆöBÖ&V&–æs¢vF6†FörF—6&ÆVFF¶W2æð¢òò&öG’BÆÂ(	BF†R'6W"&VgW6W2F†Rf—'7B7FFVÖVçB(	B6ò¢òòF—6&ÆVBvF6†Förw2&öG–—2Çv—2V×G’æBv÷VÆB6öÆÆV7@¢òòæ÷F†–ærv—F‚÷"v—F†÷WBF†RwV&BâF†RwV&B7F—26òF†@¢òòÆÆ÷v–ær&öG’ÆFW"6ææ÷B6–ÆVçFÇ’7F'B&VgW6–ær&Æö6°¢òòv†÷6R6öFVvVâ—27W&W76VBà¢ÆWBF—6&ÆVBÐ¢f÷&ÖB‚'µ$TÅTDWÕÆævVçBÆâvF6†FörF—6&ÆVEÆçµ%§ÒVæBvF6†FöuÆæVæBvVçBÆâ"“°¢ÆWBW'"Ò†&3£§'6W#£§'6U÷6÷W&6R‚fF—6&ÆVB’æW‡V7EöW'"‚&vF6†FörF—6&ÆVFF¶W2æò&öG’"“°¢76W'B†f÷&ÖB‚'¶W'#£÷Ò"’æ6öçF–ç2‚%VæW‡V7FVEFö¶Vâ"’Â'¶W'#£÷Ò"“°§Ð ¢òòòF†RGvò†ÇfW2öbG&ç67F÷"6†&RôäRæÖR66÷RÂæBæÖP¢òòò&V&÷VæBFòâVç&W6öÇf&ÆRG—R7F÷2&W6öÇf–ærà¢òòð¢òòò&÷F‚&RF†R6ÖRÖ—7F¶R–âF–ffW&VçBÆ6W3¢66÷R76VÖ&ÆV@¢òòòg&öÒF†Rw&öær6WBöbFV6Æ&F–öç2â7–çF…ö6ö×öæVçEög&öÕð¢òòòG&ç67F÷&6öæ6FVæFW2G&ç67F÷"w2Çv—2×&W6VçB—FV×2æB—G0¢òòòv†Vâ7F—fV&Æö6²Â6òf–VÆBFV6Æ&VB–âF†R6†&VB†Æb&VÆÇ¢òòò—2–â66÷R–ç6–FRv†Vâ7F—fV(	BvÆ¶–ærF†R†ÇfW22–æFWVæFVç@¢òòò66÷W2ÖFRF†÷6R&öF–W26öÆÆV7Bæ÷F†–ærBÆÂâæBÆWF÷ ¢òòò&ÖWFW"v†÷6RG—RFöW2æ÷B&W6öÇfR×W7BTä$”äBF†RæÖRf–VÆ@¢òòò6VVFVBÂæ÷BÆVfRF†Rf–VÆBw2G—R7FæF–ærà¢5·FW7EÐ¦fâö6ö×öæVçE÷66÷U÷7ç5ö&÷F…÷G&ç67F÷%ö†ÇfW5öæEö†öæ÷W'5÷6†F÷v–ær‚’°¢6öç7B$TÅTDS¢g7G"Ò%À¦'W2 ¢FÆÕöÖWF†öBvò†FG#¢V–çCÃ3#â“¢&Æö6¶–æs°¦VæB'W2  §G&ç67F–öâ&Vt÷ ¢FG"¢V–çCÃƒâv—F‚·&ævRƒÂcB•Ð¢fÇVR¢V–çCÃ3#à¦VæBG&ç67F–öâ&Vt÷  §&VÆF–öâ&æB‡#¢&Vt÷ÂÆó¢–çBÂ†“¢–çB’Ò"çfÇVRâÆòbb"çfÇVRÂ†¢#°¢6öç7B%¥õ#¢g7G"Ð¢"&æFöÖ—¦R‡"’v—F‚&æB‡"Â†’Ò#ÂÆòÒ’VæB&æFöÖ—¦UÆâ#°¢ÆWB6—FW2ÒÆ&öG“¢g7G'Â°¢ÆWB7&2Òf÷&ÖB‚'µ$TÅTDWÕÆç¶&öG—Ò"“°¢ÆWB'6VBÒ†&3£§'6W#£§'6U÷6÷W&6R‚g7&2’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·7&7ÕÆç¶S£÷Ò"’“°¢†&3£§6öÇfW#£§&ö&ÆVÕ÷F&ÆS£¦'V–ÆEö6ö×öæVçE÷66÷U÷&ö&ÆVÕ÷F&ÆR‚g'6VB¢æVçG&–W0¢æÆVâ‚¢Ó° ¢òòF†Rf–VÆB—2–âF†R4„$TB†Æc²F†R&æFöÖ—¦R—2–âv†Và¢òò7F—fVâöæR66÷R7ç2&÷F‚Â6òF†R6—FR&W6öÇfW2à¢76W'EöW€¢6—FW2‚ff÷&ÖB€¢'G&ç67F÷"‚&÷VæBFò%Æâ"¢&Vt÷Æâv†Vâ7F—fUÆâ†öö¶&ÆRvò‚•Æçµ%¥õ'ÒVæBvõÆâVæBv†VåÆæVæBG&ç67F÷"…Æâ ¢’’À¢À¢&6†&VBÖ†Æbf–VÆB—2–â66÷R–ç6–FRv†Vâ7F—fV ¢“° ¢òòÆWFF†B&V&–æG2F†R6ÖRæÖRFòG—RF†—2vÆ²6ææ÷@¢òò&W6öÇfRÆVfW2—BVç&W6öÇfVB&F†W"F†âfÆÆ–ær&6²FòF†P¢òòf–VÆBw2G—R(	B6òF†R6—FR—2æ÷B6öÆÆV7FVBVæFW"&Vt÷à¢76W'EöW€¢6—FW2‚ff÷&ÖB€¢&vVçBÆâ"¢&Vt÷Æâ†öö¶&ÆRvò‚•ÆâÆWB"ÒUÆçµ%¥õ'ÒVæBvõÆæVæBvVçBÆâ ¢’’À¢À¢&âVçG—VBÆWF6†F÷w2F†Rf–VÆB&F†W"F†â–æ†W&—F–ær—G2G—R ¢“°¢òòF†R6ÖRÂf–&ÖWFW"à¢76W'EöW€¢6—FW2‚ff÷&ÖB€¢&vVçBÆâ"¢&Vt÷Æâ†öö¶&ÆRvò‡"•Æçµ%¥õ'ÒVæBvõÆæVæBvVçBÆâ ¢’’À¢À¢&âVçG—VB&ÖWFW"6†F÷w2F†Rf–VÆB ¢“°¢òò6†F÷v–ærF†BDôU2&W6öÇfR7F–ÆÂ&W6öÇfW2(	BFòF†R–ææW"G—Rà¢76W'EöW€¢6—FW2‚ff÷&ÖB€¢&vVçBÆâ"¢V–çCÃƒåÆâ†öö¶&ÆRvò‚•ÆâÆWB"¢&Vt÷Æçµ%¥õ'ÒVæBvõÆæVæBvVçBÆâ ¢’’À¢À¢&G—VBÆWF6†F÷v–æræöâ×G&ç67F–öâf–VÆB&W6öÇfW2 ¢“°§Ð ¢òòòf—fRF—66&FVBW'&÷'2W6VBFòF—6&ÆRF†R&VgW6ÂVçF—&VÇ’à¢òòð¢òòòÔ…ôU%$õ%6—2F–væ÷7F–72ÕdôÅTÔRwV&BÂæB—Bv2F÷V&Æ–ær0¢òòòF†R7F÷6öæF—F–öâf÷"F†Rv†öÆR6öç7G&–çBvÆ²âf—fR6ÆW6W2F†@¢òòòG&—FVÆ–&W&FVÇ’ÖF—66&FVBW'&÷"(	BBæFG"ÓÒBçfÇVVG&—0¢òòòv–GF„Ö—6ÖF6†Âv†–6‚F—fW&vVæ6RS’&V6÷&G226&–Æ—G’vÂæ÷@¢òòò&B&öw&Ò(	Bf–ÆÆVBF†RW'&÷"fV7F÷"ÂæBF†RvÆ²7F÷VB&Vf÷&P¢òòòF†R&æB‡BÂ†’Ò#ÂÆòÒ–öâF†RæW‡BÆ–æRv2WfW ¢òòòW‡æFVBâD"Ô•"Æ÷vW&VBÂæB&÷F‚&6¶VæG2VÖ—GFVBF†R7vVBÀ¢òòòVç6F—6f–&ÆR6öç7G&–çBà¢òòð¢òòòF†R6æ÷r&÷VæG2öæÇ’†÷rÖç’W'&÷'2&R5Dõ$TC¢&VÆF–öâW'&÷ ¢òòò—2Çv—2¶WBÂæB66ææ–ær7F÷2öæ6RöæR—2–â†æBà¢5·FW7EÐ¦fâ÷'VåööeöF—66&FVEöW'&÷'5öæõöÆöævW%ö†–FW5ö÷&VÆF–öåöW'&÷"‚’°¢ÆWB7&2ÒÇC¢W6—¦WÂ°¢ÆWBæö—6S¢7G&–ærÒƒâçB¢æÖ‡Å÷Â"BæFG"ÓÒBçfÇVUÆâ"çFõ÷7G&–ær‚’¢æ6öÆÆV7B‚“°¢f÷&ÖB€¢&FöÖ–âEÆâg&WöÖ‡£¢ÆæVæBFöÖ–âEÆåÆåÀ¢G&ç67F–öâ&WÆâFG"¢V–çCÃƒåÆâfÇVR¢V–çCÃ3#åÆåÀ¢VæBG&ç67F–öâ&WÆåÆåÀ¢&VÆF–öâ&æB‡#¢&WÂÆó¢–çBÂ†“¢–çB’Ò"çfÇVRâÆòbb"çfÇVRÂ†•ÆåÆåÀ¢FW7BEÆâÆWBGWB¢F÷ÆâÆWBB¢&WÆâ6Æö6²6Æ²ÒEÆâ'VåÆåÀ¢Çƒ#&æFöÖ—¦R‡B’v—F…Æç¶æö—6WÒ&æB‡BÂ†’Ò#ÂÆòÒ•ÆåÀ¢Çƒ#VæB&æFöÖ—¦UÆâVæB'VåÆæVæBFW7BEÆâ ¢¢Ó° ¢òòf÷W"æö—6R6ÆW6W2v2VæFW"F†R6æBÇ&VG’&VgW6VC²f—fRv0¢òòF†RW†7Bö–çBF†R&VgW6ÂW6VBFòfæ—6‚â&÷F‚&VgW6Ræ÷rÂæ@¢òò6òFöW2'VâvVÆÂ7BF†R6à¢f÷"B–â³W6—¦RÂBÂRÂ•Ò°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g7&2‡B’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B€¢×6ræ6öçF–ç2‚&Ö—7Æ6VBæÖVB&wVÖVçB–â&VÆF–öâ6ÆÂ"’À¢'·GÒæö—6R6ÆW6W3¢¶×6wÒ ¢“°¢òòv†BF†R&VgW6Â—2v÷'Fƒ¢cVÖ—G2F†R7vVB&÷VæG2@¢òòWfW'’öæRöbF†W6RÂæBD"Ô•"F–BFöòBBãÒRà¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2‡B’’¢æW‡V7B‚'cVÖ—G2"¢æ6öçF–ç2‚'£3£§VwB…÷¥÷fÇVRÂö7G‚æ'e÷fÂ‚‡V–çCcE÷B“#ÂcB’’"’À¢'·GÒæö—6R6ÆW6W3¢c&–æG2Æò£Ò# ¢“°¢Ð ¢òòF†R67F–ÆÂ63¢F†RF—66&FVBW'&÷'2&Ræ÷B×VÇF—Æ–VB'¢òòF†RÆöævW"vÆ²Â6òW6W"FöW2æ÷BvWBvÆÂöbF–væ÷7F–72f÷ ¢òò6Æ72F†—2&6¶VæBFVÆ–&W&FVÇ’7F—2V–WB&÷WBà¢ÆWBF&ÆRÒ†&3£§6öÇfW#£§&ö&ÆVÕ÷F&ÆS£¦'V–ÆE÷G—VE÷6öÇfW%÷&ö&ÆVÕ÷F&ÆR‚fÖW&vVE÷7&2‚g7&2ƒ’’’“°¢ÆWBW''3¢fV3ÇW6—¦SâÒF&ÆP¢æVçG&–W0¢æ—FW"‚¢æf–ÇFW%öÖ‡ÆWÂÖF6‚fRæ'V–ÆB°¢†&3£§6öÇfW#£§&ö&ÆVÕ÷F&ÆS£¥G—VE6öÇfW%&ö&ÆVÔ'V–ÆC£¤Æ÷vW$W'&÷"‡b’Óâ°¢6öÖR‡bæ—FW"‚’æf–ÇFW"‡Ç‡Â‚æ—5÷&VÆF–öåöW'&÷"‚’’æ6÷VçB‚’¢Ð¢òÓâæöæRÀ¢Ò¢æ6öÆÆV7B‚“°¢76W'B€¢W''2æ—FW"‚’æÆÂ‡ÆçÂ¦âÃÒR’À¢&æöâ×&VÆF–öâW'&÷'27F’6VBBÔ…ôU%$õ%3¢¶W''3£÷Ò ¢“°§Ð ¢òòòcv—fW2F–væ÷7F–2–ç7FVBöbF¶–ærF†R&ö6W72F÷vâà¢òòð¢òòòW‡æE÷&VÆF–öå÷7V'G&VV†BæòwV&Böbç’¶–æBÂ6ð¢òòò&VÆF–öâ"‡"’Ò"‡"–&V7W'6VBVçF–ÂF†R7F6²&â÷WC¢4”t%%BÀ¢òòòæòÖW76vRÂæòW†—B6öFR'V–ÆB7—7FVÒ6â–çFW'&WBâF—fW&vVæ6RS¢òòò&V6÷&FVB—BæBÆVgB—B÷Vâ&V6W6R&FW7B6ææ÷B6F6‚â&÷'B ¢òòò(	Bv†–6‚—2G'VRÂæB—2W†7FÇ’v‡’F†RwV&B†BFò6öÖR&Vf÷&RF†P¢òòòFW7Bà¢òòð¢òòòF‡&VR6†W2&âv’ÂæBV6‚FVfVFVBF†RwV&Bw&—GFVâf÷"F†P¢òòòöæR&Vf÷&R—BâF†Rf—‚—2æ÷B&WGFW"&÷VæBöâF†R$U5TÅC²—B—2¢òòò&VÆF–öâÔäÔR7F6²Âv†–6‚7F÷2F†RÆö÷B—G2&ö÷C ¢òòð¢òòò¢W"ÖW‡ç6–öâ'VFvWBÆVgB&VÆF–öâ"‡"’Ò"‡"²"–F÷V&Æ–æp¢òòòF†R&wVÖVçBVçF–ÂF†R&ö6W72v2ôôÒÖ¶–ÆÆVBà¢òòò¢6†&v–ærW"æöFR$ôET4TBf—†VBF†BÂæBÆVg@¢òòò&VÆF–öâ"‡"’Ò"‚‚‚Ž(
g.(
b’’’–(	BcæW7FVB&Vç2ÂC‚'—FW2ö`¢òòò6÷W&6R(	B×VÇF—Ç––ærF†RG&VRw2DUD‚'’cW"ÆWfVÂVçF–ÂF†P¢òòò7G'V7GW&ÂvÆ²÷fW&fÆ÷vVBF†R7F6²âF†B—2F†RfW'’4”t%%@¢òòòF†RwV&Bv2w&—GFVâFò&WfVçBà¢òòò¢&VÆF–öâÇ&VG’&V–ærW‡æFVB—2W‡æF–ær–çFò—G6VÆbÂ6ð¢òòòF†RæÖR7F6²&VgW6W2—B&Vf÷&Rç’G&VR—2'V–ÇBâ—BFöW2æ÷@¢òòòÖGFW"†÷rf7BF†R&öG’v÷VÆB†fRw&÷vâà¢òòð¢òòòF†W&R—2æòVæBFòF†Rf—'7BÆ—7BÂ&V6W6R&÷VæF–ærF†R÷WGWBöbà¢òòòVæ&÷VæFVBÆö÷—2F†Rw&öærÆ6RFò7FæBà¢òòò6öç7G&–çG3£§G—VEöÆ÷vW&Ç&VG’wV&FVBF†R6ÖR&V7W'6–öâF†—0¢òòòv“²&V6†–ærf÷"—B†W&RFöö²F‡&VRG&–W2à¢òòð¢òòòF†RæöFR'VFvWBæBFWF‚Æ–Ö—B7F’2&6·7F÷2f÷"w&÷wF‚F†B—0¢òòòæ÷B7–6Æ–2(	B6†–âöbD•5D”ä5B&VÆF–öç2V6‚6ÆÆ–ærF†R&Wf–÷W0¢òòòöæRGv–6R—2f–æ—FRæBW‡öæVçF–ÂâF†B66R—2äõB&VÆ÷rÂ&V6W6P¢òòò—B—2æ÷Bc×7V6–f–3¢G—VEöÆ÷vW&†2—G2÷vâW‡æFW"÷fW"F†P¢òòò6ÖR&VÆF–öç2Â—B&âVæ'VFvWFVBf÷"ÆöævW"F†âF†—2wV&BW†—7FVBÀ¢òòòæB&÷F‚'VâöâWfW'’†&26–Öv†FWfW"ÒÖ6öFVvVæ6—2â&÷VæF–æp¢òòòöæRöbGvòW‡æFW'2&÷VæG2æ÷F†–ærâ&÷F‚Æ–Ö—G2æ÷rÆ—fR–à¢òòò7Bç'6æB&÷F‚W‡æFW'26†&vRF†VÓ²F†R6†R—2–ææVB'¢òòòöF÷V&Æ–æu÷&VÆF–öåö6†–åö—5÷&VgW6VEöE÷F†U÷6ÖU÷ö–çEö'•ö&÷F…ð¢òòò&6¶VæG6à¢5·FW7EÐ¦fâcöæõöÆöævW%ö&÷'G5ööåö÷&VÆF–öå÷F†EöW‡æG5öf÷&WfW"‚’°¢ÆWB†VBÒ&FöÖ–âEÆâg&WöÖ‡£¢ÆæVæBFöÖ–âEÆåÆåÀ¢G&ç67F–öâ&WÆâFG"¢V–çCÃƒåÆâfÇVR¢V–çCÃ3#åÆåÀ¢VæBG&ç67F–öâ&WÆåÆâ#°¢ÆWBFW7BÒÆ6ÆÃ¢g7G'Â°¢f÷&ÖB€¢'FW7BEÆâÆWBGWB¢F÷ÆâÆWBB¢&WÆâ6Æö6²6Æ²ÒEÆâ'VåÆåÀ¢Çƒ#&æFöÖ—¦R‡B’v—F…Æâ¶6ÆÇÕÆâVæB&æFöÖ—¦UÆåÀ¢Çƒ#VæB'VåÆæVæBFW7BEÆâ ¢¢Ó°¢ÆWB6†–âÒÆFWFƒ¢W6—¦WÂ°¢ÆWB×WB&VÇ2Ò7G&–æs£¦g&öÒ‚'&VÆF–öâ#‡#¢&W’Ò"çfÇVRâÆâ"“°¢f÷"’–ââãÖFWF‚°¢&VÇ2³Òff÷&ÖB‚'&VÆF–öâ'¶—Ò‡#¢&W’Ò'·Ò‡"•Æâ"Â’Ò“°¢Ð¢&VÇ0¢Ó° ¢òò&V6†–ærç’öbF†W6RÆ–æW2BÆÂ—2F†R76W'F–öã¢öâF†RöÆ@¢òò6öFRF†RFW7B&–æ'’F–VB†W&Rv—F‚6–væÂb‡F†Rf—'7BGvò’÷ ¢òòv2ôôÒÖ¶–ÆÆVB‡F†RÆ7BGvò’à¢f÷"‡v†BÂ&VÇ2Â6ÆÂ’–â°¢€¢&&VÆF–öâF†B6ÆÇ2—G6VÆb"À¢'&VÆF–öâ"‡#¢&W’Ò"‡"•Æâ"çFõ÷7G&–ær‚’À¢%"‡B’"À¢’À¢€¢'Gvò&VÆF–öç2F†B6ÆÂV6‚÷F†W""À¢'&VÆF–öâ‡#¢&W’Ò"‡"•Æç&VÆF–öâ"‡#¢&W’Ò‡"•Æâ"çFõ÷7G&–ær‚’À¢$‡B’"À¢’À¢òòF†RöæRW"ÖW‡ç6–öâ'VFvWBÖ—76VC¢F†R$uTÔTåBF÷V&ÆW2À¢òò6òF†RG&VRW‡ÆöFW2BFWF‚F†RFWF‚wV&B—2æ÷v†W&P¢òòæV"à¢€¢&&VÆF–öâv†÷6R&wVÖVçBw&÷w2"À¢'&VÆF–öâ"‡#¢&W’Ò"‡"²"•Æâ"çFõ÷7G&–ær‚’À¢%"‡B’"À¢’À¢€¢'GvòF†B6ÆÂV6‚÷F†W"v—F‚w&÷v–ær&wVÖVçB"À¢'&VÆF–öâ‡#¢&W’Ò"‡"²"•Æç&VÆF–öâ"‡#¢&W’Ò‡"²"•Æâ"çFõ÷7G&–ær‚’À¢$‡B’"À¢’À¢òòF†RöæRF†RæöFR'VFvWBÖ—76VC¢F†R&wVÖVçBFöW2æ÷BvW@¢òò$”ttU"Â—BvWG2DTUU"Âc‚W"ÆWfVÂÂVçF–ÂF†R7G'V7GW&À¢òòvÆ²'Vç2÷WBöb7F6²à¢€¢&&VÆF–öâv†÷6R&wVÖVçBFVWVç2"À¢f÷&ÖB€¢'&VÆF–öâ"‡#¢&W’Ò"‡·×'·Ò•Æâ"À¢"‚"ç&WVBƒc’À¢"’"ç&WVBƒc¢’À¢%"‡B’"À¢’À¢Ò°¢ÆWB7&2Òf÷&ÖB‚'¶†VG×·&VÇ7ÕÆç·Ò"ÂFW7B†6ÆÂ’“°¢ÆWBW'"Ò7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’æW‡V7EöW'"‚'c&VgW6W2"“°¢76W'B€¢f÷&ÖB‚'¶W''Ò"’æ6öçF–ç2‚&6öç7G&–çBgVæ7F–öâ6ÆÂæ÷B7W÷'FVB"’À¢'·v†GÓ¢¶W''Ò ¢“°¢òòD"Ô•"æÖW2F†R7GVÂ&ö&ÆVÒ&F†W"F†â–æ†W&—F–ærcw0¢òòvVæW&–2ÖW76vR(	B—G2÷vâ7–6ÆRFWFV7F÷"—2Væ6†ævVBà¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&W‡æG2–çFò—G6VÆb"’Â'·v†GÓ¢¶×6wÒ"“°¢Ð ¢òòF†RwV&B—2Æ–Ö—BÂæ÷B&ã¢6†–âf"FVWW"F†âç—F†–æp¢òò&VÂ7F–ÆÂW‡æG2ÂæBF†R–ææW&Ö÷7B&÷VæB7W'f—fW2FòF†P¢òòVÖ—GFVB2²²à¢ÆWBFVWÒf÷&ÖB‚'¶†VG×·ÕÆç·Ò"Â6†–âƒ3’ÂFW7B‚%#3‡B’"’“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fFVW’¢æW‡V7B‚&3ÖFVW6†–â7F–ÆÂVÖ—G2"¢æ6öçF–ç2‚"‡V–çCcE÷B“"’À¢'F†R–ææW&Ö÷7B&VÆF–öâw2&÷VæB&V6†W2F†R÷WGWB ¢“° ¢òòF†RwV&B—2Æ–Ö—Böâ$T5U%4”ôâÂæ÷Böâ6—¦R÷"æW7F–ærÂæ@¢òòF†W6R6öçG&öÇ26’6ó¢6†–âöbCF—7F–æ7B&VÆF–öç2æBà¢òòW‡&W76–öâc&Vç2FVW&÷F‚7F–ÆÂVÖ—Bv—F‚F†R–ææW&Ö÷7@¢òò&÷VæB–çF7BâF†RFWF‚&6·7F÷—2v†Bv÷VÆB&VgW6RF†R6†–à¢òòBcB(	BFVÆ–&W&FRÂæBF†R6÷'W2w2FVWW7B&VÂæW7B—22à¢f÷"‡v†BÂ7&2’–â°¢€¢&CÖFVW6†–âöbF—7F–æ7B&VÆF–öç2"À¢f÷&ÖB‚'¶†VG×·ÕÆç·Ò"Â6†–âƒC’ÂFW7B‚%#C‡B’"’’À¢’À¢€¢&c×&VâW‡&W76–öâ"À¢f÷&ÖB€¢'¶†VG×&VÆF–öâ"‡#¢&W’Ò·×"çfÇVW·ÒâÆåÆç·Ò"À¢"‚"ç&WVBƒc’À¢"’"ç&WVBƒc’À¢FW7B‚%"‡B’"¢’À¢’À¢Ò°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·v†GÓ¢¶WÒ"’¢æ6öçF–ç2‚"‡V–çCcE÷B“"’À¢'·v†GÓ¢F†R–ææW&Ö÷7B&÷VæB&V6†W2F†R÷WGWB ¢“°¢Ð§Ð ¢òòò6öç7G&–çB%T”ÅD”â—2æ÷BâVæ¶æ÷vâ&VÆF–öâà¢òòð¢òòòWfW'’æÖR‚âââ–v—F‚â–FVçF6ÆÆVR–â6öç7G&–çBv2&÷WFV@¢òòòF‡&÷Vv‚F†R&VÆF–öâF‚ÂæBæÖRF†B—2æ÷BFV6Æ&V@¢òòò&VÆF–öâv2&W÷'FVB2öæRâc†æFÆW26ÖÆÂ6WBöbF†W6P¢òòò—G6VÆb(	B7VÒƒÆÆ—7Cå¶Æòâæ†•Ò–Â&VBöf`¢òòò7÷F#£§G'•öVÖ—Eö6öç7G&–çEöÆ—7Eö6ÆÆ(	B6ò6ÆÆ–ær—BâVæ¶æ÷và¢òòò&VÆF–öâ—2fÇ6R&VgW6Âöb&öw&Òc6ö×–ÆW2Âv—F‚¢òòòF–væ÷7F–2F†B6VæG2F†R&VFW"Æöö¶–ærf÷"&VÆF–öæ ¢òòòFV6Æ&F–öâF†W’æWfW"ÖVçBFòw&—FRà¢òòð¢òòòöæÇ’F†RdÅ4RÕ$TeU4Â66R—2ÆFVçB(	BâV&Æ–W"fW'6–öâöbF†—0¢òòòFö26–BF†Rv†öÆRF†–ærv2âD"Ô•"&VgW6W2ç’G&ç67F–öâ6''––æp¢òòòÆ—7CÅCæf–VÆB&Vf÷&R6öç7G&–çBÆ÷vW&–ær'Vç2ÂæBWfW'’7VÖ ¢òòòc44UE2æVVG2Æ—7Bf–VÆBÂ6òæòcÖ6ö×–Æ–ær&öw&Ò&V6†W0¢òòòF†Rf—‚FöF’âF†B—2v‡’F†RÆ—7BÖ&V&–ær66W2&R76W'FVBöà¢òòòF†R6öç7G&–çBF&ÆS¢VæB×FòÖVæBF†W&Rv÷VÆB72f÷"F†Rw&öæp¢òòò&V6öâÂ6–æ6RF†RÆ—7BÖf–VÆBvFRf—&W2f—'7BæBv÷VÆB¶VW76–æp¢òòò†÷vWfW"F†—2vW&R6Æ76–f–VBà¢òòð¢òòò'WB7VÖ÷fW"44Ä"&V6†W2F†Rf—†VBÆ–æRFöF’Âv—F‚æòÆ—7@¢òòòf–VÆBç—v†W&RÂæBF†B66R—276W'FVBVæBFòVæB(	B&V6W6RF†P¢òòò&VF–6FR6†V6·2äÔRæB$•E’öæÇ’ÂFVÆ–&W&FVÇ’v–FW"F†âcÀ¢òòòv†–6‚Ç6ò&WV—&W2&ævR×6Æ–6VBÆ—7Bf–VÆBâv†B¶VW2F†P¢òòòv–FVæ–ær6fR—2æ÷BF†R&VF–6FR'WBF†R6†&VBVÖ—GFW"ÂæB¢òòò&VF–6FRF†B—26fRöæÇ’&V6W6Röbv†B†Vç2F÷vç7G&VÒæVVG0¢òòòF†RF÷vç7G&VÒ76W'FVBà¢5·FW7EÐ¦fâ÷cö6öç7G&–çEö'V–ÇF–åö—5öæ÷E÷&W÷'FVEö5öå÷Væ¶æ÷vå÷&VÆF–öâ‚’°¢ÆWB7&2ÒÆ6ÆW6S¢g7G'Â°¢f÷&ÖB€¢&FöÖ–âEÆâg&WöÖ‡£¢ÆæVæBFöÖ–âEÆåÆåÀ¢G&ç67F–öâÆââ¢V–çCÃƒåÆâ—FV×2¢Æ—7CÇV–çCÃƒãåÆåÀ¢VæBG&ç67F–öâÆåÆåÀ¢FW7BEÆâÆWBGWB¢F÷ÆâÆWB¢Æâ6Æö6²6Æ²ÒEÆâ'VåÆåÀ¢Çƒ#&æFöÖ—¦R‡’v—F…Æâ¶6ÆW6WÕÆâVæB&æFöÖ—¦UÆåÀ¢Çƒ#VæB'VåÆæVæBFW7BEÆâ ¢¢Ó°¢òòWfW'’VçG'’w2W'&÷'2ÂfÆGFVæVBÂv—F‚F†R&VÆF–öâöæW2Ö&¶VBà¢ÆWBW''2ÒÆ6ÆW6S¢g7G'ÂÓâfV3Â†&ööÂÂ7G&–ær“â°¢†&3£§6öÇfW#£§&ö&ÆVÕ÷F&ÆS£¦'V–ÆE÷G—VE÷6öÇfW%÷&ö&ÆVÕ÷F&ÆR‚fÖW&vVE÷7&2‚g7&2†6ÆW6R’’¢æVçG&–W0¢æ—FW"‚¢æf–ÇFW%öÖ‡ÆWÂÖF6‚fRæ'V–ÆB°¢†&3£§6öÇfW#£§&ö&ÆVÕ÷F&ÆS£¥G—VE6öÇfW%&ö&ÆVÔ'V–ÆC£¤Æ÷vW$W'&÷"‡b’Óâ6öÖR‡b’À¢òÓâæöæRÀ¢Ò¢æfÆGFVâ‚¢æÖ‡ÆWÂ†Ræ—5÷&VÆF–öåöW'&÷"‚’Âf÷&ÖB‚'¶S£÷Ò"’’¢æ6öÆÆV7B‚¢Ó° ¢òòF†RÆ—7BÖf–VÆBvFR&VÆÇ’FöW2f—&Rf—'7BÂf÷"WfW'’öæRö`¢òòF†W6R(	B–æ6ÇVF–ærF†R6ÆW6Rv—F‚æò6ÆÂ–â—BBÆÂâF†B—0¢òòF†RÖ6¶–ærÂ7FFVB2ÖV7W&VÖVçB&F†W"F†â77VÖVBà¢f÷"6ÆW6R–â°¢'7VÒ†—FV×5³ââ—FV×2æÆVâ‚•Ò’ÓÒ"À¢$æõ7V6…&VÂ‡’"À¢'æââ2"À¢Ò°¢ÆWBW'"ÒÆ÷vW%÷7&2‚g7&2†6ÆW6R’’çVçw&öW'"‚“°¢76W'B€¢f÷&ÖB‚'¶W''Ò"’æ6öçF–ç2‚&æ—FV×6v—F‚âVç7W÷'FVB†æöâ×66Æ"’ÆVbG—R"’À¢'¶6ÆW6WÓ¢W‡V7FVBF†RÆ—7BÖf–VÆBvFRÂv÷B¶W''Ò ¢“°¢Ð ¢òòæBF†BvFRw2ÒÖ6öFVvVâc7VvvW7F–öâ—2†öæW7BÂv†–6‚—0¢òòv†BÖ¶W2F†—2v÷'F‚f—†–ær&F†W"F†âf–Æ–ær2Vç&V6†&ÆS ¢òòv—fRF†RÆ—7B&÷VæBæBcVÖ—G2F†Rv†öÆRF†–ærÂ7VÖ6ÆÀ¢òò–æ6ÇVFVBâ6òF†RfÇ6RVæ¶æ÷vå&VÆF–öæ6BF—&V7FÇ’–âg&öçBö`¢òòf÷&Òc6ö×–ÆW2à¢ÆWB&÷VæFVBÒ7&2‚&—FV×2æÆVâ‚’ÃÒEÆâ7VÒ†—FV×5³ââ—FV×2æÆVâ‚•Ò’ÓÒ"“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f&÷VæFVB’’æW‡V7B‚'cVÖ—G2&÷VæFVBÆ—7Bv—F‚7VÖ6öç7G&–çB"“° ¢òò7VÖ—2c'V–ÇF–ã¢æ÷B&VÆF–öâW'&÷"Â6ò—B—2F—66&FV@¢òòæBF†R&öw&Òv÷VÆBÆ÷vW"öæ6RÆ—7Bf–VÆG2Fòà¢f÷"6ÆW6R–â°¢'7VÒ†—FV×5³ââ—FV×2æÆVâ‚•Ò’ÓÒ"À¢'7VÒ†—FV×5³ââ—FV×2æÆVâ‚•Ò’"À¢Ò°¢ÆWBv÷BÒW''2†6ÆW6R“°¢76W'B€¢v÷Bæ—5öV×G’‚’bbv÷Bæ—FW"‚’æÆÂ‡Â†—5÷&VÂÂò—Â—5÷&VÂ’À¢&¶6ÆW6WÖ×W7B&öGV6Ræò&VÆF–öâW'&÷#¢¶v÷C£÷Ò ¢“°¢Ð ¢òòæÖRF†B—2æV—F†W"&VÆF–öâæ÷"c'V–ÇF–â7F—2¢òò&VÆF–öâW'&÷"âc&V¦V7G2—BFöò‚&6öç7G&–çBgVæ7F–öâ6ÆÂæ÷@¢òò7W÷'FVB–âc6öÇfW"F‚"’Â6ò&VgW6–ær—2F†R&–v‡BfW&F–7@¢òòWfVâv†VâF†RæÖRv2æWfW"ÖVçB2&VÆF–öâà¢f÷"6ÆW6R–â²$æõ7V6…&VÂ‡’"Â&æ÷7V6†fâ‡æâ’ÓÒ"Â'7VÒ‡æâÂæâ’ÓÒ%Ò°¢ÆWBv÷BÒW''2†6ÆW6R“°¢76W'B€¢v÷Bæ—FW"‚’æç’‡Â†—5÷&VÂÂò—Â¦—5÷&VÂ’À¢&¶6ÆW6WÖ×W7B7F–ÆÂ&R&VÆF–öâW'&÷#¢¶v÷C£÷Ò ¢“°¢Ð ¢òò&VÆF–öâv†÷6RæÖR6†F÷w2F†R'V–ÇF–â'WBF¶W2F–ffW&Vç@¢òò&—G’âcw2W‡æFW"FV6Æ–æW2öâF†R&—G’æB—G2Æ—7BÖ7VÖ ¢òò'V–ÇF–âF¶W2÷fW"Â6òcTÔ•E2(	B&W÷'F–ærâ&—G’Ö—6ÖF6€¢òòv÷VÆB&RF†R6ÖRfÇ6R&VgW6ÂöæR6†RgW'F†W"÷WBà¢ÆWB6†F÷vVBÒf÷&ÖB€¢'&VÆF–öâ7VÒ†¢Â#¢’ÒæââÆåÆç·Ò"À¢7&2‚&—FV×2æÆVâ‚’ÃÒEÆâ7VÒ†—FV×5³ââ—FV×2æÆVâ‚•Ò’ÓÒ"¢“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g6†F÷vVB’’æW‡V7B‚'cVÖ—G2v†Vâ&VÆF–öâ6†F÷w27VÖ"“°¢ÆWBv÷C¢fV3Â†&ööÂÂ7G&–ær“âÐ¢†&3£§6öÇfW#£§&ö&ÆVÕ÷F&ÆS£¦'V–ÆE÷G—VE÷6öÇfW%÷&ö&ÆVÕ÷F&ÆR‚fÖW&vVE÷7&2‚g6†F÷vVB’¢æVçG&–W0¢æ—FW"‚¢æf–ÇFW%öÖ‡ÆWÂÖF6‚fRæ'V–ÆB°¢†&3£§6öÇfW#£§&ö&ÆVÕ÷F&ÆS£¥G—VE6öÇfW%&ö&ÆVÔ'V–ÆC£¤Æ÷vW$W'&÷"‡b’Óâ6öÖR‡b’À¢òÓâæöæRÀ¢Ò¢æfÆGFVâ‚¢æÖ‡ÆWÂ†Ræ—5÷&VÆF–öåöW'&÷"‚’Âf÷&ÖB‚'¶S£÷Ò"’’¢æ6öÆÆV7B‚“°¢76W'B€¢v÷Bæ—FW"‚’æÆÂ‡Â†—5÷&VÂÂò—Â—5÷&VÂ’À¢&&VÆF–öâ6†F÷v–ær7VÖBF†R'V–ÇF–âw2&—G’—2æ÷B&VÆF–öâW'&÷#¢¶v÷C£÷Ò ¢“° ¢òòTäBDòTäBÂöâG&ç67F–öâv—F‚äòÆ—7Bf–VÆBÂv†–6‚&V6†W0¢òòF†Rf—†VBÆ–æRFöF’â7VÖ÷fW"66Æ"—2cW'&÷"F†—0¢òò&VF–6FRFVÆ–&W&FVÇ’vfW2F‡&÷Vvƒ²v†B&VgW6W2—B—2F†P¢òò6†&VBVÖ—GFW"Â–âcw2÷vâv÷&G2â&Vf÷&RF†Rf—‚F†RW6W"v÷@¢òò&7VÖæÖW2æò&VÆF–öæFV6Æ&VB–âF†—2f–ÆR"–ç7FVB(	@¢òò67W&FRF–væ÷7F–2Â6ÖRfW&F–7Bà¢ÆWB66Æ"Ò&FöÖ–âEÆâg&WöÖ‡£¢ÆæVæBFöÖ–âEÆåÆåÀ¢G&ç67F–öâÆââ¢V–çCÃƒåÆæVæBG&ç67F–öâÆåÆåÀ¢FW7BEÆâÆWBGWB¢F÷ÆâÆWB¢Æâ6Æö6²6Æ²ÒEÆâ'VåÆåÀ¢Çƒ#&æFöÖ—¦R‡’v—F…Æâ7VÒ‡æâ’ÓÒÆâVæB&æFöÖ—¦UÆåÀ¢Çƒ#VæB'VåÆæVæBFW7BEÆâ#°¢ÆWBÖW&vVBÒÖW&vVE÷7&2‡66Æ"“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVB’æW‡V7EöW'"‚'c&VgW6W27VÖ÷fW"66Æ""“°¢76W'B€¢f÷&ÖB‚'·cÒ"’æ6öçF–ç2‚&6öç7G&–çBgVæ7F–öâ6ÆÂæ÷B7W÷'FVB–âc6öÇfW"F‚"’À¢'·cÒ ¢“°¢òòD"Ô•"Æ÷vW'2—B(	BF†R&VgW6Â†2ÔõdTBÂæ÷BF—6V&VBà¢ÆWB&öw&ÒÒÆ÷vW%÷7&2‡66Æ"’æW‡V7B‚&7VÖ÷fW"66Æ"æ÷rÆ÷vW'2"“°¢fW&–g“£§fW&–g•÷&öw&Ò‚g&öw&Ò’æW‡V7B‚'fW&–f–W2"“°¢ÆWBF"ÒF&—#£¦VÖ—B‚g&öw&ÒÂfÖW&vVBÂf7÷F#£¤VÖ—D÷G3£¦FVfVÇB‚’¢æW‡V7EöW'"‚'F†R6†&VBVÖ—GFW"&VgW6W2—B"“°¢76W'B€¢f÷&ÖB‚'·F'Ò"’æ6öçF–ç2‚&6öç7G&–çBgVæ7F–öâ6ÆÂæ÷B7W÷'FVB–âc6öÇfW"F‚"’À¢%D"Ô•"×W7B&VgW6R–âcw2÷vâv÷&G2Âæ÷B–çfVçBöæR&÷WB&VÆF–öç3¢·F'Ò ¢“°§Ð ¢òòòÆövff–æG2—G2ÖW76vR÷6—F–öæÆÇ’ÂF†Rv’cFöW2à¢òòð¢òòòc4ôå5TÔU2F†RF‚(	B7F×D¶–æC£¤Æötf7Æ—G2F†Rf—'7B÷6—F–öæÀ¢òòò7G&–ær÷WBöbF†R&wVÖVçBÆ—7BæB†æG2VÖ—EöÆövv†B—2ÆVgB(	@¢òòò6ò—G2'VÆR—2÷6—F–öæÂâD"Ô•"6ö×&VBV6‚7G&–ærw2dÅTP¢òòòv–ç7BF†RF‚–ç7FVBÂv†–6‚v—fW2F†R6ÖRç7vW"öæÇ’v†–ÆRF†P¢òòòÖW76vR†Vç2FòF–ffW"g&öÒF†RF‚à¢òòð¢òòòF†—2v2F—fW&vVæ6RSƒ¢Æ—fR6–ÆVçBD•dU$tTä4RÂæ÷B6†&V@¢òòòÖ—2ÖÆ÷vW&–ærâ&÷F‚&6¶VæG266WB&÷F‚&öw&×2&VÆ÷ræBVÖ—GFV@¢òòòF–ffW&VçBFW‡Bà¢5·FW7EÐ¦fâÆöve÷F¶W5ö—G5÷F…ö'•÷÷6—F–öåöæ÷Eö'•÷fÇVR‚’°¢ÆWB7&2ÒÇ7F×C¢g7G'Â°¢f÷&ÖB€¢&FöÖ–âEÆâg&WöÖ‡£¢ÆæVæBFöÖ–âEÆåÆåÀ¢FW7BEÆâÆWBGWB¢F÷Æâ6Æö6²6Æ²ÒEÆâ'VåÆåÀ¢Çƒ#·7F×GÕÆâv—B7–6ÆUÆâVæB'VåÆæVæBFW7BEÆâ ¢¢Ó°¢òòF†RVÖ—GFVBÆör6ÆÂÂf÷"v†–6†WfW"&6¶VæBâF†R6VVCÖÆ–æR—0¢òòF†R†&æW72&VÖ&ÆRWfW'’FW7BVÖ—G2Âæ÷BF†R7FFVÖVçBVæFW ¢òòFW7B(	BG&÷–ær—B'’æÖR&F†W"F†â'’÷6—F–öâ6ò6†ævR–à¢òò&VÖ&ÆR÷&FW&–ærf–Ç2Æ÷VFÇ’–ç7FVBöb6–ÆVçFÇ’6VÆV7F–ærF†P¢òòw&öærÆ–æRà¢ÆWBÆ–æRÒÆ÷WC¢g7G'ÂÓâ7G&–ær°¢÷WBæÆ–æW2‚¢æf–ÇFW"‡ÆÇÂÂæ6öçF–ç2‚'6–ÕöÆöveöÆ–æR‚"’ÇÂÂæ6öçF–ç2‚'6–ÕöÆöuöÆ–æR…Â""’¢æf–ÇFW"‡ÆÇÂÂæ6öçF–ç2‚'6VVCÒ"’¢æÖ‡ÆÇÂÂçG&–Ò‚’çFõ÷7G&–ær‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚¢æ¦ö–â‚"³²"¢Ó° ¢f÷"‡v†BÂ7F×BÂW‡V7FVB’–â°¢òòF—fW&vVæ6RS‚w2Gvò66W2âF†RÖW76vRUTÅ2F†RF‚Â6ò¢òòfÇVR6ö×&—6öâ6¶—2—BæBÆæG2öâF†RæW‡B7G&–ærà¢€¢&ÖW76vRWVÂFòF†RF‚"À¢"2&Æövb‚'BæÆör"Â'BæÆör"ÂW'&÷"Â$$ôôÒ"’"2À¢"2'6–ÕöÆöveöÆ–æR†Æöuö7G‚æf–ÆR‚'BæÆör"’Â$U%$õ""Â'BæÆör"“²"2À¢’À¢€¢'F†RF‚&WVFVB2F†RÖW76vR"À¢"2&Æövb‚'BæÆör"ÂW'&÷"Â'BæÆör"’"2À¢"2'6–ÕöÆöveöÆ–æR†Æöuö7G‚æf–ÆR‚'BæÆör"’Â$U%$õ""Â'BæÆör"“²"2À¢’À¢òò6öçG&öÇ3¢F†R6†W2F†BÇ&VG’w&VVB×W7B¶VWw&VV–ærà¢€¢&â÷&F–æ'’Æövb"À¢"2&Æövb‚'BæÆör"ÂW'&÷"Â$$ôôÒ"’"2À¢"2'6–ÕöÆöveöÆ–æR†Æöuö7G‚æf–ÆR‚'BæÆör"’Â$U%$õ""Â$$ôôÒ"“²"2À¢’À¢€¢&6WfW&—G’w&—GFVâgFW"F†RÖW76vR"À¢"2&Æövb‚'BæÆör"Â$$ôôÒ"ÂW'&÷"’"2À¢"2'6–ÕöÆöveöÆ–æR†Æöuö7G‚æf–ÆR‚'BæÆör"’Â$U%$õ""Â$$ôôÒ"“²"2À¢’À¢€¢&F†—&B7G&–ærÂv†–6‚—2–væ÷&VB"À¢"2&Æövb‚'BæÆör"Â$"Â$""’"2À¢"2'6–ÕöÆöveöÆ–æR†Æöuö7G‚æf–ÆR‚'BæÆör"’Â$”ädò"Â$"“²"2À¢’À¢òòÆ–âÆöv6öç7VÖW2æòF‚Â6ò—G2ÖW76vR—2F†Rd•%5@¢òò7G&–ær(	BF†R6ÖR6öFRæ÷r†2FòvWB&÷F‚66W2&–v‡Bà¢€¢&Æ–âÆör"À¢"2&Æör†W'&÷"Â$$ôôÒ"’"2À¢"2'6–ÕöÆöuöÆ–æR‚$U%$õ""Â$$ôôÒ"“²"2À¢’À¢€¢&Æ–âÆörv—F‚6V6öæB7G&–ær"À¢"2&Æör†W'&÷"Â$"Â$""’"2À¢"2'6–ÕöÆöuöÆ–æR‚$U%$õ""Â$"“²"2À¢’À¢Ò°¢ÆWBÖW&vVBÒÖW&vVE÷7&2‚g7&2‡7F×B’“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVB’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·v†GÓ¢cVÖ—G3¢¶WÒ"’“°¢ÆWB&öw&ÒÒÆ÷vW%÷7&2‚g7&2‡7F×B’’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·v†GÓ¢Æ÷vW'3¢¶WÒ"’“°¢ÆWBF"ÒF&—#£¦VÖ—B‚g&öw&ÒÂfÖW&vVBÂf7÷F#£¤VÖ—D÷G3£¦FVfVÇB‚’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·v†GÓ¢F&—"VÖ—G3¢¶WÒ"’“°¢76W'EöW†Æ–æR‚gc’ÂW‡V7FVBÂ'·v†GÓ¢cw2÷vâ÷WGWBÖ÷fVB"“°¢76W'EöW†Æ–æR‚gF"’ÂÆ–æR‚gc’Â'·v†GÓ¢&6¶VæG2F—6w&VR"“°¢Ð§Ð ¢òòòF†RöæRÖ&wVÖVçB&V6÷&B’—2wV&FVBÆ–¶RWfW'’÷F†W"ÂæBF†P¢òòòwV&B&W÷'G2F†Rtõ%5B&wVÖVçB&F†W"F†âF†Rf—'7Bà¢òòð¢òòò&V6÷&E÷&VFv2F†RöæÇ’&V6÷&BÔ’6—FRF†B66WFVBâVæ¶æ÷và¢òòò&ÖWFW"æÖR6–ÆVçFÇ’âöæR&wVÖVçBFöW2æ÷BÖ¶RF†R6†V6°¢òòòö–çFÆW73¢æÖRÖF6†–æræ÷F†–ær—27F–ÆÂ&öw&ÒW'&÷"æð¢òòò&6¶VæB6â†öæ÷W"ÂæB&V6÷&E÷&VB‡&VrÒB–&VG2Æ–¶R—BæÖW0¢òòò6öÖWF†–ærà¢òòð¢òòòF†Rv÷'7BÖ&wVÖVçB†Æb—26W&FR6Æ–ÒâF†RGvòfW&F–7G2&P¢òòòæ÷BWVÆÇ’&BæBF†R&wVÖVçG2&Ræ÷BW†Ö–æVB–â÷&FW"ö`¢òòò&FæW72Â6ò&WGW&æ–æröâF†Rf—'7BöæRf÷VæBÆWBâVæ¶æ÷vâæÖP¢òòò†–FRvVçV–æR7v&V†–æB—B(	BF†R6–ÆVçBÖ—2ÖÆ÷vW&–ær&V–ærF†P¢òòòw&fW"öbF†RGvòà¢5·FW7EÐ¦fâF†U÷&V6÷&Eö•öwV&E÷&W÷'G5÷F†U÷v÷'7Eö&wVÖVçEöæ÷E÷F†Uöf—'7B‚’°¢6öç7B$TC¢g7G"Ò&ÆWB6Ò&Vw2ç&V6÷&E÷&VBƒƒ‚’#°¢6öç7Bu$•DS¢g7G"Ò'&Vw2ç&V6÷&E÷w&—FRƒƒ‚Â3SC“ƒ“b’#°¢ÆWBf—‡GW&U÷7&2Òf—‡GW&R‚'&Vv&Æö6µ÷&V6÷&Eö•÷FW7Bæ†&2"“°¢76W'B€¢f—‡GW&U÷7&2æ6öçF–ç2…$TB’bbf—‡GW&U÷7&2æ6öçF–ç2…u$•DR’À¢&f—‡GW&R6†R6†ævVB ¢“°¢òòF†Rf—‡GW&RW6V27FFÆ–"'W2Â6ò—BöæÇ’Æ÷vW'2Æöæw6–FRF†P¢òò'W2FV6Æ&F–öâ(	B6ÖR2F†R4Ä’w2&W6öÇfU÷W6Uö–×÷'G6à¢ÆWB'W5÷7&2Ò7FC£¦g3£§&VE÷Fõ÷7G&–ær€¢Fƒ£¦æWr†Vçb‚$4$tõôÔä”dU5EôD•""’¢æ¦ö–â‚'7FFÆ–""¢æ¦ö–â‚$'W4†”Æ—FRæ&6‚"’À¢¢æW‡V7B‚'&VB7FFÆ–"'W2"“°¢ÆWBÖW&vU÷v—F…ö'W2ÒÇ7&3¢g7G'Â°¢ÖW&vS£¦ÖW&vUöf÷%÷6–Ò€¢fV2°¢'6U÷6÷W&6R‡7&2’æW‡V7B‚&f—‡GW&R'6W2"’À¢'6U÷6÷W&6R‚f'W5÷7&2’æW‡V7B‚'7FFÆ–"'W2'6W2"’À¢ÒÀ¢æöæRÀ¢¢æW‡V7B‚&ÖW&vR"¢Ó°¢ÆWBÆ÷vW%ö—BÒÇ7&3¢g7G'ÂÆ÷vW#£¦Æ÷vW%÷&öw&Ò‚fÖW&vU÷v—F…ö'W2‡7&2’“°¢ÆWBv—F‚ÒÆöÆC¢g7G"ÂæWs¢g7G'Âf—‡GW&U÷7&2ç&WÆ6Vâ†öÆBÂæWrÂ“° ¢òòF†Rf—‡GW&R—2v÷&¶–ær&öw&ÒVæFW"&÷F‚&6¶VæG3¢F†R6öçG&öÀ¢òòF†B¶VW2WfW'’76W'F–öâ&VÆ÷rg&öÒ76–ærf÷"F†Rw&öæp¢òò&V6öâà¢Æ÷vW%ö—B‚ff—‡GW&U÷7&2’æW‡V7B‚'F†RVæÖöF–f–VBf—‡GW&RÆ÷vW'2"“° ¢òò&V6÷&E÷&VFÂöæR&wVÖVçBÂæÖR–â—G2÷vâ÷6—F–öã¢–æW'BÂæ@¢òò×W7B7F–ÆÂÆ÷vW"à¢Æ÷vW%ö—B‚gv—F‚…$TBÂ&ÆWB6Ò&Vw2ç&V6÷&E÷&VB†FG"Òƒ‚’"’¢æW‡V7B‚&6÷'&V7FÇ’ÖæÖVB6–ævÆR&wVÖVçBÆ÷vW'2"“° ¢òòæÖRÖF6†–æræò&ÖWFW"—2–çfÆ–F(	Bc&–æG2'’÷6—F–öà¢òòæBVÖ—G2W†7FÇ’F†R&–v‡B6öFRÂ6ò6Æ–Ö–ær—BÖ—2ÖÆ÷vW'2v÷VÆ@¢òò&RfÇ6RW‡ÆæF–öâà¢òòäõB&VrÒƒ†¢&Vv—2ÆW†W"¶W—v÷&BÂ6òF†B&öw&ÒFöW0¢òòæ÷B'6RæBF†R76W'F–öâv÷VÆB&RÖV7W&–ærF†R'6W"âF†P¢òò6ÖRG&Ç&VG’6÷7BF†—2wV&Böæ6RÂv†Vâ&V6÷&E÷w&—FVv0¢òòv—fVââ–çfVçFVB²'&Vr"Â'fÇVR%ÖÆ—7Bv†÷6Rf—'7BVçG'’6÷VÆ@¢òòæWfW"ÖF6‚'6V&ÆR&öw&Òà¢76W'B€¢'6U÷6÷W&6R‚gv—F‚…$TBÂ&ÆWB6Ò&Vw2ç&V6÷&E÷&VB‡&VrÒƒ‚’"’’æ—5öW'"‚’À¢&&Vv—2¶W—v÷&C²–bF†—27F'G2'6–ærÂF†RW†×ÆR&VÆ÷r6âW6R—B ¢“°¢ÆWB×6rÒ76W'Eö–çfÆ–B€¢fÆ÷vW%ö—B‚gv—F‚…$TBÂ&ÆWB6Ò&Vw2ç&V6÷&E÷&VB†æ÷7V6‚Òƒ‚’"’’çVçw&öW'"‚’À¢“°¢76W'B€¢×6ræ6öçF–ç2‚&æ÷7V6†æÖW2æò&ÖWFW"öb&V6÷&E÷&VB‚âââ–6ÆÂ"¢bb×6ræ6öçF–ç2‚&W‡V7FVBFG&"’À¢'¶×6wÒ ¢“° ¢òòF†Rv÷'7BÖ&wVÖVçB66RÂöâF†RGvòÖ&wVÖVçB6–&Æ–æs¢âVæ¶æ÷và¢òòæÖRd•%5BæBvVçV–æR7v4T4ôäBâF†R7v×W7Bv–âà¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%ö—B‚gv—F‚€¢u$•DRÀ¢'&Vw2ç&V6÷&E÷w&—FR†æ÷7V6‚Òƒ‚ÂFG"Ò3SC“ƒ“b’"À¢’¢çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B€¢×6ræ6öçF–ç2‚&FG&—2&ÖWFW"†W&R'WBv2w&—GFVâ–â÷6—F–öâ""’À¢'F†R7v×W7B÷WG&æ²F†RVæ¶æ÷vâæÖS¢¶×6wÒ ¢“° ¢òòv—F‚æò7v&W6VçBF†RVæ¶æ÷vâæÖR—27F–ÆÂ&W÷'FVBÂ6ð¢òò&VfW'&–ærF†R7vF–Bæ÷B6–ÆVæ6R—Bà¢ÆWB×6rÒ76W'Eö–çfÆ–B€¢fÆ÷vW%ö—B‚gv—F‚…u$•DRÂ'&Vw2ç&V6÷&E÷w&—FR†æ÷7V6‚Òƒ‚Â3SC“ƒ“b’"’’çVçw&öW'"‚’À¢“°¢76W'B†×6ræ6öçF–ç2‚&æ÷7V6†æÖW2æò&ÖWFW""’Â'¶×6wÒ"“° ¢òòæBc66WG2&÷F‚æÖVB&wVÖVçG2æB&–æG2F†VÒ'’÷6—F–öâÀ¢òòv†–6‚—2v†BÖ¶W2F†R7vF†Rw&fW"fW&F–7BöbF†RGvòà¢7÷F#£¦VÖ—B‚fÖW&vU÷v—F…ö'W2‚gv—F‚€¢u$•DRÀ¢'&Vw2ç&V6÷&E÷w&—FR†æ÷7V6‚Òƒ‚ÂFG"Ò3SC“ƒ“b’"À¢’’¢æW‡V7B‚'c66WG2&÷F‚æÖVB&wVÖVçG2"“°§Ð ¢òòòæÖVB&wVÖVçBw&—GFVâ–â—G2÷vâ÷6—F–öâ—2–æW'BÂæBF‡&VP¢òòò6ÆÂfÖ–Æ–W2æ÷r6’6ò–ç7FVBöb&VgW6–ærF†Rv†öÆR6öç7G'V7Bà¢òòð¢òòòcG&÷2&wVÖVçBæÖW2æB&–æG27G&–7FÇ’'’÷6—F–öââÖV7W&VBf÷ ¢òòòV6‚fÖ–Ç’&VÆ÷rÂ6ö×&–ærVÖ—GFVB2²³ ¢òòð¢òòòÂ6ÆÂÂcVÖ—G2À¢òòòÂÒÒ×ÂÒÒ×À¢òòòÂ†ÇƒÂ##"–Â†ÇƒÂ##"–À¢òòòÂ†Ç†ÒÂ"Ò##"–Â†ÇƒÂ##"–À¢òòòÂ†Ç†"Ò##"ÂÒ–Â†Çƒ##"Â–À¢òòð¢òòò6òF†R–âÖ÷&FW"f÷&Ò—2'—FRÖ–FVçF–6ÂFòF†R÷6—F–öæÂöæR(	Bc¢òòò6öçG&–'WFW2æ÷F†–ær'’G&÷–ærF†RæÖR(	BæBöæÇ’F†R&V÷&FW&V@¢òòòf÷&Ò6–ÆVçFÇ’7v2F†RfÇVW2â&VgW6–ærWfW'’æÖVB&wVÖVç@¢òòò&VgW6VBF†R–æW'Bf÷&ÒFöòà¢òòð¢òòòV6‚&ÖWFW"Æ—7B—2&VBöfbF†RDT4Ä$D”ôâF†R6ÆÂ&W6öÇfW0¢òòòFòÂæWfW"w&—GFVâg&öÒÖVÖ÷'“¢&V6÷&E÷w&—FVv2öæ6Rv—fVâà¢òòò–çfVçFVB²'&Vr"Â'fÇVR%ÖæB&VgW6VBF†RFö7VÖVçFVBf÷&Ò÷WG&–v‡Bà¢5·FW7EÐ¦fâöæÖVEö&wVÖVçEö–åö—G5ö÷vå÷÷6—F–öåöÆ÷vW'5öf÷%÷F†UöfÖ–Æ–W5÷F†Eö¶æ÷u÷F†V—%÷&ÖWFW'2‚’°¢6öç7BC¢g7G"Ò&FöÖ–âEÆâg&WöÖ‡£¢ÆæVæBFöÖ–âEÆåÆâ#°¢òò†fÖ–Ç’Â6÷W&6RFV×ÆFRv—F‚¶6ÆÇÒÂF†RVÖ—GFVBFW‡BFòÖF6‚¢ÆWB†VÇW"ÒÆ6ÆÃ¢g7G'Â°¢f÷&ÖB€¢'´GÖgVæ7F–öâ†Ç†¢V–çCÃ3#âÂ#¢V–çCÃ3#â•ÆâÆör†–æfòÂÂ&‚G·¶×ÒG·¶'×ÕÂ"•ÆåÀ¢VæBgVæ7F–öâ†ÇÆåÆåÀ¢FW7BEÆâÆWBGWB¢F÷Æâ6Æö6²6Æ²ÒEÆâ'VåÆâ¶6ÆÇÕÆåÀ¢Çƒ#v—B7–6ÆUÆâVæB'VåÆæVæBFW7BEÆâ ¢¢Ó°¢ÆWBF%öÖWF†öBÒÆ6ÆÃ¢g7G'Â°¢f÷&ÖB€¢'´G×FW7F&Væ6‚F%ÆâGWB¢F÷ÆâgVæ7F–öâ†Ç†¢V–çCÃ3#âÂ#¢V–çCÃ3#â•ÆåÀ¢Çƒ#Æör†–æfòÂÂ&‚G·¶×ÒG·¶'×ÕÂ"•ÆâVæBgVæ7F–öâ†ÇÆæVæBFW7F&Væ6‚F%ÆåÆåÀ¢–×ÂBf÷"F%Æâ'VåÆâ¶6ÆÇÕÆâv—B7–6ÆUÆâVæB'VåÆæVæB–×ÂEÆâ ¢¢Ó°¢ÆWBW‡FW&åöfâÒÆ6ÆÃ¢g7G'Â°¢f÷&ÖB€¢'´GÖW‡FW&âgVæ7F–öâ&VeöFB†¢V–çCÃ3#âÂ#¢V–çCÃ3#â’ÓâV–çCÃ3#åÆåÆåÀ¢FW7BEÆâÆWBGWB¢F÷Æâ6Æö6²6Æ²ÒEÆâ'VåÆâÆWBbÒ¶6ÆÇÕÆåÀ¢Çƒ#Æör†–æfòÂÂ'bG··g×ÕÂ"•Æâv—B7–6ÆUÆâVæB'VåÆæVæBFW7BEÆâ ¢¢Ó° ¢f÷"†fÖ–Ç’Â7&2Â6ÆÆVRÂæVVFÆR’–â°¢€¢&†VÇW""À¢f†VÇW"2fG–âfâ‚g7G"’Óâ7G&–ærÀ¢&†Ç"À¢&†Ç‚"À¢’À¢‚&FW7F&Væ6‚ÖWF†öB"ÂgF%öÖWF†öBÂ&†Ç"Â%F%ö†Ç…÷F"Â"’À¢‚&âW‡FW&âfâ"ÂfW‡FW&åöfâÂ'&VeöFB"Â'&VeöFB‚"’À¢Ò°¢ÆWBVÖ—GFVBÒÆ6ÆÃ¢g7G'ÂÓâ7G&–ær°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2†6ÆÂ’’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'¶fÖ–Ç—Ó¢cVÖ—G2¶6ÆÇÖ¢¶WÒ"’¢æÆ–æW2‚¢æf–ÇFW"‡ÆÇÂÂæ6öçF–ç2†æVVFÆR’¢æÖ‡ÆÇÂÂçG&–Ò‚’çFõ÷7G&–ær‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚¢æ¦ö–â‚"³²"¢Ó° ¢òòcw2÷vâ&V†f–÷W"Âv†–6‚—2v†BF†R6Æ76–f–6F–öâ&W7G2öâà¢ÆWB÷6—F–öæÂÒVÖ—GFVB‚ff÷&ÖB‚'¶6ÆÆVWÒƒÂ##"’"’“°¢76W'EöW€¢VÖ—GFVB‚ff÷&ÖB‚'¶6ÆÆVWÒ†ÒÂ"Ò##"’"’’À¢÷6—F–öæÂÀ¢'¶fÖ–Ç—Ó¢F†R–âÖ÷&FW"f÷&Ò×W7B&R'—FRÖ–FVçF–6ÂFò÷6—F–öæÂ ¢“°¢76W'EöæR€¢VÖ—GFVB‚ff÷&ÖB‚'¶6ÆÆVWÒ†"Ò##"ÂÒ’"’’À¢÷6—F–öæÂÀ¢'¶fÖ–Ç—Ó¢F†R&V÷&FW&VBf÷&Ò×W7BF–ffW"(	BF†B—2F†R7v ¢“° ¢òòD"Ô•#¢÷6—F–öæÂæB–âÖ÷&FW"Æ÷vW#²öæÇ’F†R7v—2&VgW6VBà¢Æ÷vW%÷7&2‚g7&2‚ff÷&ÖB‚'¶6ÆÆVWÒƒÂ##"’"’’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'¶fÖ–Ç—Ó¢÷6—F–öæÂÆ÷vW'3¢¶WÒ"’“°¢Æ÷vW%÷7&2‚g7&2‚ff÷&ÖB‚'¶6ÆÆVWÒ†ÒÂ"Ò##"’"’’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'¶fÖ–Ç—Ó¢â–âÖ÷&FW"æÖRÆ÷vW'3¢¶WÒ"’“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g7&2‚ff÷&ÖB‚'¶6ÆÆVWÒ†"Ò##"ÂÒ’"’’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B€¢×6ræ6öçF–ç2‚&&—2&ÖWFW""†W&R'WBv2w&—GFVâ–â÷6—F–öâ"’À¢'¶fÖ–Ç—Ó¢¶×6wÒ ¢“° ¢òòæÖRÖF6†–æræò&ÖWFW"—2–çfÆ–F¢c&–æG2'¢òò÷6—F–öâæBVÖ—G2F†R&–v‡B6öFRÂ6ò—B—2&öw&ÒW'&÷"À¢òòæ÷BÖ—2ÖÆ÷vW&–ærà¢ÆWB×6rÐ¢76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚g7&2‚ff÷&ÖB‚'¶6ÆÆVWÒ†æ÷7V6‚ÒÂ##"’"’’’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚&æ÷7V6†æÖW2æò&ÖWFW"öb"’À¢'¶fÖ–Ç—Ó¢¶×6wÒ ¢“°¢Ð§Ð ¢òòòF†R6÷fW&w&÷W†VÇW"6ÆÂ—2F†RÆ7BæÖVBÖ&wVÖVçBfÖ–Ç’ÂæB—@¢òòò&W6öÇfW2—G2&ÖWFW"æÖW2F‡&÷Vv‚F–ffW&VçB&Vv—7G'’à¢òòð¢òòòÖV7W&VBf÷"D„•2fÖ–Ç’&F†W"F†â–æ†W&—FVBg&öÒ—G26–&Æ–æw3¢–à¢òòò6÷fW'ö–çBF&vWBÂ–6²†ÒââÂ"Ò–VÖ—G2F†R6ÖP¢òòò–6²ƒÇ6Æ–6SâÂ–cVÖ—G2÷6—F–öæÆÇ’ÂæB–6²†"ÒÂÒââ– ¢òòòVÖ—G2–6²ƒÂÇ6Æ–6Sâ–(	BF†RfÇVW27vVBÂ6–ÆVçFÇ’Â–ç6–FRF†P¢òòò6×ÆW"F†BFV6–FW2v†–6‚&–âvWG2†—Bà¢òòð¢òòòF†RæÖW26öÖRg&öÒv†–6†WfW"&Vv—7G'’&W6öÇfW2F†R6ÆÆVS¢¢òòòf–ÆRÖÆWfVÂgVæ7F–öæf–†VÇW%&Vv—7G'–Â÷"âW‡FW&âgVæ7F–öæ ¢òòòf–F†RW‡FW&âÖâv†VâæV—F†W"&W6öÇfW2F†W&R—2æò&ÖWFW"Æ—7@¢òòòFò6†V6²v–ç7BæBF†R6ÆÂ¶VW2—G2&Ææ¶WB&VgW6ÂÂ&V6W6P¢òòò&VgW6–ær&VG2wVW76–ærÆ—7Bà¢5·FW7EÐ¦fâö6÷fW&w&÷Wö†VÇW%ö6ÆÅö§VFvW5ööæÖVEö&wVÖVçEö'•ö—G5÷÷6—F–öâ‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&6÷eöW‡%÷F&vWG5÷FW7Bæ†&2"“°¢6öç7B5¢g7G"Ò"7öÆ÷uöæ–&&ÆR¢6÷fW"GWBæ6÷VçEö÷WE³3£Ò#°¢76W'B†f—‡GW&Ræ6öçF–ç2„5’Â&f—‡GW&R6†R6†ævVB"“°¢ÆWB7&2ÒÆ6ÆÃ¢g7G'Â°¢f÷&ÖB€¢&gVæ7F–öâ–6²†¢V–çCÃƒâÂ#¢V–çCÃƒâ’ÓâV–çCÃƒåÆâ&WGW&âÆåÀ¢VæBgVæ7F–öâ–6µÆåÆç·Ò"À¢f—‡GW&Rç&WÆ6Vâ„5Âff÷&ÖB‚"7öÆ÷uöæ–&&ÆR¢6÷fW"¶6ÆÇÒ"’Â¢¢Ó°¢ÆWBVÖ—GFVBÒÆ6ÆÃ¢g7G'ÂÓâ7G&–ær°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2†6ÆÂ’’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'cVÖ—G2¶6ÆÇÖ¢¶WÒ"’¢æÆ–æW2‚¢æf–ÇFW"‡ÆÇÂÂæ6öçF–ç2‚'–6²‚"’¢æÖ‡ÆÇÂÂçG&–Ò‚’çFõ÷7G&–ær‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚¢æ¦ö–â‚"³²"¢Ó° ¢òòcw2&V†f–÷W"Âv†–6‚F†R6Æ76–f–6F–öâ&W7G2öâà¢ÆWB÷6—F–öæÂÒVÖ—GFVB‚'–6²†GWBæ6÷VçEö÷WE³3£ÒÂ’"“°¢76W'B€¢÷6—F–öæÂæ6öçF–ç2‚'–6²‚"’À¢'F†R†VÇW"6ÆÂ&V6†W2cw2÷WGWB ¢“°¢76W'EöW€¢VÖ—GFVB‚'–6²†ÒGWBæ6÷VçEö÷WE³3£ÒÂ"Ò’"’À¢÷6—F–öæÂÀ¢&–âÖ÷&FW"æÖW2VÖ—BF†R÷6—F–öæÂ6ÆÂ'—FRÖf÷"Ö'—FR ¢“°¢76W'EöæR€¢VÖ—GFVB‚'–6²†"ÒÂÒGWBæ6÷VçEö÷WE³3£Ò’"’À¢÷6—F–öæÂÀ¢'&V÷&FW&VBæÖW27vF†RfÇVW2–ç6–FRF†R6×ÆW" ¢“° ¢òòD"Ô•#¢F†R–æW'Bf÷&×2Æ÷vW"à¢Æ÷vW%÷7&2‚g7&2‚'–6²†GWBæ6÷VçEö÷WE³3£ÒÂ’"’’æW‡V7B‚'÷6—F–öæÂÆ÷vW'2"“°¢Æ÷vW%÷7&2‚g7&2‚'–6²†ÒGWBæ6÷VçEö÷WE³3£ÒÂ"Ò’"’’æW‡V7B‚&–âÖ÷&FW"æÖW2Æ÷vW""“° ¢òòF†R7v—2F†R6–ÆVçBÖ—2ÖÆ÷vW&–ærà¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g7&2‚'–6²†"ÒÂÒGWBæ6÷VçEö÷WE³3£Ò’"’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B€¢×6ræ6öçF–ç2‚&6÷fW&w&÷W†VÇW"6ÆÂ–6²‚âââ–"¢bb×6ræ6öçF–ç2‚&&—2&ÖWFW""†W&R'WBv2w&—GFVâ–â÷6—F–öâ"’À¢'¶×6wÒ ¢“° ¢òòæÖRÖF6†–æræò&ÖWFW"—2&öw&ÒW'&÷"à¢ÆWB×6rÐ¢76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚g7&2‚'–6²†æ÷7V6‚ÒGWBæ6÷VçEö÷WE³3£ÒÂ"Ò’"’’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&æ÷7V6†æÖW2æò&ÖWFW"öb"’Â'¶×6wÒ"“°§Ð ¢òòò†öö¶VBöæ–â7FFVÖVçB÷6—F–öâ—2§VFvVB'’v†WF†W"—G2F€¢òòò$U4ôÅdU2Âæ÷B'’—G26†Rà¢òòð¢òòò—5÷cöÖWF†öEö†ööµ÷6†V66WG2ç’F÷GFVBF‚öbF†R&–v‡@¢òòòÆVæwF‚Â6òG'bç6VæBç†Âæ÷7V6‚ç6VæFÂG'bçÆ–ææBGWBç'7Bç† ¢òòòÆÂ&V6†VBF†RÒÖ6öFVvVâc7VvvW7F–öââÖV7W&VC¢c&VgW6W0¢òòòWfW'’öæRöbF†VÒ‚&ö&¢æÖWF†öB×W7B&W6öÇfRFò†öö¶&ÆVöâ¢òòò¶æ÷vâ6ö×öæVçBG—R"’v†–ÆRF†R&W6öÇf–ærG'bç6VæFVÖ—G2âF†P¢òòò7VvvW7F–öâv2†öæW7Bf÷"W†7FÇ’öæRöbF†Rf—fRà¢òòð¢òòòF†RvFR—2cw2÷vâ6öæF—F–öâæBF†R6ÖRöæRF†RFW7B×66÷R&Ð¢òòòÆ–W2â—B—26†V6¶VB–âF†R&V6÷fW&&ÆRF—&V7F–öã¢Ö—72––VÆG0¢òòòF†R†öæW7B&V¦V7G6Â†—BöæÇ’WfW"Ww&FW2FòF†R7VvvW7F–öâà¢5·FW7EÐ¦fâ÷7FFVÖVçE÷÷6—F–öåö†ööµö—5ö§VFvVEö'•÷&W6öÇWF–öåöæ÷E÷6†R‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&†–Æ—FUö†öö·5÷FW7Bæ†&2"“°¢6öç7Bä4„õ#¢g7G"Ò"Æör†–æfòÂÂ$†”Æ—FU&Vw2&R÷÷7B†öö·2FW7EÂ"’#°¢76W'B†f—‡GW&Ræ6öçF–ç2„ä4„õ"’Â&f—‡GW&R6†R6†ævVB"“°¢ÆWBv—F‚ÒÇFƒ¢g7G'Â°¢f—‡GW&Rç&WÆ6Vâ€¢ä4„õ"À¢ff÷&ÖB€¢'´ä4„õ'ÕÆâöâ·F‡Ò&UÆâ&Uö6÷VçBÒ&Uö6÷VçB²ÆåÀ¢Çƒ#VæBöâ ¢’À¢À¢¢Ó° ¢òòF†RöæRF†B&W6öÇfW3¢cTÔ•E2Â6òÒÖ6öFVvVâc—2&VÀ¢òòW66R†F6‚æBF†R&Ò¶VW2—G27VvvW7F–öâà¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gv—F‚‚&G'bç6VæB"’’’æW‡V7B‚'cVÖ—G2&W6öÇf–ær†öö²F‚"“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚gv—F‚‚&G'bç6VæB"’’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&†öö²–â7FFVÖVçB÷6—F–öâ"’Â'¶×6wÒ"“° ¢òòF†Rf÷W"F†BFòæ÷Bâ6ÖR4„RÂæBc&VgW6W2V6‚öæRÂ6ò¢òò7VvvW7F–öâv÷VÆB6VæBF†RW6W"Fò6V6öæBW'&÷"à¢f÷"‡v†BÂF‚’–â°¢‚&F‚öæR6VvÖVçBFöòÆöær"Â&G'bç6VæBç‚"’À¢‚&âVæFV6Æ&VB&V6V—fW""Â&æ÷7V6‚ç6VæB"’À¢‚&&VÂ&V6V—fW"v—F‚æò7V6‚ÖWF†öB"Â&G'bçÆ–â"’À¢‚&EUB÷'BF‚"Â&GWBç'7Bç‚"’À¢Ò°¢ÆWBW'"Ò7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gv—F‚‡F‚’’¢æW‡V7EöW'"‚ff÷&ÖB‚'·v†GÓ¢c&VgW6W2·F‡Ö"’“°¢76W'B€¢f÷&ÖB‚'¶W''Ò"’æ6öçF–ç2‚&×W7B&W6öÇfRFò†öö¶&ÆV"’À¢'·v†GÓ¢¶W''Ò ¢“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚gv—F‚‡F‚’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥&V¦V7G2À¢“°¢76W'B†×6ræ6öçF–ç2‚&æÖW2æò†öö¶&ÆV"’Â'·v†GÓ¢¶×6wÒ"“°¢Ð§Ð ¢òòòF†RF‡&VRöæÖ†æFÆW"&×2öâF†RGvò&÷VæB×FòG&ç67F÷"F‡0¢òòò†Æ÷vW%ö&÷VæE÷F&vWE÷G&ç67F÷&ÂæBF†RÇv—2ÖöâæBv†Và¢òòò7F—fVÆö÷2öbÆ÷vW%ö&÷VæEö–æ—F–F÷%÷G&ç67F÷&’ÆÂ6–@¢òòò&WfVçBÖG&—fVâG&ç67F÷'2v—BF†RWfVçB6Æ–6R"à¢òòð¢òòòæò&öw&ÒF†B&V6†W2F†VÒ6öçF–ç2âWfVçBÖG&—fVâ†æFÆW"âF†P¢òòòvFR—26ö×öæVçG3£§G&ç67F÷%ö—5ö6ö×öæVçFÂv†–6‚f÷"&÷VæB×Fð¢òòòG&ç67F÷"&WGW&ç2†5ööåö†æFÆW&(	BfÆr6WB'’äôâ×W&–öF–0¢òòò†æFÆW'2ÆöæRâ6òWfW'’WfVçB7V'67&–&W"Â'W2ãÆ6ƒâæ†æG6†¶V ¢òòòÖöæ—F÷"æB7–6ÆR×G&–vvW"&÷WFW2FòF†R6ö×÷6—FRF&ÆRÂæBöâÄãà¢òòò7–6ÆW6—2F†R6öÆR6†RF†BfÆÇ2F‡&÷Vv‚FòF†W6R&×2à¢òòð¢òòòF†—2–ç2&÷F‚†ÇfW3¢F†RF‡&VR÷6—F–öç2F†BDò'&—fRÂæB¢òòòæöâ×W&–öF–26†RW"÷6—F–öâF†B&÷f&Ç’FöW2æ÷Bà¢òòð¢òòòF†RfW&F–7B—26–ÆVçFÇ”Ö—4Æ÷vW'6ÂæB—BFöö²F‡&VRG&–W2FòvW@¢òòòF†W&R(	BVç7W÷'FVFg&öÒF†RöÆB6öFRÂF†VâVÖ—G5Væ6ö×–Æ&ÆV ¢òòòöæ6RW&–öBæÖ–ærÆFRÆWFGW&æVB÷WBæ÷BFò6ö×–ÆRÂF†Và¢òòòF†—2öæ6Rf–ÆR×66÷R6öç7FöbF†R6ÖRæÖRGW&æVB÷WBFòÖ¶P¢òòò—B6ö×–ÆRæB'VâBF†Rw&öær&FRâF—fW&vVæ6Rs†2F†R6—€¢òòòÖV7W&VB&÷w2âv†BF†RFW7B–ç2—2F†RGvòF†B&÷VæBF†P¢òòòfW&F–7C¢F†RÆ—FW&ÂW&–öBÂv†–6‚v÷&·2ÂæBF†R6†F÷vVBöæRÀ¢òòòv†–6‚6WG2F†R7FGW2à¢5·FW7EÐ¦fâö&÷VæE÷Fõ÷G&ç67F÷%ööåö&Õö—5÷&V6†&ÆUööæÇ•öf÷%ö÷W&–öF–5ö†æFÆW"‚’°¢ÆWB&6RÒf—‡GW&R‚'FÆÕ÷F&vWE÷F‡&VEö–e÷FW7Bæ†&2"“°¢6öç7BD…$TC¢g7G"Ò"F‡&VB'W2ç&VB†FG#¢V–çCÃƒâ’#°¢76W'B†&6Ræ6öçF–ç2…D…$TB’Â&f—‡GW&R6†R6†ævVB"“°¢ÆWB†öö¶&ÆRÒ"†öö¶&ÆR–ær‡c¢V–çCÃƒâ•Æâ&Wö62Ò&Wö62²eÆâVæB–æuÆâ#°¢ÆWBW&–öF–2Ò"öâR7–6ÆW5Æâ&Wö62Ò&Wö62²ÆâVæBöåÆâ#° ¢òòF†R–æ—F–F÷"f÷&ÒæVVG2F†RF&vWBF‡&VBtôäS¢G&ç67F÷ ¢òò6''––ær&÷F‚—26Vv‡B'’F†RÖ—†–ær6†V6²†VBöbF†W6R&×2à¢ÆWBæõ÷F‡&VBÒ°¢ÆWB’Ò&6Ræf–æB…D…$TB’æW‡V7B‚'F‡&VB&W6VçB"“°¢ÆWB¢Ò&6Ræf–æB‚"VæBF‡&VB"’æW‡V7B‚'F‡&VBVæG2"’²"VæBF‡&VEÆâ"æÆVâ‚“°¢f÷&ÖB‚'·×·Ò"Âf&6U²âæ•ÒÂf&6U¶¢âåÒ¢Ó°¢ÆWB–æ—F–F÷"ÒÆ—FV×3¢g7G'Â°¢æõ÷F‡&VBç&WÆ6Vâ€¢"&Wö62¢V–çCÃ3#âFVfVÇBÆâ"À¢ff÷&ÖB‚"&Wö62¢V–çCÃ3#âFVfVÇBÆåÆç¶†öö¶&ÆWÕÆç¶—FV×7Ò"’À¢À¢¢Ó° ¢òòF†Rf—‡GW&R&–æG2F†R–ç7Fæ6R76—fVÂv†–6‚—2v†B¢òòv†Vâ7F—fV†æFÆW"—266÷VBõUBöbâF†B&÷ræVVG2à¢òò7F—fV–ç7Fæ6Rf÷"cw2VÖ—76–öâFò&Rö'6W'f&ÆRBÆÂà¢6öç7B54•dS¢g7G"Ò&ÆWBF&vWB¢FÆÔÖVÕF&vWB76—fRÒ&–æBÖVÒ#°¢76W'B†&6Ræ6öçF–ç2…54•dR’Â&f—‡GW&R&–æF–ær6†ævVB"“°¢ÆWB7F—fRÒÇ7&3¢7G&–æwÂ7&2ç&WÆ6Vâ…54•dRÂe54•dRç&WÆ6R‚'76—fR"Â&7F—fR"’Â“° ¢òòWfW'’÷6—F–öâF†B&V6†W2öæRöbF†RF‡&VR&×2à¢f÷"‡v†BÂ7&2’–â°¢òòF&vWB×6–FS¢æò†öö¶&ÆVÂ6òF†RF‡&VB7F—2à¢€¢'F&vWB"À¢&6Rç&WÆ6Vâ…D…$TBÂff÷&ÖB‚'·W&–öF–7ÕÆçµD…$TGÒ"’Â’À¢’À¢òò–æ—F–F÷"×6–FRÂÇv—2Ööâ—FV×2â7F—fV†W&RFöó¢F†P¢òò–æ—F–F÷"$dÒf÷&Ò&VgW6W276—fV&–æF–ær÷WG&–v‡BÂ6ð¢òò76—fR6÷W&6RæWfW"&V6†W2F†R&ÒBÆÂà¢‚&–æ—F–F÷"—FV×2"Â7F—fR†–æ—F–F÷"‡W&–öF–2’’’À¢òò–æ—F–F÷"×6–FRÂ–ç6–FRv†Vâ7F—fVà¢€¢&–æ—F–F÷"v†Vâ7F—fR"À¢7F—fR†–æ—F–F÷"‚ff÷&ÖB€¢"v†Vâ7F—fUÆç·W&–öF–7ÒVæBv†VåÆâ ¢’’’À¢’À¢Ò°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B€¢×6ræ6öçF–ç2‚'W&–öF–2öâÄãâ7–6ÆW6†æFÆW'2"’À¢'·v†GÓ¢¶×6wÒ ¢“° ¢òòF†R7FGW26öÖW2g&öÒF†RW&–öBU…$U54”ôâÂæ÷BF†P¢òò†æFÆW"Â6òF†RÆ—FW&Â66R×W7B7F–ÆÂ&R6†÷vâFòv÷&²(	@¢òò÷F†W'v—6R—B—2VæV&æVB–âF†R÷F†W"F—&V7F–öââc¢òò&Vv—7FW'27–6ÆR×7F×VB6Æ÷7W&RF†Bf—&W2F†R&öG’WfW'¢òòâ7–6ÆW2v–ç7BF†R–ç7Fæ6Rw27FFR7G'V7Bà¢ÆWBcÐ¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·v†GÓ¢cVÖ—G3¢¶WÒ"’“°¢76W'B€¢cæ6öçF–ç2‚%÷W&–öBÒ†–çCcE÷B’ƒR“²"’bbcæ6öçF–ç2‚'&Wö62²²"’À¢'·v†GÓ¢c×W7BVÖ—BF†RW&–öF–2&öG’ ¢“° ¢òòW&–öBæÖ–ærâ–×Â×66÷RÆWFâcVÖ—G2—B–çFòF†P¢òò6ÖR6Æ÷7W&RÂv†–6‚—2&Vv—7FW&VB$Tdõ$RF†BÆWFW†—7G2À¢òò6òF†RVÖ—GFVB2²²FöW2æ÷B6ö×–ÆRà¢ÆWBæÖVBÒ7&2ç&WÆ6Vâ‚&öâR7–6ÆW2"Â&öâÆ–Ö—B7–6ÆW2"Â’ç&WÆ6Vâ€¢"'VåÆâ"À¢"ÆWBÆ–Ö—BÒUÆåÆâ'VåÆâ"À¢À¢“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fæÖVB’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·v†GÓ¢cVÖ—G2F†RæÖVBW&–öC¢¶WÒ"’“°¢ÆWBW6VBÒc¢æf–æB‚%÷W&–öBÒ†–çCcE÷B’†Æ–Ö—B“²"¢çVçw&ö÷%öVÇ6R‡ÇÂæ–2‚'·v†GÓ¢c×W7BVÖ—BF†RæÖVBW&–öB"’“°¢ÆWBFV6Æ&VBÒc¢æf–æB‚&–çCcE÷BÆ–Ö—BÒS²"¢çVçw&ö÷%öVÇ6R‡ÇÂæ–2‚'·v†GÓ¢c×W7BVÖ—BF†RÆWF"’“°¢76W'B€¢FV6Æ&VBâW6VBÀ¢'·v†GÓ¢F†RÆWF×W7B&RVÖ—GFVBeDU"F†RW6R(	BF†—2—2'—FRÖöfg6WBÀ¢&÷‡’f÷"vFöW2æ÷B6ö×–ÆRrÂæB—B—2öæÇ’F†C¢F†R&÷r&VÆ÷r—2F†RÀ¢6ÖR÷&FW&–ærv—F‚6öç7FFFVBÂæB—B6ö×–ÆW2 ¢“° ¢òòF†RÖ—'&÷"Ö–ÖvR&÷rÂæBF†R&V6öâF†RFWF–ÂæÖW2¢òòõ4•D”ôâ&F†W"F†â6FVv÷'“¢Ö÷fRF†R6ÖRÆWF&÷fP¢òòF†RG&ç67F÷"w2÷vâ&–æF–æræBcVÖ—G2—B$Tdõ$RF†P¢òò&Vv—7G&F–öâÂ6òF†R÷WGWB6ö×–ÆW2æBF†R†æFÆW"'Vç2@¢òòF†R&–v‡B&FRâÒÖ6öFVvVâc—2vVçV–æRW66R†F6‚f÷ ¢òòF†B&öw&ÒÂæBFWF–Â6Æ–Ö–ær'F†RFW7Bw2÷vâÆWF ¢òò&–æF–æw2"v÷VÆB&RÇ––ær&÷WB—Bà¢òð¢òò–ç6W'FVB&WGvVVâF†R%U2&–æF–æræBF†RE$å45Dõ"&–æF–ærÀ¢òòæ÷B&÷fR&÷F‚(	BF†RFWF–ÂæÖW2F†RG&ç67F÷"w2&–æF–ærÀ¢òò6òF†B—2F†R&÷VæF'’F†R&÷r†2Fò7G&FFÆRâf—'7@¢òòfW'6–öâWBF†RÆWF&÷fRWfW'—F†–ærÂv†–6‚FVÖöç7G&FW0¢òò6öÖWF†–ærvV¶W"à¢òòæ6†÷&VBöâF†RG&ç67F÷"&–æF–ærv†FWfW"—G2ÖöFR(	BGvð¢òòöbF†RF‡&VR&÷w2&Ww&—FR76—fVFò7F—fVà¢ÆWB&–æF–ærÒ7&0¢æÆ–æW2‚¢æf–æB‡ÆÇÂÂæ6öçF–ç2‚&ÆWBF&vWB¢FÆÔÖVÕF&vWB"’bbÂæ6öçF–ç2‚&&–æBÖVÒ"’¢çVçw&ö÷%öVÇ6R‡ÇÂæ–2‚'·v†GÓ¢F†RG&ç67F÷"&–æF–ær×W7B&R&W6VçB"’¢çFõ÷7G&–ær‚“°¢ÆWBV&Ç’Ò7&2ç&WÆ6Vâ‚&öâR7–6ÆW2"Â&öâÆ–Ö—B7–6ÆW2"Â’ç&WÆ6Vâ€¢f&–æF–ærÀ¢ff÷&ÖB‚"ÆWBÆ–Ö—BÒUÆç¶&–æF–æwÒ"’À¢À¢“°¢76W'B€¢V&Ç’æf–æB‚&ÆWBÆ–Ö—B"’æW‡V7B‚&–ç6W'FVB"¢âV&Ç’æf–æB‚&&–æBGWB"’æW‡V7B‚'F†R'W2&–æF–ær—2&÷fR—B"’À¢'F†RÆWF×W7B6—B$TÄõrF†R'W2&–æF–ærÂ÷"F†R&÷rFW7G2F†Rw&öær&÷VæF'’ ¢“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fV&Ç’’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·v†GÓ¢cVÖ—G2F†RV&Ç’ÆWF¢¶WÒ"’“°¢ÆWBW6VBÒc¢æf–æB‚%÷W&–öBÒ†–çCcE÷B’†Æ–Ö—B“²"¢çVçw&ö÷%öVÇ6R‡ÇÂæ–2‚'·v†GÓ¢c×W7BVÖ—BF†RæÖVBW&–öB"’“°¢ÆWBFV6Æ&VBÒc¢æf–æB‚&–çCcE÷BÆ–Ö—BÒS²"¢çVçw&ö÷%öVÇ6R‡ÇÂæ–2‚'·v†GÓ¢c×W7BVÖ—BF†RÆWF"’“°¢76W'B€¢FV6Æ&VBÂW6VBÀ¢'·v†GÓ¢ÆWF&÷fRF†R&–æF–ær×W7B&RVÖ—GFVB$Tdõ$RF†RW6R ¢“° ¢òòæBF†R&÷rF†B4UE2F†R7FGW2Âv†–6‚—2v÷'6RF†âV—F†W ¢òòöbF†÷6S¢FBf–ÆR×66÷R6öç7FöbF†R6ÖRæÖRâæ÷rF†P¢òò6Æ÷7W&R&W6öÇfW2(	BFòF†R6öç7FW‡&BæÖW76R66÷R(	@¢òò6òcw2÷WGWB6ö×–ÆW2æBF†R†æFÆW"'Vç2Brv†W&RF†P¢òò&öw&Ò6—2Râ'V–ÇBæB'Vâ÷WG6–FRF†R7V—FS¢Gv–6R–â#¢òò7–6ÆW2–ç7FVBöbf÷W"à¢ÆWB6†F÷vVBÒf÷&ÖB‚&6öç7BÆ–Ö—BÒuÆåÆç¶æÖVGÒ"“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g6†F÷vVB’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·v†GÓ¢cVÖ—G2F†R6†F÷vVBW&–öC¢¶WÒ"’“°¢ÆWB¶öç7BÒc¢æf–æB‚'7FF–26öç7FW‡"–çCcE÷BÆ–Ö—BÒs²"¢çVçw&ö÷%öVÇ6R‡ÇÂæ–2‚'·v†GÓ¢c×W7BVÖ—BF†R6öç7B"’“°¢ÆWBW6VBÒc¢æf–æB‚%÷W&–öBÒ†–çCcE÷B’†Æ–Ö—B“²"¢çVçw&ö÷%öVÇ6R‡ÇÂæ–2‚'·v†GÓ¢c×W7BVÖ—BF†RæÖVBW&–öB"’“°¢ÆWBÇBÒc¢æf–æB‚&–çCcE÷BÆ–Ö—BÒS²"¢çVçw&ö÷%öVÇ6R‡ÇÂæ–2‚'·v†GÓ¢c×W7BVÖ—BF†RÆWF"’“°¢76W'B€¢¶öç7BÂW6VBbbW6VBÂÇBÀ¢'·v†GÓ¢F†R6öç7B×W7B&V6VFRF†RW6RæBF†RÆWFföÆÆ÷r—BÂÀ¢÷"F†RfÇVRF†R†æFÆW"–6·2W—2æ÷BF†Rw&öæröæR ¢“°¢Ð ¢òòF†Rv†Vâ7F—fV&÷w2æVVFVBâ7F—fV–ç7Fæ6RÂæBF†—2—0¢òòv‡“¢&÷VæB76—fVÂc44õU2D„R„äDÄU"õUB(	B—G2÷WGWB—0¢òò'—FRÖ–FVçF–6ÂFòF†R6ÖR&öw&Òv—F‚F†R†æFÆW"FVÆWFVBà¢òòF†B—2cö&W––ærv†Vâ7F—fVÂæ÷BG&÷–ærF†R6öç7G'V7Bà¢ÆWBv†Våö7F—fRÒ–æ—F–F÷"‚ff÷&ÖB‚"v†Vâ7F—fUÆç·W&–öF–7ÒVæBv†VåÆâ"’“°¢76W'B€¢v†Våö7F—fRæ6öçF–ç2…54•dR’À¢'F†R76—fR&–æF–ær7W'f—fW2 ¢“°¢76W'EöW€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gv†Våö7F—fR’’æW‡V7B‚'cVÖ—G2"’À¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f–æ—F–F÷"‚""’’’æW‡V7B‚'cVÖ—G2"’À¢'c66÷W2v†Vâ7F—fVW&–öF–2†æFÆW"÷WBöb76—fR–ç7Fæ6R ¢“° ¢òòF†RæVvF—fR†Æc¢äôâ×W&–öF–2öææWfW"'&—fW2Bç’ö`¢òòF†RF‡&VR&×2âV6‚&÷r&VÆ÷r—2F†R6ÖR6÷W&6R2öæRöbF†P¢òò÷6—F—fR&÷w2&÷fRv—F‚öæÇ’F†RE$”ttU"6†ævVBÂ6òv†BÖ÷fW0¢òò—B—2F†RvFRæBæ÷F†–ærVÇ6Rà¢ÆWBæöå÷W&–öF–2Ò"öâ&VEö6÷VçBâÆâ&Wö62Ò&Wö62²ÆâVæBöåÆâ#°¢òòÆ÷vW'66—2v†–6‚'&æ6‚F†R&÷r—2W‡V7FVBFòF¶Râv—F†÷W@¢òò—B&÷r6÷VÆB6–ÆVçFÇ’7v—F6‚'&æ6†W2(	B–bF†RF&vWB&÷rw0¢òòF‡&VF×F‡&÷Vv‚×F†RÖ6ö×öæVçB×F‚&ÒvW&RÆFW"–×ÆVÖVçFVB—@¢òòv÷VÆB7F'BÆ÷vW&–ærÂF¶RF†Rö¶'&æ6‚ÂæBV–WFÇ’7F÷ ¢òò76W'F–ærç—F†–ær&÷WBv†W&R—BvVçBà¢f÷"‡v†BÂÆ÷vW'2Â7&2’–â°¢€¢'F&vWB"À¢fÇ6RÀ¢&6Rç&WÆ6Vâ…D…$TBÂff÷&ÖB‚'¶æöå÷W&–öF–7ÕÆçµD…$TGÒ"’Â’À¢’À¢‚&–æ—F–F÷"—FV×2"ÂG'VRÂ7F—fR†–æ—F–F÷"†æöå÷W&–öF–2’’’À¢€¢&–æ—F–F÷"v†Vâ7F—fR"À¢G'VRÀ¢7F—fR†–æ—F–F÷"‚ff÷&ÖB€¢"v†Vâ7F—fUÆç¶æöå÷W&–öF–7ÒVæBv†VåÆâ ¢’’’À¢’À¢Ò°¢ÆWBv÷BÒÆ÷vW%÷7&2‚g7&2“°¢76W'EöW€¢v÷Bæ—5öö²‚’À¢Æ÷vW'2À¢'·v†GÓ¢W‡V7FVBÆ÷vW'3×¶Æ÷vW'7ÒÂv÷BF†R÷F†W"'&æ6‚ ¢“°¢ÖF6‚v÷B°¢òòF†RGvò–æ—F–F÷"&÷w2vògW'F†W"F†â&æ÷B†W&R#¢F†P¢òò6ö×÷6—FRF&ÆRÄõtU%2F†VÒÂæBF†RG&–vvW"ÆæG22¢òò7–6ÆR†æFÆW"öâF†R6ö×öæVçBâF†B—2F†R÷6—F—fP¢òòv—FæW72f÷"v†W&RF†W’vVçBà¢ö²‡&ör’Óâ°¢ÆWB2Ò&öp¢æ6ö×öæVçG0¢æ—FW"‚¢æf–æB‡Æ7Â2ææÖRÓÒ%FÆÔÖVÕF&vWB"¢çVçw&ö÷%öVÇ6R‡ÇÂæ–2‚'·v†GÓ¢Æ÷vW&VBÂ'WBæ÷B26ö×öæVçB"’“°¢76W'B€¢2æ7–6ÆUö†æFÆW'2æ—5öV×G’‚’À¢'·v†GÓ¢F†R6ö×÷6—FRF&ÆR×W7B6''’F†RG&–vvW" ¢“°¢Ð¢òòF†RF&vWB&÷r7F–ÆÂ†2—G2F‡&VFÂv†–6‚F†P¢òò6ö×÷6—FRF‚FöW2æ÷BÆ÷vW"(	B'WB—Bf–Ç2D„U$RÂæ@¢òò6—26òà¢W'"†R’Óâ°¢ÆWB×6rÒf÷&ÖB‚'¶WÒ"“°¢76W'B€¢×6ræ6öçF–ç2‚'&V6†VBF‡&÷Vv‚F†R6ö×öæVçBF‚"’À¢'·v†GÓ¢æöâ×W&–öF–2öæ×W7B&÷WFRFòF†R6ö×÷6—FRF&ÆS¢¶×6wÒ ¢“°¢76W'B‚×6ræ6öçF–ç2‚'W&–öF–2öâÄãâ7–6ÆW6"’Â'·v†GÓ¢¶×6wÒ"“°¢Ð¢Ð¢Ð§Ð ¢òòò6†–âöbD•5D”ä5B&VÆF–öç2ÂV6‚6ÆÆ–ærF†R&Wf–÷W2öæRGv–6Rà¢òòòæ÷F†–ær—27–6Æ–2Â6òæV—F†W"W‡æFW"w2&VÆF–öâÖæÖR7F6²6VW0¢òòòç—F†–ærw&öær(	BæBF†RW‡ç6–öâ7F–ÆÂF÷V&ÆW2BWfW'’ÆWfVÂà¢òòð¢òòòcw2W‡æFW"v2v—fVâæöFR'VFvWB–ââV&Æ–W"&F6‚æB†VÆBà¢òòò6öç7G&–çG3£§G—VEöÆ÷vW&w2v2æ÷BÂæB—B'Vç2öâUdU%’†&0¢òòò6–Ö&Vv&FÆW72öbÒÖ6öFVvVæÂ6òF†R'VFvWB&÷VæFVBæ÷F†–æs¢¢òòòbÖÆ–æ²6†–âFöö²w2Ââ‚ÖÆ–æ²6†–â3G2ÂæB#ÖÆ–æ²6†–âF–@¢òòòæ÷Bf–æ—6‚â&÷VæF–æröæRöbGvòW‡æFW'2÷fW"F†R6ÖR&VÆF–öç2—0¢òòòæ÷B&÷VæF–ærF†R&VÆF–öâà¢òòð¢òòò&÷F‚Æ–Ö—G2æ÷rÆ—fR–â7Bç'6æB&÷F‚W‡æFW'26†&vRF†VÒ÷W@¢òòòöbF†R6ÖR6öç7FçBâF†Rö–çBöb6†&–ær—B—2F†R&÷r&VÆ÷rv†W&P¢òòòF†RGvò&6¶VæG2w&VS¢F†W’&VgW6RF†R6ÖR6†–âBF†R6ÖP¢òòòÆVæwF‚Â6ò–çfÆ–F(	B&æV—F†W"&6¶VæB'Vç2F†—2"(	B—2ÖV7W&V@¢òòò6Æ–Ò&F†W"F†ââ77V×F–öâ&÷WBcà¢òòð¢òòòF†—2FW7Bf–æ—6†–ærBÆÂ—2'BöbF†R76W'F–öââF†R#BÖÆ–æ°¢òòò66RF–Bæ÷BFW&Ö–æFR&Vf÷&RF†RwV&BW†—7FVBà¢5·FW7EÐ¦fâöF÷V&Æ–æu÷&VÆF–öåö6†–åö—5÷&VgW6VEöE÷F†U÷6ÖU÷ö–çEö'•ö&÷F…ö&6¶VæG2‚’°¢ÆWB7&2ÒÆÆ–æ·3¢W6—¦WÂ°¢ÆWB×WB&VÇ2Ò7G&–æs£¦æWr‚“°¢f÷"²–ââæÆ–æ·2°¢&VÇ2³Òff÷&ÖB‚'&VÆF–öâ'¶·Ò‡#¢&W’Ò'·Ò‡"’bb'·Ò‡"•Æâ"Â²²Â²²“°¢Ð¢&VÇ2³Òff÷&ÖB‚'&VÆF–öâ'¶Æ–æ·7Ò‡#¢&W’Ò"çfÇVRâÆâ"“°¢f÷&ÖB€¢&FöÖ–âEÆâg&WöÖ‡£¢ÆæVæBFöÖ–âEÆåÆåÀ¢G&ç67F–öâ&WÆâFG"¢V–çCÃƒåÆâfÇVR¢V–çCÃ3#åÆåÀ¢VæBG&ç67F–öâ&WÆåÆç·&VÇ7ÕÆåÀ¢FW7BEÆâÆWBGWB¢F÷ÆâÆWBB¢&WÆâ6Æö6²6Æ²ÒEÆâ'VåÆåÀ¢Çƒ#&æFöÖ—¦R‡B’v—F…Æâ#‡B•ÆâVæB&æFöÖ—¦UÆåÀ¢Çƒ#VæB'VåÆæVæBFW7BEÆâ ¢¢Ó° ¢òòF†RwV&B—2Æ–Ö—BÂæ÷B&ã¢’ÖÆ–æ²6†–â—2S"ÆVfW2æ@¢òò&÷F‚&6¶VæG27F–ÆÂW‡æB—Bà¢ÆWBö²Ò7&2ƒ’“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fö²’’æW‡V7B‚'cW‡æG2’ÖÆ–æ²6†–â"“°¢Æ÷vW%÷7&2‚fö²’æW‡V7B‚%D"Ô•"W‡æG2’ÖÆ–æ²6†–â"“° ¢òòöæRÆ–æ²gW'F†W"ÂæB$õD‚&VgW6R(	BF†R6ÖR&÷VæF'’Â&V6W6P¢òòF†R'VFvWB—2öæR6öç7FçBà¢f÷"Æ–æ·2–â³ÂbÂ#EÒ°¢ÆWBFöõö&–rÒ7&2†Æ–æ·2“° ¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gFöõö&–r’’æW‡V7EöW'"‚'c&VgW6W2"“°¢76W'B€¢f÷&ÖB‚'·cÒ"’æ6öçF–ç2‚&6öç7G&–çBgVæ7F–öâ6ÆÂæ÷B7W÷'FVB"’À¢'¶Æ–æ·7ÒÆ–æ·3¢·cÒ ¢“° ¢òò–çfÆ–FÂæ÷BVç7W÷'FVF¢c—2æ÷Bv’Fò'Vâ—Bà¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚gFöõö&–r’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚&W†6VVG2F†R&VÆF–öâÖW‡ç6–öâÆ–Ö—B"’À¢'¶Æ–æ·7ÒÆ–æ·3¢¶×6wÒ ¢“° ¢òòF†Rt„ôÄRÖW76vRÂæ÷B&Vf—‚öb—BâF†Rf—'7BfW'6–öâö`¢òòF†—2F–væ÷7F–2&V6†VBW6W'2v—F‚Gvò'Vç2öb#"76W2–à¢òò—B(	Bf÷&ÖB&öG’v†÷6RÆ–æR6öçF–çVF–öç2†B&VVà¢òòfÆGFVæVB(	BæBWfW'’76W'F–öâ&÷fRÖF6†W2FW‡BF†@¢òòÆæG2&Vf÷&RF†Rf—'7BvÂ6òæöæRöbF†VÒ6÷VÆB6VR—Bà¢76W'B€¢×6ræ6öçF–ç2‚""’À¢'¶Æ–æ·7ÒÆ–æ·3¢F†RF–væ÷7F–2×W7Bæ÷B6''’'VâÖöâv†—FW76S¢¶×6s£÷Ò ¢“°¢76W'B€¢×6ræVæG5÷v—F‚‚&Ö÷&RF†âöæ6RF÷V&ÆW2BWfW'’ÆWfVÂ"’À¢'¶Æ–æ·7ÒÆ–æ·3¢¶×6s£÷Ò ¢“°¢Ð ¢òòF†R÷F†W"6†&VBÆ–Ö—BÂöâF†R6ÖRfö÷F–æs¢6†–âöb4”ätÄP¢òò6ÆÇ2æWfW"F÷V&ÆW2Â6òF†R'VFvWBæWfW"f—&W2æBF†RFWF€¢òò&6·7F÷—2v†Bç7vW'2â—Bç7vW'2BF†R6ÖRÆ–æ²–â&÷F€¢òò&6¶VæG2(	Bc2W‡æG2ÂcBFöW2æ÷B(	Bv†–6‚—2F†Rv†öÆRö–çBö`¢òòF†R6öç7FçG2&V–æröæR6öç7FçBà¢ÆWBÆ–æV"ÒÆÆ–æ·3¢W6—¦WÂ°¢ÆWB×WB&VÇ2Ò7G&–æs£¦æWr‚“°¢f÷"²–ââæÆ–æ·2°¢&VÇ2³Òff÷&ÖB‚'&VÆF–öâ'¶·Ò‡#¢&W’Ò'·Ò‡"•Æâ"Â²²“°¢Ð¢&VÇ2³Òff÷&ÖB‚'&VÆF–öâ'¶Æ–æ·7Ò‡#¢&W’Ò"çfÇVRâÆâ"“°¢f÷&ÖB€¢&FöÖ–âEÆâg&WöÖ‡£¢ÆæVæBFöÖ–âEÆåÆåÀ¢G&ç67F–öâ&WÆâFG"¢V–çCÃƒåÆâfÇVR¢V–çCÃ3#åÆåÀ¢VæBG&ç67F–öâ&WÆåÆç·&VÇ7ÕÆåÀ¢FW7BEÆâÆWBGWB¢F÷ÆâÆWBB¢&WÆâ6Æö6²6Æ²ÒEÆâ'VåÆåÀ¢Çƒ#&æFöÖ—¦R‡B’v—F…Æâ#‡B•ÆâVæB&æFöÖ—¦UÆåÀ¢Çƒ#VæB'VåÆæVæBFW7BEÆâ ¢¢Ó°¢ÆWBFVWÒÆ–æV"ƒc2“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fFVW’’æW‡V7B‚'cW‡æG2c2ÖÆ–æ²6†–â"“°¢Æ÷vW%÷7&2‚fFVW’æW‡V7B‚%D"Ô•"W‡æG2c2ÖÆ–æ²6†–â"“° ¢ÆWBFöõöFVWÒÆ–æV"ƒcB“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gFöõöFVW’’æW‡V7EöW'"‚'c&VgW6W2cBÖÆ–æ²6†–â"“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚gFöõöFVW’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚&W†6VVG2F†R&VÆF–öâÖW‡ç6–öâÆ–Ö—B"’À¢'¶×6wÒ ¢“°§Ð ¢òòòVÖ—BÆWcâ‚âââ–†2F‡&VRÆ÷vW&–ær'&æ6†W2ÂæBöæÇ’ôäRöbF†VÐ¢òòò6†V6¶VBF†BâWfVçB–ÆöB—2W†7FÇ’öæR&wVÖVçC¢F†P¢òòòFW7B×66÷RÆWBR¢WfVçCÅCæÆö6ÂâF†RF÷GFVB×F‚f÷&Ð¢òòò†VÖ—BFvvW"æ–åöWb‡b–’æBF†R6VÆb×&VÆF—fRf÷&Ð¢òòò†VÖ—Bö'6W'fVB‡b––ç6–FRÖWF†öB&öG’’&÷F‚Föö²v†FWfW"F†W¢òòòvW&Rv—fVâà¢òòð¢òòòÖV7W&VBöâ&÷F‚&6¶VæG2B&÷F‚6—FW2Âv†–6‚—2v†BÖ¶W2F†—0¢òòò–çfÆ–F&F†W"F†âv ¢òòð¢òòòÂ&—G’ÂF&—"ÂcÀ¢òòòÂÒÒ×ÂÒÒ×ÂÒÒ×À¢òòòÂ÷fW"Â÷2‡bÂ"–(	BVæ6ö×–Æ&ÆRÂ÷2‡b–(	B6–ÆVçFÇ’G&÷2—BÀ¢òòòÂVæFW"Â÷2‚–(	BVæ6ö×–Æ&ÆRÂ÷2‚–(	BVæ6ö×–Æ&ÆRÀ¢òòð¢òòòr²²öâF†R÷fW"×7WÇ’f÷&Ó¢&æòÖF6‚f÷"6ÆÂFð¢òòòr‡7FC£¦gVæ7F–öãÇfö–B†ÆöærVç6–væVB–çB“â’†–çBÂ–çB’r"â6òæð¢òòò&6¶VæB'Vç2F†R&öw&Ò2w&—GFVâVæFW"ç’öbF†Rf÷W"6VÆÇ2À¢òòòæBF†RfW&F–7B—2F†RöæRF†RÆö6ÂÖWfVçB'&æ6‚Ç&VG’vfRà¢5·FW7EÐ¦fâåöWfVçEöVÖ—E÷F¶W5öW†7FÇ•ööæU÷–ÆöEöEöWfW'•ö'&æ6‚‚’°¢f÷"‡v†BÂ&6RÂ6ÆÂ’–â°¢€¢&F÷GFVBF‚"À¢f—‡GW&R‚&vVçEööåö†æFÆW%÷FW7Bæ†&2"’À¢&VÖ—BFvvW"æ–åöWb†’²’"À¢’À¢€¢'6VÆb×&VÆF—fR"À¢f—‡GW&R‚&æÇ—6—5÷6–æµö6öææV7E÷FW7Bæ†&2"’À¢&VÖ—Bö'6W'fVB‡b’"À¢’À¢Ò°¢76W'B†&6Ræ6öçF–ç2†6ÆÂ’Â'·v†GÓ¢f—‡GW&R6†R6†ævVB"“°¢òòF†R6öçG&öÂÆ÷vW'2Â6òF†R&VgW6Ç2&VÆ÷r&R&÷WB&—G’æ@¢òòæ÷B&÷WBF†Rf—‡GW&Rà¢Æ÷vW%÷7&2‚f&6R’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·v†GÓ¢6öçG&öÂÆ÷vW'3¢¶WÒ"’“° ¢ÆWBöæRÒ6ÆÂç&f–æB‚r‚r’æW‡V7B‚&6ÆÂ†2&w2"“°¢f÷"†&—G’Â&w2’–â²‚&÷fW""Â"†’²Â"’"’Â‚'VæFW""Â"‚’"•Ò°¢ÆWB&w2Ò–bv†BÓÒ'6VÆb×&VÆF—fR"bb&—G’ÓÒ&÷fW""°¢"‡bÂ"’ ¢ÒVÇ6R°¢&w0¢Ó°¢ÆWB7&2Ò&6Rç&WÆ6Vâ†6ÆÂÂff÷&ÖB‚'·×¶&w7Ò"Âf6ÆÅ²âæöæUÒ’Â“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚&âWfVçB–ÆöB—2W†7FÇ’öæR"’À¢'·v†GÒ÷¶&—G—Ó¢¶×6wÒ ¢“° ¢òòcVÖ—G2&÷F‚Âv†–6‚—2W†7FÇ’v‡’D"Ô•"×W7Bæ÷B6Væ@¢òòç–öæRF†W&S¢F†R÷fW"×7WÇ’f÷&ÒG&÷2F†RW‡G&¢òò–ÆöB6–ÆVçFÇ’æBF†RVæFW"×7WÇ’f÷&ÒFöW2æ÷@¢òò6ö×–ÆRà¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·v†GÒ÷¶&—G—Ó¢cVÖ—G3¢¶WÒ"’“°¢–b&—G’ÓÒ'VæFW""°¢76W'B‡cæ6öçF–ç2‚"’÷2‚“²"’Â'·v†GÓ¢cVÖ—G2æòÖ&r6ÆÂ"“°¢ÒVÇ6R°¢òòF†Rõ4•D•dRf7BÂæ÷B§W7BF†R'6Væ6RöbF†R6V6öæ@¢òò–ÆöC¢cVÖ—G2F†RöæRÖ&wVÖVçB6ÆÂÂ6òF†Rv†öÆP¢òòf–ÆR—2F†R6öçG&öÂw2â6öçF–ç2‚"Â"“²"–ÆöæP¢òòv÷VÆBÇ6ò72–bcG&÷VBF†RVÖ—F7FFVÖVç@¢òòVçF—&VÇ’Â÷"&VgW6VB(	BæB'6–ÆVçFÇ’G&÷2F†RW‡G&¢òò–ÆöB"—2F†R6VÆÂF†R–çfÆ–FfW&F–7B&W7G2öâà¢76W'EöW€¢cÀ¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f&6R’’æW‡V7B‚'cVÖ—G2F†R6öçG&öÂ"’À¢'·v†GÓ¢cw2÷fW"×7WÇ’÷WGWB×W7BWVÂF†R6öçG&öÂw2 ¢“°¢Ð¢Ð¢Ð ¢òò–çfÆ–F&VgW6W2&öw&×2÷WG&–v‡BÂ6òF†Rv’FòvWBF†—2w&öæp¢òò—2ÆVvÂ7VÆÆ–ærv†÷6R&—G’—2æ÷BöæRâGvòW†—7BæB$õD€¢òò72†&26†V6¶âæV—F†W"—2âW66R†F6ƒ¢cVÖ—G2¢òòöæR×–ÆöB6†ææVÂ†÷vWfW"F†Rf–VÆB—2w&—GFVâà¢ÆWBvVçBÒf—‡GW&R‚&vVçEööåö†æFÆW%÷FW7Bæ†&2"“°¢6öç7Bd”TÄC¢g7G"Ò&–åöWb¢WfVçCÇV–çCÃƒãâ#°¢76W'B†vVçBæ6öçF–ç2„d”TÄB’Â&f—‡GW&R6†R6†ævVB"“° ¢òòæòG—R&wVÖVçBBÆÂÂVÖ—GFVBæB†æFÆVBv—F‚æò–ÆöBà¢ÆWB&&RÒvVç@¢ç&WÆ6Vâ„d”TÄBÂ&–åöWb¢WfVçB"Â¢ç&WÆ6Vâ‚&VÖ—BFvvW"æ–åöWb†’²’"Â&VÖ—BFvvW"æ–åöWb‚’"Â¢ç&WÆ6Vâ‚&öâ–åöWb‡B’"Â&öâ–åöWb‚’"Â¢ç&WÆ6Vâ‚"Æ7BÒEÆâ"Â""Â“°¢76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚f&&R’çVçw&öW'"‚’“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f&&R’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢cæ6öçF–ç2‚'7FC£§fV7F÷#Ç7FC£¦gVæ7F–öãÇfö–B‡V–çCcE÷B“ãâ–åöWc²"’bbcæ6öçF–ç2‚"’÷2‚“²"’À¢'c¶VW2öæR×–ÆöB6†ææVÂæB6ÆÇ2—Bv—F‚æöæR ¢“° ¢òòGvòG—R&wVÖVçG2ÂVÖ—GFVBv—F‚Gvò–ÆöG2à¢ÆWBGvòÒvVç@¢ç&WÆ6Vâ„d”TÄBÂ&–åöWb¢WfVçCÇV–çCÃƒâÂV–çCÃƒãâ"Â¢ç&WÆ6Vâ‚&VÖ—BFvvW"æ–åöWb†’²’"Â&VÖ—BFvvW"æ–åöWb†’²Â"’"Â“°¢76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚gGvò’çVçw&öW'"‚’“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gGvò’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢cæ6öçF–ç2‚'7FC£§fV7F÷#Ç7FC£¦gVæ7F–öãÇfö–B‡V–çCcE÷B“ãâ–åöWc²"’À¢'c¶VW2öæR×–ÆöB6†ææVÂ ¢“°¢76W'B€¢cæ6öçF–ç2‚"Â"“²"’À¢&æBG&÷2F†R6V6öæB–ÆöB&F†W"F†â6''––ær—B ¢“°§Ð ¢òòòF†R&VÖ–æ–ær6öææV7F4TÔåD”2&×2ÂæöæRöbv†–6‚†BFW7Bà¢òòòÖV7W&VBv–ç7Bcöââ–ç7FçF–FVBVçb(	BF†RöæÇ’Æ6RcÆöö·0¢òòòBF†RVFvRBÆÂà¢òòð¢òòòÂVFvRÂcÂfW&F–7BÀ¢òòòÂÒÒ×ÂÒÒ×ÂÒÒ×À¢òòòÂ6–æ²—2Æ–âgVæ7F–öæÂf÷"†WFòb÷2¢6–æ²çÆ–â–(	Bæ÷B7G'V7BÖVÖ&W"ÂVÖ—G5Væ6ö×–Æ&ÆVÀ¢òòòÂ6–æ²—266Æ"f–VÆBÂf÷"†WFòb÷2¢6–æ²æ÷F†W"–÷fW"V–çCcE÷FÂVÖ—G5Væ6ö×–Æ&ÆVÀ¢òòòÂ–ÆöBÖ—6ÖF6‚Â$T4õ$Bg266Æ"Â'&–FvRÆÖ&F–ç7FçF–FW2v–ç7BF†Rw&öærG—RÂVÖ—G5Væ6ö×–Æ&ÆVÀ¢òòòÂ–ÆöBÖ—6ÖF6‚Â4”täTDäU52öæÇ’Â–×Æ–6—B6öçfW'6–öâ(	B6ö×–ÆW2æB'Vç26÷'&V7FÇ’ÂVç7W÷'FVFÀ¢òòð¢òòòF†RVæ6ö×–Æ&ÆR&÷w2&R6ö×–ÆW"ÖÖV7W&VBÂæ÷B&VBöfbF†RFW‡Bà¢òòð¢òòòF†R–ÆöB&÷w2&RöæR&Ò6÷fW&–ærGvò6†W2Â&V6W6P¢òòòWfVçE÷–ÆöEöÖF6†W5ö—%÷G—V6ö×&W26–væVFæW72æB&V6÷&@¢òòò–FVçF—G’öæÇ’âcw2'&–FvRÆÖ&F—2tTäU$”2æBÆöö·0¢òòòG—RÖvæ÷7F–3²6öçfW'F–ær—BFòF†R6÷W&6Rw0¢òòò7FC£¦gVæ7F–öãÇfö–B‡V–çCcE÷B“æ–ç7FçF–FW2—BBF†Rv—&–ærÆ–æRÀ¢òòòv†–6‚&V6÷&B6ææ÷B7W'f—fRæB6–vâF–ffW&Væ6R76W2F‡&÷Vv€¢òòò2â÷&F–æ'’–×Æ–6—B6öçfW'6–öââf—'7B72ÖV7W&VBöæÇ’F†P¢òòò&V6÷&B6†RæB6ÆÆVBF†Rv†öÆR&Ò–çfÆ–F(	Bw&öærGv–6R÷fW"À¢òòò6–æ6Rc&÷F‚'Vç2F†R6–vâ66RæB'Vç2ç’öbF†W6R–ç6–FRâVç`¢òòòæ÷F†–ær–ç7FçF–FW2à¢5·FW7EÐ¦fâö6öææV7E÷6–æµ÷F†Eö6ææ÷E÷&V6V—fU÷F†U÷–ÆöEö—5÷7Æ—Eö'•÷v†E÷cöFöW2‚’°¢ÆWB7&2ÒÇ6–æµöFV6Ã¢g7G"ÂF&vWC¢g7G"ÂW‡G&¢g7G'Â°¢f÷&ÖB€¢"2'7G'V7B&V@¢Fr¢V–çCÃƒâFVfVÇB ¦VæB7G'V7B&V@ §G&ç67F÷"7&0¢ö'6W'fVB¢÷WBWfVçCÇV–çCÃƒãà¢†öö¶&ÆRV&Æ—6‚‡c¢V–çCÃƒâ¢VÖ—Bö'6W'fVB‡b¢VæBV&Æ—6€¦VæBG&ç67F÷"7&0 §66÷&V&ö&B6–æ°¢6VVâ¢V–çCÃ3#âFVfVÇB ¢÷F†W"¢V–çCÃ3#âFVfVÇB §·6–æµöFV6ÇÐ¦VæB66÷&V&ö&B6–æ° §¶W‡G&Ð¦VçbP¢7&2¢7&276—fP¢6–æ²¢6–æ°§¶W‡G&öf–VÆGÐ¢6öææV7@¢7&2æö'6W'fVBÓâ·F&vWGÐ¢VæB6öææV7@¦VæBVçbP §FW7F&Væ6‚F ¢GWB¢F÷ ¢F÷¢P¦VæBFW7F&Væ6‚F ¦–×ÂBf÷"F ¢'Và¢v—B"7–6ÆW0¢VæB'Và¦VæB–×ÂB"2À¢W‡G&öf–VÆBÒ–bW‡G&æ—5öV×G’‚’°¢" ¢ÒVÇ6R°¢"G7B¢G7B76—fUÆâ ¢Ð¢¢Ó°¢ÆWB†öö¶&ÆRÒ"†öö¶&ÆRF¶R‡c¢V–çCÃƒâ•Æâ6VVâÒ6VVâ²ÆâVæBF¶R#°¢ÆWBG7BÒÇ–ÆöC¢g7G'Â°¢f÷&ÖB‚'G&ç67F÷"G7EÆâ–æ6öÖ–ær¢WfVçCÇ·–ÆöGÓåÆæVæBG&ç67F÷"G7EÆâ"¢Ó° ¢òòF†R6öçG&öÃ¢vVÆÂÖf÷&ÖVBVFvRöbV6‚6–æ²6†RÆ÷vW'2à¢Æ÷vW%÷7&2‚g7&2††öö¶&ÆRÂ'6–æ²çF¶R"Â""’’æW‡V7B‚&ÖWF†öB6–æ²Æ÷vW'2"“°¢Æ÷vW%÷7&2‚g7&2††öö¶&ÆRÂ&G7Bæ–æ6öÖ–ær"ÂfG7B‚'V–çCÃƒâ"’’’æW‡V7B‚&WfVçB6–æ²Æ÷vW'2"“° ¢f÷"‡v†BÂ6–æµöFV6ÂÂF&vWBÂW‡G&ÂvçB’–â°¢€¢'Æ–âgVæ7F–öâ"À¢"gVæ7F–öâÆ–â‡c¢V–çCÃƒâ•Æâ6VVâÒ6VVâ²ÆâVæBÆ–â"À¢'6–æ²çÆ–â"À¢""çFõ÷7G&–ær‚’À¢&æ÷B†öö¶&ÆV"À¢’À¢€¢'66Æ"f–VÆB"À¢†öö¶&ÆRÀ¢'6–æ²æ÷F†W""À¢""çFõ÷7G&–ær‚’À¢&æV—F†W"†öö¶&ÆV6–æ²ÖWF†öBæ÷"âWfVçFf–VÆB"À¢’À¢€¢&ÖWF†öB–ÆöBÖ—6ÖF6‚Â&V6÷&B"À¢"†öö¶&ÆRF¶R‡c¢&VB•Æâ6VVâÒ6VVâ²ÆâVæBF¶R"À¢'6–æ²çF¶R"À¢""çFõ÷7G&–ær‚’À¢'–ÆöBÖ—6ÖF6‚"À¢’À¢€¢&WfVçB–ÆöBÖ—6ÖF6‚Â&V6÷&B"À¢†öö¶&ÆRÀ¢&G7Bæ–æ6öÖ–ær"À¢G7B‚$&VB"’À¢'–ÆöBÖ—6ÖF6‚"À¢’À¢Ò°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g7&2‡6–æµöFV6ÂÂF&vWBÂfW‡G&’’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¤VÖ—G5Væ6ö×–Æ&ÆRÀ¢“°¢76W'B†×6ræ6öçF–ç2‡vçB’Â'·v†GÓ¢¶×6wÒ"“°¢Ð ¢òòF†R4”täTDäU52&÷rÂv†–6‚—2F†R6ÖR&ÒæBF†R÷÷6—FP¢òòfW&F–7C¢cw2–×Æ–6—B6öçfW'6–öâ6'&–W2F†R–ÆöBF‡&÷Vv€¢òò–çF7BÂ6ò—B¶VW2F†R7VvvW7F–öâà¢f÷"‡v†BÂ6–æµöFV6ÂÂF&vWBÂW‡G&’–â°¢€¢&ÖWF†öB6–æ²"À¢"†öö¶&ÆRF¶R‡c¢6–çCÃƒâ•Æâ6VVâÒ6VVâ²ÆâVæBF¶R"À¢'6–æ²çF¶R"À¢""çFõ÷7G&–ær‚’À¢’À¢‚&WfVçB6–æ²"Â†öö¶&ÆRÂ&G7Bæ–æ6öÖ–ær"ÂG7B‚'6–çCÃƒâ"’’À¢Ò°¢ÆWB2Ò7&2‡6–æµöFV6ÂÂF&vWBÂfW‡G&“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚g2’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚'–ÆöBÖ—6ÖF6‚"’Â'·v†GÓ¢¶×6wÒ"“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g2’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·v†GÓ¢cVÖ—G26–vâÖ—6ÖF6ƒ¢¶WÒ"’“°¢Ð§Ð ¢òòòF†RdõU%D‚ÆæF–æröbF†RæöâÖÆ—FW&ÂW&–öF–2W&–öB(	B¢òòòFW7F&Væ6‚×66÷VBöâÄãâ7–6ÆW6†æFÆW"(	BgFW"F†RF‡&VR&÷VæB×Fð¢òòòG&ç67F÷"&×2â—Bv2Vç7W÷'FVFæBVçFW7FVBÂæB—B&V†fW0¢òòòW†7FÇ’Æ–¶RF†R÷F†W"F‡&VRÂv†–6‚—2F†Rö–çBöbw&÷W–ær'¢òòòv†B6öç7G'V7BDôU2&F†W"F†âv†W&R—B—27VÆÆVBà¢òòð¢òòòcVÖ—G2F†RW&–öBW‡&W76–öâfW&&F–Ò–çFòö6†V6¶W'66Æ÷7W&P¢òòò&Vv—7FW&VB†VBöbF†R–×Âw2÷vâÆWF3 ¢òòð¢òòòÂW&–öBÂcÀ¢òòòÂÒÒ×ÂÒÒ×À¢òòòÂ&(	BÆ—FW&ÂÂf–æRÂæBF†R&Vv—7FW&VBf—‡GW&R&÷fW2—BÀ¢òòòÂW&Ââ–×Â×66÷RÆWFÂW6VBBÆ–æRcÂFV6Æ&VBBsR(	BFöW2æ÷B6ö×–ÆRÀ¢òòòÂW&Âv—F‚f–ÆR×66÷R6öç7BW"ÒvFöòÂ&W6öÇfW2FòF†R6öç7C¢6ö×–ÆW2ÂæB'Vç2BrÀ¢òòð¢òòòF†RÆ7B&÷rv2'V–ÇBæB%Tã¢"f—&–æw2–â#7–6ÆW2v†W&RF†P¢òòò6÷W&6R6·2f÷"W&–öBöb"â†Væ6R6–ÆVçFÇ”Ö—4Æ÷vW'6à¢5·FW7EÐ¦fâ÷FW7F&Væ6…÷W&–öF–5ö†æFÆW%÷v—F…ööæÖVE÷W&–öEö—5öæ÷EöåöW66Uö†F6‚‚’°¢ÆWB&6RÒf—‡GW&R‚'FW7F&Væ6…÷W&–öF–5÷W&–öC%÷FW7Bæ†&2"“°¢6öç7BÄ•DU$Ã¢g7G"Ò"öâ"7–6ÆW2#°¢76W'B†&6Ræ6öçF–ç2„Ä•DU$Â’Â&f—‡GW&R6†R6†ævVB"“° ¢òòF†R6öçG&öÃ¢F†RÆ—FW&ÂW&–öBÆ÷vW'2Â6òF†R&÷w2&VÆ÷r&P¢òò&÷WBF†RU…$U54”ôâæBæ÷B&÷WBF†Rf—‡GW&Rà¢Æ÷vW%÷7&2‚f&6R’æW‡V7B‚'F†RÆ—FW&ÂW&–öBÆ÷vW'2"“° ¢ÆWBæÖVBÒ&6Rç&WÆ6Vâ„Ä•DU$ÂÂ"öâW"7–6ÆW2"Â’ç&WÆ6Vâ€¢%Æâ'VåÆâ"À¢%ÆâÆWBW"Ò%ÆåÆâ'VåÆâ"À¢À¢“°¢76W'B†æÖVBæ6öçF–ç2‚&ÆWBW"Ò""’Â'F†RÆWF×W7B&R–ç6W'FVB"“° ¢f÷"‡v†BÂ7&2’–â°¢‚&&&RÆWF"ÂæÖVBæ6ÆöæR‚’’À¢‚'6†F÷vVB'’6öç7B"Âf÷&ÖB‚&6öç7BW"ÒuÆåÆç¶æÖVGÒ"’’À¢Ò°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B€¢×6ræ6öçF–ç2‚&æöâÖÆ—FW&Â÷"æöâ×÷6—F—fRW&–öB"’À¢'·v†GÓ¢¶×6wÒ ¢“°¢Ð ¢òòF†R&÷rF†B6WG2F†R7FGW2âcVÖ—G2F†R6öç7BBæÖW76P¢òò66÷R$Tdõ$RF†R6Æ÷7W&RæBF†RÆWFgFW"—BÂ6òF†R6Æ÷7W&P¢òò&VG2rv†–ÆRF†R'Vâ&öG’&VG2"(	B6ÖRæÖRÂGvòfÇVW2À¢òòFV6–FVB'’VÖ—76–öâ÷6—F–öâà¢ÆWB6†F÷vVBÒf÷&ÖB‚&6öç7BW"ÒuÆåÆç¶æÖVGÒ"“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g6†F÷vVB’’æW‡V7B‚'cVÖ—G2"“°¢ÆWB¶öç7BÒc¢æf–æB‚'7FF–26öç7FW‡"–çCcE÷BW"Òs²"¢æW‡V7B‚'cVÖ—G2F†R6öç7BBæÖW76R66÷R"“°¢ÆWBW6VBÒc¢æf–æB‚%÷W&–öBÒ†–çCcE÷B’‡W"“²"¢æW‡V7B‚'cVÖ—G2F†RæÖVBW&–öB"“°¢ÆWBÇBÒcæf–æB‚&–çCcE÷BW"Ò#²"’æW‡V7B‚'cVÖ—G2F†RÆWF"“°¢76W'B€¢¶öç7BÂW6VBbbW6VBÂÇBÀ¢'F†R6öç7B×W7B&V6VFRF†RW6RæBF†RÆWFföÆÆ÷r—BÂ÷"F†R6Æ÷7W&RÀ¢FöW2æ÷B–6²WF†Rw&öærfÇVR ¢“° ¢òòæBv—F†÷WBF†R6öç7BÂF†R6ÖR÷&FW&–ærÆVfW2F†RæÖP¢òòVæFV6Æ&VBBF†Rö–çBöbW6Rà¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fæÖVB’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢cæ6öçF–ç2‚&6öç7FW‡"–çCcE÷BW""’À¢&æò6öç7B–âF†—2öæR ¢“°¢ÆWBW6VBÒc¢æf–æB‚%÷W&–öBÒ†–çCcE÷B’‡W"“²"¢æW‡V7B‚'cVÖ—G2F†RæÖVBW&–öB"“°¢ÆWBÇBÒcæf–æB‚&–çCcE÷BW"Ò#²"’æW‡V7B‚'cVÖ—G2F†RÆWF"“°¢76W'B‡W6VBÂÇBÂ'F†RÆWF—2VÖ—GFVBgFW"F†RW6R"“°§Ð ¢òòòGvò&÷VæB×Fò–ç7Fæ6R&×2V6‚ç7vW&VB$õD‚v—2öbvWGF–ærF†P¢òòòÖöFRææ÷FF–öâw&öærv—F‚öæRVç7W÷'FVFÂæBcç7vW'2F†VÐ¢òòòfW'’F–ffW&VçFÇ’à¢òòð¢òòòÂ–ç7Fæ6RÂcÀ¢òòòÂÒÒ×ÂÒÒ×À¢òòòÂæòææ÷FF–öâBÆÂÂ&VgW6W3¢'G&ç67F÷"–ç7FçF–F–öâ&WV—&W2ÖöFRææ÷FF–öâ"À¢òòòÂ–æ—F–F÷"$dÒFV6Æ&VB76—fVÂVÖ—G2Â'—FRÖ–FVçF–6ÂFòF†R7F—fV&öw&ÒÀ¢òòòÂF&vWB&W7öæFW"FV6Æ&VB7F—fVÂVÖ—G2Â'—FRÖ–FVçF–6ÂFòF†R76—fV&öw&ÒÀ¢òòð¢òòò6òÖ—76–ærææ÷FF–öâ—2&öw&ÒW'&÷"VæFW"&÷F‚&6¶VæG2Âæ@¢òòòF†Ru$ôärææ÷FF–öâ—2cG&÷–ær—B(	BF†RW6W"6·2f÷"76—fP¢òòò–ç7Fæ6RæBvWG2G&—fW"Â÷"6·2f÷"â7F—fR&W7öæFW"æBvWG0¢òòò76—fRöæRÂv—F‚æ÷F†–ær6–BV—F†W"v’à¢òòð¢òòòF†RçF’×f7V—G’6†V6²ÖGFW'2†W&RÂ&V6W6R&'—FRÖ–FVçF–6Â"6÷VÆ@¢òòòÖVâc†2æòæ÷F–öâöbÖöFRBÆÃ¢f÷"G&ç67F÷"F†B„2&÷F€¢òòò†ÇfW2ÂfÆ—–ærF†RÖöFR6†ævW2crÆ–æW2öbcw2÷WGWBâ—B—0¢òòò7V6–f–6ÆÇ’F†R†öö¶&ÆRÖöæÇ’æBF‡&VBÖöæÇ’6†W2v†÷6P¢òòòææ÷FF–öâcG&÷2à¢òòð¢òòòF†RGvò66W2&RFöÆB'B'’F†R5BW†7FÇ’(	BÖöFS¢æöæV ¢òòòfW'7W2ÖöFS¢6öÖR‡w&öær–(	B6òF†—2—27Æ—BöâF†R&VÀ¢òòòF—7F–æ7F–öâ&F†W"F†â6†R†WW&—7F–2à¢5·FW7EÐ¦fâö&÷VæE÷Fõö–ç7Fæ6UöÖöFUöææ÷FF–öå÷7Æ—G5öÖ—76–æuög&öÕ÷w&öær‚’°¢òòF†R7FFÆ–"Ö'W2f—‡GW&W2æVVBF†R'W2FV6ÂÖW&vVB–âÂW†7FÇ’0¢òòÆ÷vW%÷v—F…÷7FFÆ–%ö'W6FöW2f÷"f—‡GW&RäÔR(	BF†W6RF¶R¢òòÖöF–f–VB6÷W&6R7G&–ær–ç7FVBà¢ÆWBv—F…ö'W2ÒÇ7&3¢g7G'Â°¢ÆWBbÒ'6U÷6÷W&6R‡7&2’æW‡V7B‚''6W2"“°¢ÆWB'W5÷F‚ÒFƒ£¦æWr†Vçb‚$4$tõôÔä”dU5EôD•""’¢æ¦ö–â‚'7FFÆ–""¢æ¦ö–â‚$'W4†”Æ—FRæ&6‚"“°¢ÆWB'W5÷7&2Ò7FC£¦g3£§&VE÷Fõ÷7G&–ær‚f'W5÷F‚¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'&VB·Ó¢¶WÒ"Â'W5÷F‚æF—7Æ’‚’’“°¢ÆWB"Ò'6U÷6÷W&6R‚f'W5÷7&2’æW‡V7B‚'7FFÆ–"'W2'6W2"“°¢ÖW&vS£¦ÖW&vUöf÷%÷6–Ò‡fV2¶bÂ%ÒÂæöæR’æW‡V7B‚&ÖW&vR"¢Ó°¢ÆWBÆ÷vW%ö'W2ÒÇ7&3¢g7G'ÂÆ÷vW#£¦Æ÷vW%÷&öw&Ò‚gv—F…ö'W2‡7&2’“° ¢òò–æ—F–F÷"$dÓ¢7F—fV—2&WV—&VBà¢ÆWB&fÒÒf—‡GW&R‚'&Vv&Æö6µö&6–5÷FW7Bæ†&2"“°¢6öç7B$dÕôÄUC¢g7G"Ò&ÆWB†VÇW"¢†–Ä†VÇW"7F—fRÒ&–æB†–Â#°¢76W'B†&fÒæ6öçF–ç2„$dÕôÄUB’Â&f—‡GW&R6†R6†ævVB"“°¢Æ÷vW%ö'W2‚f&fÒ’æW‡V7B‚'F†R7F—fV6öçG&öÂÆ÷vW'2"“° ¢òòF&vWB&W7öæFW#¢76—fV—2&WV—&VBà¢ÆWBFwBÒf—‡GW&R‚'FÆÕ÷F&vWE÷F‡&VEö–e÷FW7Bæ†&2"“°¢6öç7BDuEôÄUC¢g7G"Ò&ÆWBF&vWB¢FÆÔÖVÕF&vWB76—fRÒ&–æBÖVÒ#°¢76W'B‡FwBæ6öçF–ç2…DuEôÄUB’Â&f—‡GW&R6†R6†ævVB"“°¢Æ÷vW%÷7&2‚gFwB’æW‡V7B‚'F†R76—fV6öçG&öÂÆ÷vW'2"“° ¢òòæòææ÷FF–öã¢–çfÆ–FB&÷F‚6—FW2ÂæBc&VgW6W2Föòà¢f÷"‡v†BÂ7&2ÂcöÆ÷vW"’–â°¢€¢&–æ—F–F÷"$dÒ"À¢&fÒç&WÆ6Vâ„$dÕôÄUBÂ&ÆWB†VÇW"¢†–Ä†VÇW"Ò&–æB†–Â"Â’À¢G'VRÀ¢’À¢€¢'F&vWB&W7öæFW""À¢FwBç&WÆ6Vâ…DuEôÄUBÂ&ÆWBF&vWB¢FÆÔÖVÕF&vWBÒ&–æBÖVÒ"Â’À¢fÇ6RÀ¢’À¢Ò°¢ÆWBW'"Ò–bcöÆ÷vW"°¢Æ÷vW%ö'W2‚g7&2’çVçw&öW'"‚¢ÒVÇ6R°¢Æ÷vW%÷7&2‚g7&2’çVçw&öW'"‚¢Ó°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fW'"“°¢76W'B€¢×6ræ6öçF–ç2‚&æVVG2â7F—fVö76—fVÖöFRææ÷FF–öâ"’À¢'·v†GÓ¢¶×6wÒ ¢“°¢Ð ¢òòF†Ru$ôärææ÷FF–öã¢cF¶W2—BæBG&÷2—Bà¢ÆWB&fÕ÷76—fRÒ&fÒç&WÆ6Vâ„$dÕôÄUBÂ&ÆWB†VÇW"¢†–Ä†VÇW"76—fRÒ&–æB†–Â"Â“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%ö'W2‚f&fÕ÷76—fR’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B€¢×6ræ6öçF–ç2‚&–æ—F–F÷"Ô$dÒ–ç7Fæ6R"’bb×6ræ6öçF–ç2‚&FV6Æ&VB76—fV"’À¢'¶×6wÒ ¢“° ¢ÆWBFwEö7F—fRÒFwBç&WÆ6Vâ…DuEôÄUBÂ&ÆWBF&vWB¢FÆÔÖVÕF&vWB7F—fRÒ&–æBÖVÒ"Â“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚gFwEö7F—fR’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B€¢×6ræ6öçF–ç2‚'F&vWBÕDÄÒ&W7öæFW"–ç7Fæ6R"’bb×6ræ6öçF–ç2‚&FV6Æ&VB7F—fV"’À¢'¶×6wÒ ¢“° ¢òòF†RÖV7W&VÖVçBF†R7FGW2&W7G2öâÂB$õD‚6—FW2ââV&Æ–W ¢òòfW'6–öâF–BöæÇ’F†RF&vWBÂ&f÷"F†R6—FRv†÷6Rf—‡GW&RæVVG2æð¢òò7FFÆ–"'W2"(	B'WBF†—26ÖRFW7BÖW&vW27FFÆ–"'W2fWrÆ–æW0¢òò&VÆ÷rÂ6òF†R&V6öâF–Bæ÷B†öÆBà¢76W'EöW€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gFwEö7F—fR’’æW‡V7B‚'cVÖ—G2"’À¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gFwB’’æW‡V7B‚'cVÖ—G2"’À¢'cG&÷2F†RÖöFRææ÷FF–öâöâF&vWB&W7öæFW" ¢“°¢76W'EöW€¢7÷F#£¦VÖ—B‚gv—F…ö'W2‚f&fÕ÷76—fR’’æW‡V7B‚'cVÖ—G2"’À¢7÷F#£¦VÖ—B‚gv—F…ö'W2‚f&fÒ’’æW‡V7B‚'cVÖ—G2"’À¢'cG&÷2F†RÖöFRææ÷FF–öâöââ–æ—F–F÷"$dÒ ¢“° ¢òòçF’×f7V—G“¢cFöW2†öæ÷W"F†RÖöFRv†W&R—BÖVç26öÖWF†–ærà¢ÆWB&÷F…ö†ÇfW2Òf—‡GW&R‚&†–Æ—FUö&÷VæEöÖöå÷FW7Bæ†&2"“°¢6öç7BÔôã¢g7G"Ò&ÆWBÖöâ¢†–Å†7F÷"76—fRÒ&–æB†–Â#°¢76W'B†&÷F…ö†ÇfW2æ6öçF–ç2„Ôôâ’Â&f—‡GW&R6†R6†ævVB"“°¢76W'EöæR€¢7÷F#£¦VÖ—B‚gv—F…ö'W2‚f&÷F…ö†ÇfW2’’æW‡V7B‚'cVÖ—G2"’À¢7÷F#£¦VÖ—B‚gv—F…ö'W2‚f&÷F…ö†ÇfW2ç&WÆ6Vâ€¢ÔôâÀ¢dÔôâç&WÆ6R‚'76—fR"Â&7F—fR"’À¢¢’’¢æW‡V7B‚'cVÖ—G2"’À¢&f÷"G&ç67F÷"v—F‚&÷F‚†ÇfW2F†RÖöFR×W7B6†ævRcw2÷WGWB ¢“°§Ð ¢òòòF†RWfVçBÖG&—fVâfÆf÷W"öbF†RÖöFRÖÆW72G&ç67F÷"d”TÄBÂæBF†P¢òòò76—fV6–&Æ–ærF†BFöW2äõB6†ævRv—F‚—Bà¢òòð¢òòòÂf–VÆBÂcÀ¢òòòÂÒÒ×ÂÒÒ×À¢òòòÂG'b¢6÷VçFW$G'f(	BæòÖöFRÂ&VgW6W3¢'G&ç67F÷"f–VÆB÷F"æG'b¢6÷VçFW$G'f†2æòÖöFRæBâââ"À¢òòòÂG'b¢6÷VçFW$G'b76—fVÂ†æFÆW"–ç6–FRv†Vâ7F—fVÂVÖ—G2ÂæB6÷'&V7FÇ’ôÔ•E2F†R&Vv—7G&F–öâÀ¢òòòÂ2¢6öç7VÖW"76—fVÂ†æFÆW"–âF†RÅt•2Ôôâ&öG’ÂVÖ—G2ÂæB6÷'&V7FÇ’´TU2—B(	B'—FRÖ–FVçF–6ÂFò7F—fVÀ¢òòð¢òòò6òF†RGvò†ÇfW2öböæR6öç7G'V7B'B6ö×ç“¢Ö—76–æp¢òòòææ÷FF–öâ—2&öw&ÒW'&÷"VæFW"&÷F‚&6¶VæG2ÂæB76—fV ¢òòòöæR—2ÆVvÂ&öw&Òc'Vç2f—F†gVÆÇ’æBD"Ô•"FöW2æ÷BÆ÷vW"à¢òòòF†R6V6öæB¶VW2—G27VvvW7F–öâf÷"W†7FÇ’F†B&V6öâà¢òòð¢òòòF†RF†—&B&÷r—2v‡’F†R&Òw2FWF–ÂæòÆöævW"6Æ–×2F†R†æFÆW ¢òòò&öæÇ’&Vv—7FW'2öââ7F—fV–ç7Fæ6R"(	BG'VRöbF†Rv†Và¢òòò7F—fV6†R—Bv2w&—GFVâg&öÒÂfÇ6RöbF†R÷F†W"öæRVæFW"F†P¢òòò6ÖR&Òà¢òòð¢òòòöæÇ’F†W6RGvò&×2vW&RÖV7W&VBâF†R&ÆÆVÂEUB×ö¶–ærÔ$dÒ— ¢òòòÆöæw6–FRF†VÒæVVG2F†RG&ç67F÷"†VÆB'’âVçfFò&R&V6†V@¢òòòBÆÂÂv†–6‚æò&ö&R†W&R'V–ÆG2Â6ò—B—2ÆVgBÆöæR&F†W"F†à¢òòò&V6Æ76–f–VB'’æÆöw’à¢5·FW7EÐ¦fâöÖöFUöÆW75÷G&ç67F÷%öf–VÆEö—5ö÷&öw&ÕöW'&÷%ö'WEö÷76—fUööæUö—5öæ÷B‚’°¢ÆWB&6RÒf—‡GW&R‚&WfVçEöG&—fVå÷G&ç67F÷%÷FW7Bæ†&2"“°¢òòF†RDU5D$Tä4‚f–VÆBÂæ÷BF†RVçböæR(	BâVçbÖ†VÆBf–VÆBv—F‚æð¢òòÖöFRÆ÷vW'2VæFW"D"Ô•"æBæWfW"&V6†W2F†—2&Òà¢ÆWBF%öBÒ&6P¢æf–æB‚'FW7F&Væ6‚WfVçDG&—fVåG&ç67F÷%F""¢æW‡V7B‚&f—‡GW&R6†R6†ævVB"“°¢ÆWB††VBÂF–Â’Ò&6Rç7Æ—EöB‡F%öB“°¢6öç7Bd”TÄC¢g7G"Ò"G'b¢6÷VçFW$G'b7F—fR#°¢76W'B‡F–Âæ6öçF–ç2„d”TÄB’Â&f—‡GW&R6†R6†ævVB"“°¢ÆWBv—F‚ÒÇ&WÃ¢g7G'Âf÷&ÖB‚'¶†VG×·Ò"ÂF–Âç&WÆ6Vâ„d”TÄBÂ&WÂÂ’“° ¢Æ÷vW%÷7&2‚f&6R’æW‡V7B‚'F†R7F—fV6öçG&öÂÆ÷vW'2"“° ¢òòæòÖöFS¢–çfÆ–FÂæBc&VgW6W2Föòà¢ÆWBÖöFVÆW72Òv—F‚‚"G'b¢6÷VçFW$G'b"“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚fÖöFVÆW72’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚&æVVG2â7F—fVö76—fVÖöFRææ÷FF–öâ"’À¢'¶×6wÒ ¢“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fÖöFVÆW72’’æW‡V7EöW'"‚'c&VgW6W2—BFöò"“°¢76W'B†f÷&ÖB‚'·cÒ"’æ6öçF–ç2‚&†2æòÖöFR"’Â'·cÒ"“° ¢òò76—fS¢7F–ÆÂVç7W÷'FVFÂ&V6W6Rc&VÆÇ’FöW2'Vâ—B(	@¢òòæB'Vç2—B$”t…BâF†Röâ&W†æFÆW"Æ—fW2–ç6–FRv†Và¢òò7F—fVÂ6òcöÖ—GF–ær—G2&Vv—7G&F–öâöâ76—fR–ç7Fæ6R—0¢òòF†RÆæwVvRw2÷vâ'VÆRÂæ÷BÖ—2ÖÆ÷vW&–ærà¢ÆWB76—fRÒv—F‚‚"G'b¢6÷VçFW$G'b76—fR"“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚g76—fR’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚'76—fRWfVçBÖG&—fVâG&ç67F÷"f–VÆB"’À¢'¶×6wÒ ¢“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g76—fR’’æW‡V7B‚'cVÖ—G2F†R76—fR&öw&Ò"“°¢76W'B€¢cæ6öçF–ç2‚%÷F"æG'bç&WçW6…ö&6²‚"’À¢'c×W7BöÖ—BF†Rv†Vâ7F—fV†æFÆW"&Vv—7G&F–öâöâ76—fR–ç7Fæ6R ¢“°¢òòçF’×f7V—G“¢F†R7F—fR&öw&ÒFöW2&Vv—7FW"—Bà¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f&6R’¢æW‡V7B‚'cVÖ—G2"¢æ6öçF–ç2‚%÷F"æG'bç&WçW6…ö&6²‚"’À¢'F†R7F—fR6öçG&öÂ×W7B&Vv—7FW"F†R†æFÆW"Â÷"F†R6†V6²&÷fR—2V×G’ ¢“° ¢òòF†RõD„U"6†RVæFW"F†R6ÖR&ÒÂæBF†R&V6öâ—G2FWF–Âæð¢òòÆöævW"6—2F†R†æFÆW"&öæÇ’&Vv—7FW'2öââ7F—fV ¢òò–ç7Fæ6R"âÖ÷fRF†R–âWfVçFæB—G2†æFÆW"÷WBöbv†Và¢òò7F—fVæBcw276—fR÷WGWB—2'—FRÖ–FVçF–6ÂFò—G27F—fP¢òò÷WGWC¢F†R†æFÆW"•2&Vv—7FW&VBÂæB—Bf—&W2âF†RfW&F–7B—0¢òòVæffV7FVB(	Bc'Vç2&÷F‚6†W26÷'&V7FÇ’(	B'WBöæR&ö&Rv0¢òò&V–ærFW67&–&VB2F†Rv†öÆR6öç7G'V7Bà¢ÆWBÇv—5ööâÒ"2&FöÖ–â7—4FöÖ–à¢g&WöÖ‡£¢ ¦VæBFöÖ–â7—4FöÖ–à §G&ç67F÷"6öç7VÖW ¢&W¢–âWfVçCÇV–çCÃƒãà¢6VVâ¢V–çCÃ3#âFVfVÇB  ¢öâ&W†â¢6VVâÒ6VVâ²à¢VæBöà¦VæBG&ç67F÷"6öç7VÖW  §FW7F&Væ6‚6öç5F ¢GWB¢F÷ ¢2¢6öç7VÖW"ÔôDP¦VæBFW7F&Væ6‚6öç5F  ¦–×Â6öç5FW7Bf÷"6öç5F ¢6Æö6²6Æ²Ò7—4FöÖ–à¢'Và¢VÖ—B2ç&Wƒ2¢v—B7–6ÆP¢76W'B2ç6VVâÓÒ2VÇ6Rf–Â‚'6VVãÒG¶2ç6VVçÒ"¢VæB'Và¦VæB–×Â6öç5FW7B"3°¢ÆWBÖöFRÒÆÓ¢g7G'ÂÇv—5ööâç&WÆ6R‚$6öç7VÖW"ÔôDR"Âff÷&ÖB‚$6öç7VÖW"¶×Ò"’“°¢òòF†RÖV7W&VÖVçBF†—2FW7BW†—7G2f÷"ÂæB—B—2Væ6†ævVC¢v—F€¢òòâÅt•2Ôôâ†æFÆW"cVÖ—G2'—FRÖ–FVçF–6ÆÇ’f÷"&÷F‚ÖöFW2æ@¢òò&Vv—7FW'2F†R†æFÆW"V—F†W"v’âF†Rææ÷FF–öâ—2–æW'BF†W&Rà¢76W'EöW€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fÖöFR‚'76—fR"’’’æW‡V7B‚'cVÖ—G2"’À¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fÖöFR‚&7F—fR"’’’æW‡V7B‚'cVÖ—G2"’À¢'v—F‚âÇv—2Ööâ†æFÆW"cG&VG276—fRæB7F—fR–FVçF–6ÆÇ’ ¢“°¢76W'B€¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fÖöFR‚'76—fR"’’¢æW‡V7B‚'cVÖ—G2"¢æ6öçF–ç2‚%÷F"æ2ç&WçW6…ö&6²‚"’À¢&æB—BFöW2&Vv—7FW"F†R†æFÆW" ¢“°¢òòv†BD"Ô•"Ö¶W2öbF†B†26–æ6RÖ÷fVBÂæB–âF†RF—&V7F–öà¢òòF†RÖV7W&VÖVçBö–çG3¢76—fV(	BF†Rææ÷FF–öâÖF6†–ærv†@¢òòc7GVÆÇ’FöW2(	Bæ÷rÄõtU%2ÂæB7F—fVÂv†–6‚6Æ–×2à¢òò÷væW'6†—cFöW2æ÷Bv—fR—BÂ—2&öw&ÒW'&÷"âF†—2&ÒW6V@¢òòFò&V¦V7B76—fV2væB66WB7F—fV2F†R6öçG&öÂà¢Æ÷vW%÷7&2‚fÖöFR‚'76—fR"’’æW‡V7B‚'F†R76—fR7VÆÆ–ærÆ÷vW'2"“°¢ÆWBÆ÷vW#£¤Æ÷vW$W'&÷#£¤–çfÆ–B†×6r’ÒÆ÷vW%÷7&2‚fÖöFR‚&7F—fR"’’çVçw&öW'"‚’VÇ6R°¢æ–2‚&7F—fVöââÇv—2ÖöâæÇ—6—26÷W&6R—2&öw&ÒW'&÷""“°¢Ó°¢76W'B€¢×6ræ6öçF–ç2‚&öæÇ’F†R76—fR÷væW'6†—ææ÷FF–öâ"’À¢'¶×6wÒ ¢“°§Ð ¢òòòF†Rd”eD‚ÆæF–æröbF†RæöâÖÆ—FW&ÂöâÄãâ7–6ÆW6W&–öBÂæBF†P¢òòòöæRF†BFöW2äõBÖ÷fR(	Bv†–6‚—2v†BÖ¶W2F†R÷F†W"f÷W"w0¢òòòfW&F–7BÖVâ6öÖWF†–ærà¢òòð¢òòò7FFVÖVçB×÷6—F–öâöâÄãâ7–6ÆW6—2&Vv—7FW&VBv†W&R—B—0¢òòòu$•EDTâÂ–ç6–FRF†R'Vâ&öG’ÂgFW"F†R–×Âw2ÆWF2†fR&VVà¢òòòVÖ—GFVBâ6òF†RW&–öBW‡&W76–öâ&W6öÇfW2Fòv†BF†R6÷W&6R6—3 ¢òòð¢òòòÂÆæF–ærÂ&Vv—7G&F–öâ6—G2ÂöâW"7–6ÆW6v—F‚ÆWBW"Ò&À¢òòòÂÒÒ×ÂÒÒ×ÂÒÒ×À¢òòòÂF‡&VR&÷VæB×FòG&ç67F÷"&×2ÂæBF†RFW7F&Væ6‚×66÷VBöæRÂæV"F†RF÷öbF†R'VâgVæ7F–öâÂ&Vf÷&RF†RÆWF2ÂVæ6ö×–Æ&ÆRÂ÷"&W6öÇfW2Fò6ÖRÖæÖVB6öç7FÀ¢òòòÂ7FFVÖVçB÷6—F–öâ‡F†—2öæR’ÂBF†R7FFVÖVçBÂgFW"F†RÆWF2Â&W6öÇfW2Fò"(	B6÷'&V7BÀ¢òòð¢òòò'V–ÇBæB'Vâv—F‚f–ÆR×66÷R6öç7BW"Òv&W6VçB2vVÆÂ(	@¢òòòF†R66RF†BÖ¶W2F†R÷F†W"f÷W"6–ÆVçFÇ”Ö—4Æ÷vW'6(	BF†—2öæP¢òòòf—&W2F–ÖW2–â#7–6ÆW2BW&–öB"ÂW†7FÇ’&–v‡BÂ&V6W6RF†P¢òòòÆWF6†F÷w2F†R6öç7BBF†Rö–çBöbW6Rà¢òòð¢òòò6òF†R&Ò¶VW2Vç7W÷'FVF¢c–×ÆVÖVçG2—BâÇ––ærF†R÷F†W ¢òòòÆæF–æw2rfW&F–7B†W&R'’æÆöw’v÷VÆB†fR&VVâw&öærÂæBF†P¢òòò6öç7G'V7B—2–FVçF–6Â(	BöæÇ’F†RVÖ—76–öâ÷6—F–öâF–ffW'2à¢5·FW7EÐ¦fâ÷7FFVÖVçE÷÷6—F–öå÷W&–öF–5÷W&–öE÷&W6öÇfW5ö6÷'&V7FÇ•÷VæFW%÷c‚’°¢ÆWB7&2ÒÆöã¢g7G"ÂW‡G&¢g7G'Â°¢f÷&ÖB€¢"2'¶W‡G&ÖFöÖ–â7—4FöÖ–à¢g&WöÖ‡£¢ ¦VæBFöÖ–â7—4FöÖ–à §FW7F&Væ6‚7F ¢GWB¢F÷ ¢â¢V–çCÃ3#âFVfVÇB ¦VæBFW7F&Væ6‚7F  ¦–×Â7FW7Bf÷"7F ¢6Æö6²6Æ²Ò7—4FöÖ–à¢ÆWBW"Ò  ¢'Và¢öâ¶öçÒ7–6ÆW0¢âÒâ²¢VæBöà¢v—B‚7–6ÆW0¢76W'BââVÇ6Rf–Â‚&ãÒG·¶ç×Ò"¢VæB'Và¦VæB–×Â7FW7B"0¢¢Ó° ¢òòF†RÆ—FW&Â6öçG&öÂÆ÷vW'2VæFW"D"Ô•"à¢Æ÷vW%÷7&2‚g7&2‚#""Â""’’æW‡V7B‚'F†RÆ—FW&ÂW&–öBÆ÷vW'2"“° ¢f÷"‡v†BÂW‡G&’–â°¢‚&&&RÆWF"Â""’À¢‚'6†F÷vVB'’6öç7B"Â&6öç7BW"ÒuÆåÆâ"’À¢Ò°¢ÆWB2Ò7&2‚'W""ÂW‡G&“°¢òòD"Ô•"&VgW6W2âF†R&Ò6'&–W26–ÆVçFÇ”Ö—4Æ÷vW'6f÷"F†P¢òò6¶Röb—G2õD„U"–çWB†öâ7–6ÆW6Â&VÆ÷r“²f÷"F†RæÖV@¢òòW&–öBÖV7W&VB†W&Rc—2vVçV–æRW66R†F6‚Âv†–6‚—0¢òòv†BF†R&W7BöbF†—2FW7B6†÷w2à¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g2’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B€¢×6ræ6öçF–ç2‚&æöâÖÆ—FW&Â÷"æöâ×÷6—F—fRW&–öB"’À¢'·v†GÓ¢¶×6wÒ ¢“° ¢òòæBF†R&V6öâF†R7VvvW7F–öâ—2†öæW7C¢cVÖ—G2F†RÆWF ¢òò$Tdõ$RF†R&Vv—7G&F–öâÂ6òF†R6Æ÷7W&R&VG2F†RÆWFæ@¢òòæ÷Bç—F†–ærVÇ6Rà¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g2’’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·v†GÓ¢cVÖ—G3¢¶WÒ"’“°¢ÆWBÇBÒc¢æf–æB‚&–çCcE÷BW"Ò#²"¢çVçw&ö÷%öVÇ6R‡ÇÂæ–2‚'·v†GÓ¢c×W7BVÖ—BF†RÆWF"’“°¢ÆWBW6VBÒc¢æf–æB‚%÷W&–öBÒ†–çCcE÷B’‡W"“²"¢çVçw&ö÷%öVÇ6R‡ÇÂæ–2‚'·v†GÓ¢c×W7BVÖ—BF†RæÖVBW&–öB"’“°¢76W'B€¢ÇBÂW6VBÀ¢'·v†GÓ¢F†RÆWF×W7B&V6VFRF†RW6R†W&R(	BF†B—2F†Rv†öÆRF–ffW&Væ6RÀ¢g&öÒF†Rf÷W"ÆæF–æw2F†BÖ—2ÖÆ÷vW" ¢“°¢Ð ¢òòæBF†R6öçG&7BÖFRW‡Æ–6—C¢v—F‚F†R6öç7B&W6VçBÂ—B—0¢òòVÖ—GFVBBæÖW76R66÷R„TBöbF†RÆWFÂæBF†RÆWF ¢òò7F–ÆÂv–ç2&V6W6R—B—2æV&W"âF†B—2F†R÷÷6—FRöbv†@¢òò†Vç2v†VâF†R&Vv—7G&F–öâ6—G2&÷fRF†RÆWFà¢ÆWB6†F÷vVBÒ7&2‚'W""Â&6öç7BW"ÒuÆåÆâ"“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g6†F÷vVB’’æW‡V7B‚'cVÖ—G2"“°¢ÆWB¶öç7BÒc¢æf–æB‚'7FF–26öç7FW‡"–çCcE÷BW"Òs²"¢æW‡V7B‚'cVÖ—G2F†R6öç7B"“°¢ÆWBÇBÒcæf–æB‚&–çCcE÷BW"Ò#²"’æW‡V7B‚'cVÖ—G2F†RÆWF"“°¢ÆWBW6VBÒc¢æf–æB‚%÷W&–öBÒ†–çCcE÷B’‡W"“²"¢æW‡V7B‚'cVÖ—G2F†RæÖVBW&–öB"“°¢76W'B†¶öç7BÂÇBbbÇBÂW6VBÂ&6öç7BÂF†VâÆWFÂF†VâF†RW6R"“° ¢òòF†R&Òw2õD„U"–çWBÂæBF†R&V6öâ—B—2æ÷BVç7W÷'FVF ¢òòFW7—FRWfW'—F†–ær&÷fS¢F%÷W&–öF–5öÆ—FW&Æç7vW'2æöæVf÷ ¢òòæöâ×÷6—F—fRÆ—FW&ÂFöòÂ6òöâ7–6ÆW6ÆæG2†W&RâcVÖ—G0¢òòF†R†æFÆW"æB—G2÷vâW&–öBâwV&BæWfW"ÆWG2—Bf—&R(	@¢òò'V–ÇBæB'VâÂf—&–æw2–â#7–6ÆW2âF†R&öw&Ò6¶VBf÷"¢òò†æFÆW"æBv÷B6–ÆVçBæòÖ÷à¢ÆWB¦W&òÒ7&2‚#"Â""“°¢ÆWB×6rÒ76W'Eöæ÷Eö–×ÆVÖVçFVB€¢fÆ÷vW%÷7&2‚g¦W&ò’çVçw&öW'"‚’À¢Æ÷vW#£¥c7FGW3£¥6–ÆVçFÇ”Ö—4Æ÷vW'2À¢“°¢76W'B†×6ræ6öçF–ç2‚&æöâ×÷6—F—fRW&–öB"’Â'¶×6wÒ"“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g¦W&ò’’æW‡V7B‚'cVÖ—G2F†R¦W&ò×W&–öB†æFÆW""“°¢76W'B€¢cæ6öçF–ç2‚%÷W&–öBÒ†–çCcE÷B’ƒ“²"’À¢'cVÖ—G2F†R¦W&òW&–öBfW&&F–Ò ¢“°¢76W'B€¢cæ6öçF–ç2‚%÷W&–öBâbb"’À¢&æBwV&G2—BÂ6òF†R†æFÆW"æWfW"'Vç2 ¢“°§Ð ¢òòòæÖ–ær6ö×öæVçBÖWF†öBF†BFöW2æ÷BW†—7BÂæBW6–ærfö–F ¢òòòÖWF†öBw2&W7VÇB2fÇVRâ6—‚&×27&÷72Gvòf–ÆW26–@¢òòòVç7W÷'FVFf÷"F†W6R(	B'&R×'Vâv—F‚ÒÖ6öFVvVâc"(	BæBc¢òòòVÖ—G2&÷F‚ÂfW&&F–ÒæBVæ6ö×–Æ&ÆS ¢òòð¢òòòÂ6÷W&6RÂcVÖ—G2Âr²²À¢òòòÂÒÒ×ÂÒÒ×ÂÒÒ×À¢òòòÂÆWB‚¢V–çCÃ3#âÒ2ææ÷7V6‚ƒ2–ÂV–çCcE÷B‚Ò2ææ÷7V6‚ƒ2“¶Â"w7G'V7B6Æ2r†2æòÖVÖ&W"æÖVBvæ÷7V6‚r"À¢òòòÂÆWB‚¢V–çCÃ3#âÒ2ææ÷&WBƒ2–ÂV–çCcE÷B‚Ò6Æ5öæ÷&WB†2Â2“¶Â'fö–BfÇVRæ÷B–væ÷&VB2—B÷Vv‡BFò&R"À¢òòð¢òòò&÷F‚&RG—RW'&÷'3¢6ÆÂFòÖWF†öBF†B—2æ÷BF†W&RÂæB¢òòòfÇVRF¶Vâg&öÒ6öÖWF†–ærF†B&öGV6W2æöæRâæò&6¶VæB'Vç0¢òòòV—F†W"Â–âç’6öæf–wW&F–öâ(	BVæÆ–¶RF†R6öææV7F&×2ÂF†W&R—0¢òòòæòVæ–ç7FçF–FVB÷6—F–öâf÷"7FFVÖVçB–â'Vâ&öG’Fò†–FR–âà¢òòò6ò–çfÆ–FÂv†–6‚—2v†BW‡'2ç'6w2G&ç67F÷"×6†VB6–&Æ–æp¢òòò†G&ç67F÷"Æ·ÕÆ†2æòÖWF†öBÆ·ÕÆ’†26–BÆÂÆöærà¢òòð¢òòòt„BD„•2DU5B5ETÄÅ’$T4„U2Â&V6W6R6—‚&×2—2æ÷B6—€¢òòòÖV7W&VÖVçG2â×WFF–ærV6‚&Ò–âGW&âæB&R×'Vææ–æs ¢òòð¢òòò¢F†R&W6öÇfW"w2F‚Öf÷&Ò&Ò–â6ö×öæVçG2ç'6(	B&V6†VBÂæ@¢òòòF†RöæÇ’ÆæF–ærf÷"Ö—76–ærÖWF†öBâ×WFF–ær—Bf–Ç2&VÆ÷rà¢òòò¢F†RVçG—VBÖÆWF'&WGW&ç2æòfÇVR"&Ò–â7F×G2ç'6(	@¢òòò&V6†VBâ×WFF–ær—Bf–Ç2&VÆ÷rà¢òòò¢F†RF‡&VR&†2æòÖWF†öB"&×2–â7F×G2ç'6(	BTå$T4„$ÄRà¢òòò5ö6ö×öæVçEöÖWF†öEö6ÆÆfÆ–FFW2F†RÖWF†öBöâWfW'’F€¢òòòF†B&WGW&ç2ö²…6öÖR‚ââ’–Â6ò6ÆÆW"†öÆF–ær&W6öÇfV@¢òòòÖWF†öBÇv—2†2öæRâ×WFF–ærç’öbF†VÒf–Ç2æ÷F†–ærà¢òòò¢F†RG—VBÖÆWFæB76–væÖVçB'&WGW&ç2æòfÇVR"&×2ÂæBF†P¢òòò&ÖWFW"Öf÷&Ò&W6öÇfW"&Ò(	B$T4„TBÂV6‚v—F‚GvòÖÆ–æP¢òòò&ö&RÂgFW"âV&Æ–W"fW'6–öâöbF†—2Fö2&V6÷&FVBÆÂF‡&VP¢òòò2&æ÷B&ö&VB"âF†W’&R6÷fW&VB&VÆ÷râF†R&ÖWFW"Öf÷&Ò&Ð¢òòò–â'F–7VÆ"ÖVç2Ö—76–ærÖWF†öB†2EtòÆæF–æw2Âæ÷BF†P¢òòòöæRF†BV&Æ–W"fW'6–öâ6Æ–ÖVBà¢òòð¢òòòæBF†R6WBF†—2&Òf—&W2öâ—2&æ÷BDT4Ä$TBÖWF†öB"Âv†–6‚—0¢òòòv–FW"F†â&FöW2æ÷BW†—7B#¢F†R'V–ÇBÖ–â&VF–6FW2–FÆVÀ¢òòò–FÆUö–æÂ–FÆUö÷WFæBV–W66VFÆæB†W&RFöòÂæB$õD‚&6¶VæG0¢òòò–×ÆVÖVçBF†÷6R(	BD"Ô•"öæR7FFVÖVçB÷6—F–öâ÷fW"âF†W’&R6'fV@¢òòò÷WBFòVç7W÷'FVFæB–ææVB&VÆ÷rà¢5·FW7EÐ¦fâöÖ—76–æuö÷%÷fö–Eö6ö×öæVçEöÖWF†öEö—5ö÷&öw&ÕöW'&÷"‚’°¢ÆWB7&2ÒÆ6ÆÃ¢g7G'Â°¢f÷&ÖB€¢"2&FöÖ–â7—4FöÖ–à¢g&WöÖ‡£¢ ¦VæBFöÖ–â7—4FöÖ–à ¦vVçB6Æ0¢62¢V–çCÃ3#âFVfVÇB ¢†öö¶&ÆRFB‡c¢V–çCÃƒâ’ÓâV–çCÃ3#à¢62Ò62²`¢&WGW&â60¢VæBF@¢†öö¶&ÆRæ÷&WB‡c¢V–çCÃƒâ¢62Ò62²`¢VæBæ÷&W@¦VæBvVçB6Æ0 §FW7BæÕFW7@¢ÆWBGWB¢F÷ ¢ÆWB2¢6Æ0¢6Æö6²6Æ²Ò7—4FöÖ–à¢'Và¢¶6ÆÇÐ¢VæB'Và¦VæBFW7BæÕFW7B"0¢¢Ó° ¢òòF†R6öçG&öÃ¢&VÂÖWF†öBv—F‚&VÂ&WGW&âfÇVRÆ÷vW'2à¢Æ÷vW%÷7&2‚g7&2€¢&ÆWB‚¢V–çCÃ3#âÒ2æFBƒ2•Æâ76W'B‚ÓÒ2VÇ6Rf–Â…Â'…Â"’"À¢’¢æW‡V7B‚'F†RvVÆÂÖf÷&ÖVB6ÆÂÆ÷vW'2"“° ¢f÷"‡v†BÂ6ÆÂÂvçB’–â°¢€¢'G—VBÆWBÂÖ—76–ærÖWF†öB"À¢&ÆWB‚¢V–çCÃ3#âÒ2ææ÷7V6‚ƒ2’"À¢&†2æòÖWF†öBæ÷7V6†"À¢’À¢€¢'VçG—VBÆWBÂÖ—76–ærÖWF†öB"À¢&ÆWB‚Ò2ææ÷7V6‚ƒ2’"À¢&†2æòÖWF†öBæ÷7V6†"À¢’À¢€¢'G—VBÆWBÂfö–BÖWF†öB"À¢&ÆWB‚¢V–çCÃ3#âÒ2ææ÷&WBƒ2’"À¢'&WGW&ç2æòfÇVR"À¢’À¢€¢'VçG—VBÆWBÂfö–BÖWF†öB"À¢&ÆWB‚Ò2ææ÷&WBƒ2’"À¢'&WGW&ç2æòfÇVR"À¢’À¢Ò°¢ÆWB2Ò7&2†6ÆÂ“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚g2’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‡vçB’Â'·v†GÓ¢¶×6wÒ"“° ¢òòcVÖ—G2—BÂv†–6‚—2v‡’F†RöÆB7VvvW7F–öâv2FVBVæC ¢òòF†RVÖ—GFVB2²²—2v†BFöW2æ÷B6ö×–ÆRÂöæR7FWÆFW"à¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g2’’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·v†GÓ¢cVÖ—G3¢¶WÒ"’“°¢Ð ¢òòF†RF‡&VR&×2âV&Æ–W"fW'6–öâöbF†—2FW7B&V6÷&FVB0¢òòVç&V6†&ÆRÖ'’×&ö&RâV6‚F¶W2GvòÆ–æW2à¢òð¢òò$T4õ$B×G—VBÆWF—2æ÷B6Æ–ÖVB'’F†RVçG—VB†æFÆW"(	BF†@¢òò&Ò—2wV&FVBöâ&V6÷&BG—RæÖRà¢ÆWB&V2Ò7&2‚&ÆWBB¢F–ç•G†âÒ2ææ÷&WBƒ2’"’ç&WÆ6Vâ€¢&vVçB6Æ2"À¢'7G'V7BF–ç•G†åÆâ¢V–çCÃƒâFVfVÇBÆæVæB7G'V7BF–ç•G†åÆåÆævVçB6Æ2"À¢À¢“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚g&V2’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚'&WGW&ç2æòfÇVR"’Â'&V6÷&BÆWC¢¶×6wÒ"“° ¢òòâ54”täÔTåB—2æ÷BÆWFÂ6òæ÷F†–ær6Æ–×2—Bf—'7Bà¢ÆWB6rÒ7&2‚&ÆWB‚¢V–çCÃ3#âÒÆâ‚Ò2ææ÷&WBƒ2’"“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚f6r’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚'&WGW&ç2æòfÇVR"’Â&76–væÖVçC¢¶×6wÒ"“° ¢òò4ôÕôäTåB×G—VB&ÖWFW"&V6†W2F†R&ÖWFW"Öf÷&Ò&W6öÇfW#°¢òòF†RG&ç67F÷"ÖÖWF†öB&ÒF†Bv2&ÆÖVBf÷"6Æ–Ö–ær—BöæÇ¢òòf—&W2f÷"G&ç67F÷"×G—VB&×2à¢ÆWB&ÒÒ"2&FöÖ–â7—4FöÖ–à¢g&WöÖ‡£¢ ¦VæBFöÖ–â7—4FöÖ–à ¦vVçBÖöFVÀ¢b¢V–çCÃ3#âFVfVÇB¢†öö¶&ÆRvWB‚’ÓâV–çCÃ3#à¢&WGW&â`¢VæBvW@¦VæBvVçBÖöFVÀ §66÷&V&ö&Bö'0¢6VVâ¢V–çCÃ3#âFVfVÇB ¢gVæ7F–öâö'6W'fR†¢V–çCÃƒâÂÓ¢ÖöFVÂ¢6VVâÒ6VVâ²¢VæBö'6W'fP¦VæB66÷&V&ö&Bö'0 §FW7BFW7@¢ÆWBGWB¢F÷ ¢ÆWBÒ¢ÖöFVÀ¢6Æö6²6Æ²Ò7—4FöÖ–à¢'Và¢v—B7–6ÆP¢VæB'Và¦VæBFW7BFW7B"3°¢ÆWB&ÒÒ&Òç&WÆ6Vâ€¢"6VVâÒ6VVâ²"À¢"ÆWB"¢V–çCÃ3#âÒÒææ÷7V6‚†•Æâ6VVâÒ6VVâ²""À¢À¢“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚g&Ò’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚&†2æòÖWF†öBæ÷7V6††öâ&ÖWFW"Ö’"’À¢'&ÖWFW"f÷&Ó¢¶×6wÒ ¢“° ¢òòF†R6'fRÖ÷WBF‡&÷Vv‚F†R$ÔUDU"f÷&ÒÂ–ææVB6W&FVÇ’g&öÐ¢òòF†RF‚f÷&Òâv—F†÷WBF†—2F†R&Ò—2VæwV&FVC¢GW&æ–ær—G0¢òò—5ö'V–ÇF–åö6ö×öæVçE÷&VF–6FV6ÆÂ–çFò–bfÇ6Rbf76V@¢òòF†RVçF—&R7V—FRÂ6òF†RöæR&ÒF†R6'fRÖ÷WBW†—7G2Fò7&VFP¢òòv2F†RöæRæ÷F†–ærÖV7W&VBà¢ÆWB&Õö'V–ÇF–âÒ&Òç&WÆ6Vâ‚&Òææ÷7V6‚†’"Â&Òæ–FÆR†’"Â“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚g&Õö'V–ÇF–â’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚&öâ6ö×öæVçB×G—VB&ÖWFW""’À¢'&ÖWFW"Öf÷&Ò6'fRÖ÷WC¢¶×6wÒ ¢“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g&Õö'V–ÇF–â’¢æW‡V7B‚'cVÖ—G2F†R'V–ÇBÖ–â&VF–6FRöâ&ÖWFW""“° ¢òòæBF†R6'fRÖ÷WC¢%T”ÅBÔ”â&VF–6FR—2æ÷BFV6Æ&VBÖWF†ö@¢òòV—F†W"Â'WB&÷F‚&6¶VæG2–×ÆVÖVçB—B(	BD"Ô•"–âW‡&W76–öà¢òò÷6—F–öâÂcç—v†W&Râ—B¶VW2F†R7VvvW7F–öâà¢òð¢òòD…$TRÆæF–æw2&V6‚F†RF‚Öf÷&Ò&ÒÂæ÷BF†RGvòâV&Æ–W ¢òòfW'6–öâöbF†—2FW7B6†V6¶VBâ&&R7FFVÖVçB†2æò&–æF–æræ@¢òòæòÆö6ÂÂ6òÖW76vR&÷WB&&–æF–ær÷6—F–öâ"FW67&–&VBF†P¢òòw&öæröæRöbF†VÒà¢f÷"7F×B–â²&ÆWBÒ2æ–FÆRƒ"’"Â&2æ–FÆRƒ"’"Â'‚Ò2æ–FÆRƒ"’%Ò°¢ÆWB'V–ÇF–âÒ–b7F×Bç7F'G5÷v—F‚‚'‚Ò"’°¢7&2‚ff÷&ÖB‚&ÆWB‚¢V–çCÃ3#âÒÆâ·7F×GÒ"’¢ÒVÇ6R°¢7&2‡7F×B¢Ó°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚f'V–ÇF–â’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&'V–ÇBÖ–â&VF–6FR"’Â'·7F×GÓ¢¶×6wÒ"“°¢7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚f'V–ÇF–â’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·7F×GÓ¢cVÖ—G2F†R'V–ÇBÖ–â&VF–6FS¢¶WÒ"’“°¢Ð¢òòF†R6ÖR&VF–6FRöæR7FFVÖVçB÷6—F–öâ÷fW"Æ÷vW'2VæFW"D"Ô•"À¢òòv†–6‚—2v†BÖ¶W2–çfÆ–FfÇ6Rf÷"—Bà¢Æ÷vW%÷7&2‚g7&2‚&76W'B2æ–FÆRƒ"’VÇ6Rf–Â…Â'Â"’"’¢æW‡V7B‚%D"Ô•"Æ÷vW'2F†R&VF–6FR–âW‡&W76–öâ÷6—F–öâ"“° ¢òò6ö×öæVçBF†BDT4Ä$U2F†RæÖRvWG2—G2÷vâÖWF†öBÂöâ&÷F€¢òò&6¶VæG2(	BF†R'V–ÇBÖ–â—2FVfVÇBÂæ÷B&W6W'fVBv÷&BâD"Ô• ¢òòW6VBFò&V6‚5ö6ö×öæVçEö–FÆVf—'7BæBVÖ—BF†R†V'F&V@¢òòv–ç7B&öw&Òv†÷6R–FÆV&WGW&ç2rÂ6–ÆVçFÇ’F—6w&VV–æp¢òòv—F‚cw26Æ5ö–FÆR†2Â"–âæ÷rF†RFV6Æ&F–öâv–ç2Â6òF†P¢òò6ÆÂ—2â÷&F–æ'’6ö×öæVçBÖÖWF†öB6ÆÂæBF¶W2F†BF‚w0¢òò‡&RÖW†—7F–ærÂæÖRÖ–æFWVæFVçB’W‡&W76–öâ×÷6—F–öâvà¢f÷"æÖR–â²&–FÆR"Â&–FÆUö–â"Â&–FÆUö÷WB"Â'V–W66VB%Ò°¢ÆWBFV6Æ&VBÒ7&2‚ff÷&ÖB‚&76W'B2ç¶æÖWÒƒ"’ÓÒrVÇ6Rf–Â…Â&õÂ"’"’’ç&WÆ6Vâ€¢&vVçB6Æ2"À¢ff÷&ÖB€¢&vVçB6Æ5Æâ†öö¶&ÆR¶æÖWÒ†ã¢V–çCÃƒâ’ÓâV–çCÃ3#åÆâ&WGW&âuÆâÀ¢VæB¶æÖWÒ ¢’À¢À¢“°¢ÆWB×6rÒf÷&ÖB‚'·Ò"ÂÆ÷vW%÷7&2‚fFV6Æ&VB’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚ff÷&ÖB‚&ç¶æÖWÒ‚âââ–"’’À¢'¶æÖWÓ¢F†RFV6Æ&F–öâ×W7Bv–âÂÆVf–ærF†R÷&F–æ'’ÖWF†öBÖ6ÆÂv¢¶×6wÒ ¢“°¢òòF†R6ÖR&VgW6Ââ÷&F–æ'’æÖRvWG2(	BF†Rö–çB—2F†B—@¢òò—2æÖRÖ–æFWVæFVçBæ÷rà¢ÆWB÷&F–æ'’ÒFV6Æ&VBç&WÆ6Vâ†æÖRÂ&÷&F–æ'’"ÂB“°¢ÆWBÆ–âÒf÷&ÖB‚'·Ò"ÂÆ÷vW%÷7&2‚f÷&F–æ'’’çVçw&öW'"‚’“°¢76W'EöW€¢×6rç&WÆ6R†æÖRÂ&÷&F–æ'’"’À¢Æ–âÀ¢'¶æÖWÓ¢FV6Æ&VB'V–ÇBÖ–âæÖR×W7B&VgW6RW†7FÇ’Æ–¶Rç’÷F†W"ÖWF†öB ¢“°¢òòcF—7F6†W2FòF†RW6W"w2ÖWF†öBÂv†–6‚—2F†R&V†f–÷W ¢òòD"Ô•"æ÷rFV6Æ–æW2Fò6öçG&F–7Bà¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚fFV6Æ&VB’’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'¶æÖWÓ¢¶WÒ"’“°¢76W'B€¢cæ6öçF–ç2‚ff÷&ÖB‚$6Æ5÷¶æÖWÒ†2Â"’"’’À¢'¶æÖWÓ¢c6ÆÇ2F†RFV6Æ&VBÖWF†öB ¢“°¢Ð§Ð ¢òòòGvò7FFVÖVçB×÷6—F–öâöâÆWfVçCâ‚âââ–7V'67&—F–öâ&×2Â6–FR'¢òòò6–FR–âöæRgVæ7F–öâÂæBF†W’F¶R÷÷6—FRfW&F–7G2à¢òòð¢òòòÂ7V'67&—F–öâÂcÀ¢òòòÂÒÒ×ÂÒÒ×À¢òòòÂöâ2æö'2‡b–(	B6ö×öæVçBw2WfVçFf–VÆBÂ'’F‚Â÷F"ç2æö'2çW6…ö&6²‚âââ–v–ç7B&VÂÖVÖ&W"(	B¢¦6ö×–ÆW2æB'Vç2¢¢À¢òòòÂöâæ÷7V6‚‡b–(	BæÖRF†B&W6öÇfW2Fòæ÷F†–ærÂæ÷7V6‚çW6…ö&6²‚âââ–(	B"væ÷7V6‚rv2æ÷BFV6Æ&VB–âF†—266÷R"À¢òòð¢òòòF†Rf—'7B&÷rv2'V–ÇBæB%TâÂæ÷B§W7BVÖ—GFVC¢6VVãÓ6â6òc¢òòò–×ÆVÖVçG2—BæBF†R7VvvW7F–öâ—2†öæW7B(	BF†R&Òw2Gf–6P¢òòò‚'7V'67&–&Rg&öÒF†R6ö×öæVçBF†B÷vç2F†RWfVçFf–VÆB"’—0¢òòò&÷WBD"Ô•"w27V'6WBÂæ÷B&÷WBc&V–ær'&ö¶Vâà¢òòð¢òòòF†R6V6öæB—2âVæFVf–æVB–FVçF–f–W"Âv†–6‚—2&öw&ÒW'&÷ ¢òòòVæFW"&÷F‚&6¶VæG2âF†B—B•2öæÇ’F†Bv26†V6¶VB&F†W"F†à¢òòò77VÖVC¢FW7F&Væ6‚WfVçFf–VÆB—26Æ–ÖVB'’—G2÷vâ&Ò&Vf÷&P¢òòò&V6†–ær†W&RÂæBÆö6ÂF†B—2æ÷BâWfVçBfÆÇ2FòF†P¢òòò–çfÆ–F–ÖÖVF–FVÇ’&VÆ÷rà¢5·FW7EÐ¦fâ÷7FFVÖVçE÷÷6—F–öå÷7V'67&—F–öå÷7Æ—G5ööå÷v†WF†W%÷F†Uö6†ææVÅöW†—7G2‚’°¢ÆWB7&2ÒÆöã¢g7G'Â°¢f÷&ÖB€¢"2&FöÖ–â7—4FöÖ–à¢g&WöÖ‡£¢ ¦VæBFöÖ–â7—4FöÖ–à ¦vVçB7&0¢ö'2¢÷WBWfVçCÇV–çCÃƒãà¢†öö¶&ÆRf—&R‡c¢V–çCÃƒâ¢VÖ—Bö'2‡b¢VæBf—&P¦VæBvVçB7&0 §FW7F&Væ6‚7V%F ¢GWB¢F÷ ¢2¢7&0¢6VVâ¢V–çCÃ3#âFVfVÇB ¦VæBFW7F&Væ6‚7V%F  ¦–×Â7V%FW7Bf÷"7V%F ¢6Æö6²6Æ²Ò7—4FöÖ–à¢ÆWB6‚¢WfVçCÇV–çCÃƒãà ¢'Và¢öâ¶öçÒ‡b¢6VVâÒ6VVâ²`¢VæBöà¢VÖ—B6‚ƒ2¢v—B7–6ÆP¢76W'B6VVâÓÒ2VÇ6Rf–Â‚'6VVãÒG··6VVç×Ò"¢VæB'Và¦VæB–×Â7V%FW7B"0¢¢Ó° ¢òòF†R6öçG&öÃ¢7V'67&–&–ærFòF†RFW7B×66÷R6†ææVÂÆ÷vW'2à¢Æ÷vW%÷7&2‚g7&2‚&6‚"’’æW‡V7B‚'F†R–â×66÷R6†ææVÂÆ÷vW'2"“° ¢òò6ö×öæVçBw2WfVçBf–VÆB'’Fƒ¢D"Ô•"w27V'6WBvÂæBc¢òò&VÆÇ’—2F†Rv’÷WBà¢òð¢òòF†R7F–×VÇW2ÖGFW'2ÂæBâV&Æ–W"fW'6–öâöbF†—2FW7Bv÷B—@¢òòw&öæs¢—B7V'67&–&VBFò2æö'6æBF†VâVÖ—GFVBöâ6†Â6òF†P¢òò†æFÆW"6÷VÆBæWfW"f—&RæBF†R&'V–ÇBæB'VâÂ6VVãÓ2"6Æ–Ð¢òò&V6÷&FVBf÷"—Bv2ÖV7W&VBöâF–ffW&VçB&öw&Òâf—&–æp¢òò2æf—&Rƒ2–(	Bv†–6‚VÖ—G2ö'6–ç6–FRF†RvVçB(	B—2v†@¢òòÖ¶W2F†R7V'67&—F–öâö'6W'f&ÆRà¢ÆWBF‚Ò7&2‚'2æö'2"’ç&WÆ6Vâ‚"VÖ—B6‚ƒ2’"Â"2æf—&Rƒ2’"Â“°¢76W'B€¢F‚æ6öçF–ç2‚'2æf—&Rƒ2’"’À¢'F†R7F–×VÇW2×W7B7GVÆÇ’f—&R ¢“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚gF‚’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚&öâÇFƒâãÆWfVçCâ†&r–7V'67&—F–öâ"’À¢'¶×6wÒ ¢“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gF‚’’æW‡V7B‚'cVÖ—G2F†RF‚f÷&Ò"“°¢76W'B€¢cæ6öçF–ç2‚%÷F"ç2æö'2çW6…ö&6²‚"’bbcæ6öçF–ç2‚%7&5öf—&R‚"’À¢'c&Vv—7FW'2v–ç7BF†R&VÂÖVÖ&W"äBVÖ—G2F†R7F–×VÇW3¢·cÒ ¢“° ¢òòæÖRF†B&W6öÇfW2Fòæ÷F†–æs¢&öw&ÒW'&÷"ÂæBcw0¢òòGFV×BæÖW27–Ö&öÂF†BFöW2æ÷BW†—7Bà¢ÆWBVæ¶æ÷vâÒ7&2‚&æ÷7V6‚"“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚gVæ¶æ÷vâ’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&æÖW2æòWfVçB6†ææVÂ–â66÷R"’Â'¶×6wÒ"“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gVæ¶æ÷vâ’’æW‡V7B‚'cVÖ—G2"“°¢76W'B€¢cæ6öçF–ç2‚&æ÷7V6‚çW6…ö&6²‚"’À¢'cVÖ—G2F†RVæFVf–æVBæÖRfW&&F–Ó¢·cÒ ¢“° ¢òòÆö6ÂF†B—2æ÷BâWfVçBfÆÇ2FòF†R–çfÆ–F§W7B&VÆ÷p¢òòF†R&ÒVæFW"FW7BÂæ÷B–çFò—Bà¢ÆWBæ÷EöWfVçBÒ7&2‚&6‚"’ç&WÆ6Vâ‚"ÆWB6‚¢WfVçCÇV–çCÃƒãâ"Â"ÆWB6‚ÒR"Â“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚fæ÷EöWfVçB’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‚&—2æ÷BâWfVçB6†ææVÂ"’Â'¶×6wÒ"“° ¢òòv†BF†R&Ò5ETÄÅ’6F6†W2Âv†–6‚âV&Æ–W"fW'6–öâöbF†—0¢òòFW7BFW67&–&VB2&âVæFVf–æVB–FVçF–f–W"æBæ÷F†–ærVÇ6R"à¢òòWfW'’æÖR&VÆ÷r—2FV6Æ&VB6öÖWv†W&R–âF†R&öw&Ó²æöæR—2¢òòÆö6ÂÂ6òÆöö·WÖ—76W2æBF†W’ÆÂÆæB†W&RâcVÖ—G0¢òòÆæÖSâçW6…ö&6²‚âââ–f÷"V6‚æB&VgW6W2Fò6ö×–ÆR—BÂ6ð¢òòF†RfW&F–7B†öÆG2(	B'WBF†R6WB—2æ÷Bv†BF†R6öÖÖVçB6–Bà¢f÷"æÖR–â²'2"Â'6VVâ"Â&6Æ²"Â&GWB"Â%7&2"Â&f—&R%Ò°¢ÆWB2Ò7&2†æÖR“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚g2’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚&æÖW2æòWfVçB6†ææVÂ–â66÷R"’À¢'¶æÖWÓ¢¶×6wÒ ¢“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g2’’çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'¶æÖWÓ¢¶WÒ"’“°¢òòäÔRÂæ÷B7V'7G&–æs¢WfW'’vVæW&FVBFW7F&Væ6‚6öçF–ç0¢òò66†VBç6Æ÷G2çW6…ö&6²‚e÷'Vå÷6Æ÷B“¶Â6ò6öçF–ç2‚'2çW6…ö&6²‚"– ¢òò76VBf÷"æÖRÒ'2&öâç’÷WGWBBÆÂ(	B–æ6ÇVF–æröæP¢òòv—F‚F†R7V'67&—F–öâVÖ—76–öâFVÆWFVBâ&WV—&RF†R6†&7FW ¢òò&Vf÷&RF†RæÖRFò&RæöâÖ–FVçF–f–W"ÂæöâÖæ&÷VæF'’à¢ÆWBæVVFÆRÒf÷&ÖB‚'¶æÖWÒçW6…ö&6²‚"“°¢76W'B€¢cæÆ–æW2‚’æç’‡ÆÇÂÂæÖF6…ö–æF–6W2‚fæVVFÆR’æç’‡Â†’Âò—Â°¢Å²âæ•Ð¢æ6†'2‚¢ææW‡Eö&6²‚¢æ—5öæöæUö÷"‡Æ7Â2æ—5öÇ†çVÖW&–2‚’bb2Òuòrbb2Òrâr¢Ò’’À¢'¶æÖWÓ¢cVÖ—G2F†RæÖRfW&&F–Ò ¢“°¢Ð ¢òòFW7F&Væ6‚WfVçFd”TÄBFöW2æ÷B&V6‚F†—2&Ò(	B'WBæ÷Bf÷ ¢òòF†R&V6öââV&Æ–W"fW'6–öâöbF†—2FW7B76W'FVBâ—BF–W2@¢òòf–VÆBÖFV6Æ&F–öâÆ÷vW&–ærÂÆöær&Vf÷&RF†R'Væ&öG’Â6òF†P¢òòöÆB76W'F–öâ76VBf÷"âVç&VÆFVB&V6öâæBv÷VÆB†fP¢òò76VBv—F‚F†Rv†öÆR7V'67&—F–öâ&ÒFVÆWFVBâ–âF†R&VÀ¢òò6W6R–ç7FVBà¢ÆWBF%öf–VÆBÒ7&2‚&6‚"¢ç&WÆ6Vâ€¢"6VVâ¢V–çCÃ3#âFVfVÇB"À¢"6VVâ¢V–çCÃ3#âFVfVÇBÆâFWb¢WfVçCÇV–çCÃƒãâ"À¢À¢¢ç&WÆ6Vâ‚&öâ6‚‡b’"Â&öâFWb‡b’"Â“°¢ÆWB×6rÒf÷&ÖB‚'·Ò"ÂÆ÷vW%÷7&2‚gF%öf–VÆB’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚'FW7F&Væ6‚f–VÆBFWf"’À¢'F†Rf–VÆBFV6Æ&F–öâ—2v†B&VgW6W2—C¢¶×6wÒ ¢“°§Ð ¢òòòVWVRÖWF†öB–â5DDTÔTåB÷6—F–öâF†B—2æV—F†W"W6†æ÷ ¢òòò÷âf—fR&×2(	BFW7F&Væ6‚Ö÷væVBf–VÆBÂ66÷&V&ö&BVWVRÀ¢òòò6ö×öæVçBVWVRÂ&&RF&vWB×7FFRf–VÆBÂ–ç7Fæ6R×VÆ–f–V@¢òòòF&vWB×7FFRf–VÆB(	BæBV6‚7Æ—G2Â&V6W6RcVÖ—G2F†R6ÆÀ¢òòòv–ç7B†&5÷'C£¤†&5VWVVÂv†÷6Rv†öÆR’—0¢òòòW6†ö÷ö6—¦VöV×G–à¢òòð¢òòòÂ7FFVÖVçBÂcVÖ—G2Âr²²À¢òòòÂÒÒ×ÂÒÒ×ÂÒÒ×À¢òòòÂ6"çç6—¦R‚–Â÷F"ç6"çç6—¦R‚“¶Â6ö×–ÆW2(	BF†RfÇVR—2F—66&FVBÂ6ò—B—2ÆVvÂæòÖ÷À¢òòòÂ6"çæV×G’‚–Â÷F"ç6"çæV×G’‚“¶Â6ö×–ÆW2Â6ÖRÀ¢òòòÂ6"çæ6ÆV"‚–Â÷F"ç6"çæ6ÆV"‚“¶Â"w7G'V7B†&5÷'C£¤†&5VWVSÆÆöærVç6–væVB–çCâr†2æòÖVÖ&W"æÖVBv6ÆV"r"À¢òòòÂ6"çæg&öçB‚–Â÷F"ç6"çæg&öçB‚“¶Â6ÖRÂæòg&öçFÀ¢òòð¢òòò6ò6—¦VöV×G–¶VWF†R7VvvW7F–öâ(	Bc'Vç2F†÷6R&öw&×2(	Bæ@¢òòòWfW'—F†–ærVÇ6R—2&öw&ÒW'&÷"æò&6¶VæB'Vç2à¢òòð¢òòòÆÂd•dRÆæF–æw2vW&R&ö&VB–æFWVæFVçFÇ’&F†W"F†âf÷W ¢òòò–æfW'&VBg&öÒöæR(	BFW7F&Væ6‚Ö÷væVBf–VÆBÂ66÷&V&ö&BVWVRÀ¢òòò6ö×öæVçBVWVRÂ&&RF&vWB×7FFRf–VÆBÂ–ç7Fæ6R×VÆ–f–V@¢òòòF&vWB×7FFRf–VÆB(	BæBÆÂf—fR&V†fRF†—2v’âF‡&VRöbF†VÐ¢òòòW6VBFò6''’†æB×w&—GFVâVÖ—G5Væ6ö×–Æ&ÆV–ç7FVBÂv†–6€¢òòòÖV7W&VÖVçB6öçG&F–7G3¢VæBç6—¦R‚–VÖ—G2÷F"çVæBç6—¦R‚“¶À¢òòòVæF–ærç6—¦R‚–VÖ—G26VÆbçVæF–ærç6—¦R‚“¶æ@¢òòòÖöFVÂçVæF–ærç6—¦R‚–VÖ—G2÷F"æÖöFVÂçVæF–ærç6—¦R‚“¶ÂæBr²°¢òòò6ö×–ÆW2ÆÂF‡&VRà¢5·FW7EÐ¦fâ÷VWVUöÖWF†öEö–å÷7FFVÖVçE÷÷6—F–öå÷7Æ—G5ööå÷F†U÷'VçF–ÖUö’‚’°¢ÆWB6"ÒÆÖWF†öC¢g7G'Â°¢f÷&ÖB€¢"2&FöÖ–â7—4FöÖ–à¢g&WöÖ‡£¢ ¦VæBFöÖ–â7—4FöÖ–à §66÷&V&ö&B6 ¢¢VWVSÇV–çCÃ3#ãà¦VæB66÷&V&ö&B6  §FW7F&Væ6‚F ¢GWB¢F÷ ¢6"¢6 ¦VæBFW7F&Væ6‚F  ¦–×ÂFW7Bf÷"F ¢6Æö6²6Æ²Ò7—4FöÖ–à¢'Và¢6"ççW6‚ƒ2¢6"çç¶ÖWF†öGÒ‚¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂFW7B"0¢¢Ó°¢ÆWB6ö×ÒÆÖWF†öC¢g7G'Â°¢f÷&ÖB€¢"2&FöÖ–â7—4FöÖ–à¢g&WöÖ‡£¢ ¦VæBFöÖ–â7—4FöÖ–à ¦vVçB6öÆÀ¢W''2¢VWVSÇV–çCÃ3#ãà¢†öö¶&ÆRæ÷FR‡c¢V–çCÃ3#â¢W''2çW6‚‡b¢W''2ç¶ÖWF†öGÒ‚¢VæBæ÷FP¦VæBvVçB6öÆÀ §FW7B7FW7@¢ÆWBGWB¢F÷ ¢ÆWB2¢6öÆÀ¢6Æö6²6Æ²Ò7—4FöÖ–à¢'Và¢2ææ÷FRƒ2¢v—B7–6ÆP¢VæB'Và¦VæBFW7B7FW7B"0¢¢Ó° ¢ÆWBF%öf–VÆBÒÆÖWF†öC¢g7G'Â°¢f÷&ÖB€¢"2&FöÖ–â7—4FöÖ–à¢g&WöÖ‡£¢ ¦VæBFöÖ–â7—4FöÖ–à §FW7F&Væ6‚F%¢GWB¢F÷ ¢VæB¢VWVSÇV–çCÃ3#ãà¦VæBFW7F&Væ6‚F% ¦–×ÂF%FW7Bf÷"F%¢6Æö6²6Æ²Ò7—4FöÖ–à¢'Và¢VæBçW6‚ƒ2¢VæBç¶ÖWF†öGÒ‚¢v—B7–6ÆP¢VæB'Và¦VæB–×ÂF%FW7B"0¢¢Ó°¢ÆWB7FFRÒf—‡GW&R‚'VWVU÷7FFUö†öö¶&ÆU÷FW7Bæ†&2"“°¢ÆWB&&U÷7FFRÒÆÖWF†öC¢g7G'Â°¢7FFRç&WÆ6Vâ€¢"VæF–ærçW6‚‡fÇVR’"À¢ff÷&ÖB‚"VæF–ærçW6‚‡fÇVR•ÆâVæF–ærç¶ÖWF†öGÒ‚’"’À¢À¢¢Ó°¢ÆWB–ç7E÷7FFRÒÆÖWF†öC¢g7G'Â°¢7FFRç&WÆ6Vâ€¢"ÖöFVÂæVçVWVRƒr’"À¢ff÷&ÖB‚"ÖöFVÂæVçVWVRƒr•ÆâÖöFVÂçVæF–ærç¶ÖWF†öGÒ‚’"’À¢À¢¢Ó° ¢f÷"‡v†BÂ7&2’–â°¢‚'66÷&V&ö&BVWVR"Âg6"2fG–âfâ‚g7G"’Óâ7G&–ær’À¢‚&6ö×öæVçBVWVR"Âf6ö×’À¢‚'FW7F&Væ6‚VWVRf–VÆB"ÂgF%öf–VÆB’À¢‚&&&RF&vWB×7FFRf–VÆB"Âf&&U÷7FFR’À¢‚&–ç7Fæ6R×VÆ–f–VBF&vWB×7FFRf–VÆB"Âf–ç7E÷7FFR’À¢Ò°¢òò6—¦VöV×G–(	BcVÖ—G2ÆVvÂæòÖ÷æB'Vç2F†R&öw&Òà¢f÷"ÖWF†öB–â²'6—¦R"Â&V×G’%Ò°¢ÆWB2Ò7&2†ÖWF†öB“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚g2’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚&–â7FFVÖVçB÷6—F–öâ"’À¢'·v†GÒ÷¶ÖWF†öGÓ¢¶×6wÒ ¢“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g2’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·v†GÒ÷¶ÖWF†öGÓ¢cVÖ—G3¢¶WÒ"’“°¢76W'B€¢cæ6öçF–ç2‚ff÷&ÖB‚"ç¶ÖWF†öGÒ‚“²"’’À¢'·v†GÒ÷¶ÖWF†öGÓ¢cVÖ—G2F†RF—66&FVB6ÆÂ ¢“°¢Ð ¢òòç—F†–ærVÇ6R(	BcVÖ—G26ÆÂ†&5VWVVFöW2æ÷B†fRà¢f÷"ÖWF†öB–â²&6ÆV""Â&g&öçB"Â&æ÷7V6‚%Ò°¢ÆWB2Ò7&2†ÖWF†öB“°¢ÆWB×6rÒ76W'Eö–çfÆ–B‚fÆ÷vW%÷7&2‚g2’çVçw&öW'"‚’“°¢76W'B€¢×6ræ6öçF–ç2‚&†2öæÇ’W6†Â÷Â6—¦VæBV×G–"’À¢'·v†GÒ÷¶ÖWF†öGÓ¢¶×6wÒ ¢“°¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g2’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·v†GÒ÷¶ÖWF†öGÓ¢cVÖ—G3¢¶WÒ"’“°¢76W'B€¢cæ6öçF–ç2‚ff÷&ÖB‚"ç¶ÖWF†öGÒ‚“²"’’À¢'·v†GÒ÷¶ÖWF†öGÓ¢cVÖ—G2F†R6ÆÂfW&&F–ÒÂv†–6‚—2v†Bf–Ç2Fò6ö×–ÆR ¢“°¢Ð¢Ð ¢òòF†R7Æ—BG&6·2F†R%TåD”ÔRw2’Â6ò–âF†BF†RGvòæÖW2—@¢òòÆWG2F‡&÷Vv‚&RF†RGvòF†R†VFW"7GVÆÇ’FV6Æ&W2â–`¢òò†&5VWVVw&÷w2ÖWF†öBÂF†—2f–Ç2æBF†RÆ—7BvWG2WFFV@¢òò&F†W"F†âv÷&¶–ær6ÆÂ&V–ær&W÷'FVB2&öw&ÒW'&÷"à¢ÆWB†G"Ò7FC£¦g3£§&VE÷Fõ÷7G&–ær€¢Fƒ£¦æWr†Vçb‚$4$tõôÔä”dU5EôD•""’’æ¦ö–â‚''VçF–ÖRö†&5÷VWVU÷'Bæ‚"’À¢¢æW‡V7B‚''VçF–ÖR†VFW"&VF&ÆR"“°¢f÷"FV6Â–â²'fö–BW6‚‚"Â%B÷‚"Â&&ööÂV×G’‚"Â'6—¦U÷B6—¦R‚%Ò°¢76W'B††G"æ6öçF–ç2†FV6Â’Â$†&5VWVR×W7B7F–ÆÂFV6Æ&R¶FV6ÇÖ"“°¢Ð¢òò66âf÷"ÔTÔ$U"DT4Ä$D”ôå2Âæ÷Bç’ö67W'&Væ6S¢÷w2&öG¢òò6ÆÇ2öBæg&öçB‚–öâF†R–ææW"FWVRÂæB6öçF–ç2‚&g&öçB‚"– ¢òòÖF6†VBF†BæBf–ÆVBf÷"F†Rw&öær&V6öâà¢ÆWBFV6Æ&W2ÒÆæÖS¢g7G'Â°¢ÆWBæVVFÆRÒf÷&ÖB‚'¶æÖWÒ‚"“°¢†G"æÆ–æW2‚’æç’‡ÆÇÂ°¢òòDT4Ä$D”ôâ†2F†RæÖR&V6VFVB'’&WGW&âG—Ræ@¢òòv†—FW76S²6ÆÂ–ç6–FR&öG’†2—B&V6VFVB'’æÀ¢òòæBöBç÷ög&öçB‚–†2—B&V6VFVB'’öâÆÂF‡&VP¢òòF—7F–æ7F–öç2&RæVVFVB(	BÖF6†–ærF†R&&R7V'7G&–æp¢òòg&öçB†f÷VæB÷ög&öçB†æBf–ÆVBf÷"F†B&V6öâà¢ÂæÖF6…ö–æF–6W2‚fæVVFÆR’æç’‡Â†’Âò—Â°¢ÆWB&Vf÷&RÒfÅ²âæ•Ó°¢ÆWB&÷VæF'’Ò&Vf÷&P¢æ6†'2‚¢ææW‡Eö&6²‚¢æ—5öæöæUö÷"‡Æ7Â2æ—5öÇ†çVÖW&–2‚’bb2Òuòrbb2Òrâr“°¢&÷VæF'’bb&Vf÷&RçG&–Ò‚’æ—5öV×G’‚¢Ò¢Ò¢Ó°¢f÷"æÖR–â²'W6‚"Â'÷"Â&V×G’"Â'6—¦R%Ò°¢76W'B†FV6Æ&W2†æÖR’Â$†&5VWVR×W7B7F–ÆÂFV6Æ&R¶æÖWÖ"“°¢Ð¢òò(
fæBF†R6WB—24Äõ4TBâ7÷BÖ6†V6¶–ærGvò'6VçBæÖW2ÖFRF†—2¢òò&Æ6¶Æ—7C¢FF–ærB&6²‚’6öç7B²&WGW&âöBæ&6²‚“²ÖFòF†P¢òò†VFW"ÆVgBF†RFW7Bw&VVâv†–ÆRW''2æ&6²‚–¶WB&V–ær&W÷'FV@¢òò2&öw&ÒW'&÷"f÷"7FFVÖVçBc6ö×–ÆW2æB'Vç2âVçVÖW&FP¢òòv†BF†R7G'V7BFV6Æ&W2æB6ö×&RF†Rv†öÆR6WB–ç7FVBà¢ÆWB&öG’Ò°¢ÆWB7F'BÒ†G ¢æf–æB‚'7G'V7B†&5VWVR"¢æW‡V7B‚'7G'V7B†&5VWVR&W6VçB"“°¢ÆWB÷VâÒ†G%·7F'BâåÒæf–æB‚w²r’æW‡V7B‚'7G'V7B&öG’÷Vç2"’²7F'C°¢ÆWBVæBÒ†G%¶÷VââåÒæf–æB‚%ÆçÓ²"’æW‡V7B‚'7G'V7B&öG’6Æ÷6W2"’²÷Vã°¢f†G%¶÷VââæVæEÐ¢Ó°¢òò66âF†R7G'V7B&öG’BDUD‚öæÇ’Â÷fW"F†Rv†öÆRFW‡B&F†W ¢òòF†âÆ–æR'’Æ–æS¢FV6Æ&F–öâv†÷6R&WGW&âG—R6—G2öâ—G0¢òò÷vâÆ–æR†7FC£¦FWVSÅCåÆâG&–â‚’²ââçÖ’—2–çf—6–&ÆRFò¢òòW"ÖÆ–æR'VÆRÂæBFF–æröæRÆVgBF†—26†V6²w&VVâv†–ÆP¢òòVæBæG&–â‚–v2&V–ær6ÆÆVB&öw&ÒW'&÷"f÷"7FFVÖVç@¢òòc6ö×–ÆW2âç—F†–ær–ç6–FR²ââçÖ&öG’—26ÆÂÂæ÷B¢òòFV6Æ&F–öâÂ6òFWF‚—2v†B6W&FW2F†VÒ(	Bæ÷B–æFVçFF–öâà¢ÆWB×WBÖVÖ&W'3¢fV3Å7G&–æsâÒfV3£¦æWr‚“°¢°¢ÆWB"Ò&öG’æ5ö'—FW2‚“°¢ÆWB×WBFWF‚Ò“3#°¢ÆWB×WB’ÒW6—¦S°¢v†–ÆR’Â"æÆVâ‚’°¢ÖF6‚%¶•Ò°¢"w²rÓâFWF‚³ÒÀ¢"wÒrÓâFWF‚ÓÒÀ¢"r‚r–bFWF‚ÓÒÓâ°¢òòvÆ²&6²÷fW"F†R–FVçF–f–W"–ÖÖVF–FVÇ’&Vf÷&P¢òò†ÂF†Vâ&WV—&R4ôÔUD„”är&Vf÷&R—B†&WGW&à¢òòG—R’æBF†BF†R6†&7FW"&Vf÷&RF†RæÖR—0¢òòæ÷Bæ÷"'Böbæ÷F†W"–FVçF–f–W"(	Bv†–6‚—0¢òòv†BW†6ÇVFW2öBæg&öçB‚–æBöBç÷ög&öçB‚–à¢ÆWBVæBÒ&öG•²âæ•ÒçG&–ÕöVæB‚’æÆVâ‚“°¢ÆWB7F'BÒ&öG•²âæVæEÐ¢ç&f–æB‡Æ3¢6†'Â†2æ—5öÇ†çVÖW&–2‚’ÇÂ2ÓÒuòr’¢æÖ‡ÇÂ²¢çVçw&ö÷"ƒ“°¢ÆWBæÖRÒf&öG•·7F'BâæVæEÓ°¢ÆWB&Vf÷&RÒ&öG•²âç7F'EÒçG&–ÕöVæB‚“°¢ÆWB6WÒ&öG•²âç7F'EÒæ6†'2‚’ææW‡Eö&6²‚“°¢–bæÖRæ—5öV×G’‚¢bb&Vf÷&Ræ—5öV×G’‚¢bb6Wæ—5÷6öÖUöæB‡Æ7Â2æ—5÷v†—FW76R‚’¢bbæÖRæ6†'2‚’æÆÂ‡Æ7Â2æ—5öÇ†çVÖW&–2‚’ÇÂ2ÓÒuòr¢°¢ÖVÖ&W'2çW6‚†æÖRçFõ÷7G&–ær‚’“°¢Ð¢Ð¢òÓâ·Ð¢Ð¢’³Ò°¢Ð¢Ð¢ÖVÖ&W'2ç6÷'B‚“°¢ÖVÖ&W'2æFVGW‚“°¢76W'EöW€¢ÖVÖ&W'2À¢fV2°¢&V×G’"çFõ÷7G&–ær‚’À¢'÷"çFõ÷7G&–ær‚’À¢'W6‚"çFõ÷7G&–ær‚’À¢'6—¦R"çFõ÷7G&–ær‚¢ÒÀ¢&†&5VWVVw2ÖVÖ&W"6WB6†ævVC²VWVUöÖWF†öEö–å÷7FFVÖVçE÷÷6—F–öæ7Æ—G2öâÀ¢6—¦VöV×G–æB6ÆÇ2WfW'—F†–ærVÇ6R&öw&ÒW'&÷"Â6òF†BÆ—7B†2FòÖ÷fRÀ¢v—F‚F†R†VFW" ¢“°¢òòv†BF†—27F–ÆÂFöW2æ÷B6VRÂ7FFVB&F†W"F†â–×Æ–VC¢à¢òòõdU$ÄôBÆVfW2F†R6WBVæ6†ævVBÂ6òFF–ærB÷‡6—¦U÷Bâ– ¢òòv÷VÆBæ÷Bf–Â†W&RWfVâF†÷Vv‚—Bv÷VÆBÖ¶P¢òòVWVU÷÷÷F¶W5öæõö&wVÖVçG6w&öæs²æBÖVÖ&W"–æ†W&—FV@¢òòg&öÒ&6R6Æ72—2æ÷B–âF†—2&öG’BÆÂâ&÷F‚&R÷WG6–FP¢òòv†BæÖR×6WB6ö×&—6öâ6âç7vW"à§Ð ¢òòòF†R6—‚6÷fW&w&÷W†öö²×G&–vvW"6†R&×2Âöbv†–6‚W†7FÇ’Etò&P¢òòò&V6†&ÆR(	BF†R÷F†W"f÷W"&RwV&FVB'’F†R'6W"Âv†–6‚F†P¢òòò6öFRw2÷vâFö26öÖÖVçG2Ç&VG’6–BæBF†—2FW7BÖV7W&W2à¢òòð¢òòòÂG&–vvW"Âv†ò&VgW6W2—BÀ¢òòòÂÒÒ×ÂÒÒ×À¢òòòÂ†G'bç7FW†â’÷7B–Âæö&öG’(	BF†R6öçG&öÂÀ¢òòòÂ‚†G'b’ç7FW†â’÷7B–Âæö&öG’(	BF†R&Vâ—2Vçw&VBÀ¢òòòÂ‡7FW†â’÷7B–ÂÆ÷vW&–æs¢6ÆÆVR—2æ÷Bf–VÆB66W72‡c¢öæÇ’öæ6R–ç7FçF–FVB’À¢òòòÂ‚†G'bç‚²’ç7FW†â’÷7B–ÂÆ÷vW&–æs¢&V6V—fW"—2æ÷BF‚‡c¢öæÇ’öæ6R–ç7FçF–FVB’À¢òòòÂ†G'bç7FW÷7B–ÂF†R%4U#¢&×W7B&RÖWF†öB6ÆÂ&Vf÷&R&V÷"÷7F"À¢òòòÂ†G'bç7FW†â²’÷7B–ÂF†R%4U#¢&&wVÖVçG2×W7B&R–FVçF–f–W'2"À¢òòð¢òòò&÷F‚&V6†&ÆR&×2&RVç7W÷'FVFÂæBF†Rf—'7BfW'6–öâö`¢òòòF†—2FW7B6–B–çfÆ–F&V6W6R—BÖV7W&VBöæÇ’F†R”å5DåD”DT@¢òòò÷6—F–öââcw2ÖF6†–ær&VgW6Â6öÖW2g&öÐ¢òòòVÖ—Eö6÷fW&w&÷Wö†ööµ÷6×ÆU÷&Vv—7G&F–öæÂv†–6‚'Vç2W"6÷b ¢òòò7FW6÷ff–VÆB(	B6÷fW&w&÷WFV6Æ&VBæBæWfW"–ç7FçF–FVBæWfW ¢òòò&V6†W2—BÂæBcVÖ—G2F†Rv†öÆRFW7F&Væ6‚âD"Ô•"&VgW6W2@¢òòòDT4Ä$D”ôâÂ6òF†RGvòF—6w&VRW†7FÇ’F†W&S ¢òòð¢òòòÂÂ6÷b¢7FW6÷f&W6VçBÂ6÷fW&w&÷WVæ–ç7FçF–FVBÀ¢òòòÂÒÒ×ÂÒÒ×ÂÒÒ×À¢òòòÂcÂ&VgW6W3¢&×W7B&W6öÇfRFò†öö¶&ÆV(
b"ÂVÖ—G2ÂæBr²²6ö×–ÆW2—BÀ¢òòòÂD"Ô•"Â&VgW6W2Â&VgW6W2À¢òòð¢òòò$æò&6¶VæB'Vç2—B–âå’6öæf–wW&F–öâ"—2v†B–çfÆ–F6Æ–×2À¢òòòæBâVæ–ç7FçF–FVB6÷fW&w&÷W—26öæf–wW&F–öââF†—&BF–ÖP¢òòòF†—2'VÆR†2&VVâ'&ö¶VâöâF†—2'&æ6‚ÂgFW"6öææV7FæBF†P¢òòò'V–ÇBÖ–â&VF–6FW2à¢òòð¢òòòF†Rf÷W"'6W"ÖwV&FVB&×27F’–çfÆ–F2–çf&–çBwV&G2(	@¢òòòF†W’6ææ÷BVÖ—BfÇ6RÒÖ6öFVvVâc7VvvW7F–öâg&öÒ÷6—F–öà¢òòòæ÷F†–ær&V6†W2ÂæB–böæRWfW"F–Bf—&RF†R&öw&Òv÷VÆB&P¢òòòÖÆf÷&ÖVBà¢5·FW7EÐ¦fâö6÷fW&w&÷Wö†ööµ÷G&–vvW%ö†5÷Gvõ÷&V6†&ÆU÷6†Uö&×2‚’°¢ÆWBf—‡GW&RÒf—‡GW&R‚&6÷fW&w&÷Wö†ööµ÷G&–vvW%÷FW7Bæ†&2"“°¢òòæ6†÷"öâF†RDT4Ä$D”ôâÂæ÷BF†R&&RG&–vvW#¢F†R6ÖRFW‡@¢òòV'2–â6öÖÖVçBV–v‡BÆ–æW2&÷fRÂæB&WÆ6Vâ‚ââÂ– ¢òòöâF†RG&–vvW"ÆöæRVF—FVBF†R6öÖÖVçBæBÆVgBF†R&öw&Ð¢òòÆ÷vW&–ær6ÆVæÇ’(	BF†R&ö&RÖV7W&–ærF†Rw&öærÆ–æRÂ–à¢òòÖ–æ–GW&Rà¢6öç7BDT4Ã¢g7G"Ò&6÷fW&w&÷W7FW6÷b†G'bç7FW†â’÷7B’#°¢76W'B†f—‡GW&Ræ6öçF–ç2„DT4Â’Â&f—‡GW&R6†R6†ævVB"“°¢ÆWBv—F‚ÒÇC¢g7G'Â°¢ÆWB÷WBÒf—‡GW&Rç&WÆ6Vâ„DT4ÂÂff÷&ÖB‚&6÷fW&w&÷W7FW6÷b‡·GÒ÷7B’"’Â“°¢76W'EöæR†÷WBÂf—‡GW&RÂ'F†RFV6Æ&F–öâ×W7B7GVÆÇ’6†ævR"“°¢÷W@¢Ó° ¢òò6öçG&öÇ3¢F†Rf—‡GW&Rw2÷vâG&–vvW"ÂæBF†R&VçF†W6—¦V@¢òò&V6V—fW"Â&÷F‚Æ÷vW"à¢Æ÷vW%÷7&2‚ff—‡GW&R’æW‡V7B‚'F†Rf—‡GW&Rw2G&–vvW"Æ÷vW'2"“°¢Æ÷vW%÷7&2‚gv—F‚‚"†G'b’ç7FW†â’"’’æW‡V7B‚&&VçF†W6—¦VB&V6V—fW"Æ÷vW'2"“° ¢òòF†RGvò&×2Æ÷vW&–ær7GVÆÇ’&V6†W2à¢f÷"‡G&–vvW"ÂvçB’–â°¢‚'7FW†â’"Â&ÆæÖSâ†&w2–v—F†÷WB&V6V—fW""’À¢‚"†G'bç‚²’ç7FW†â’"Â&†öö²G&–vvW"&V6V—fW""’À¢Ò°¢ÆWB7&2Òv—F‚‡G&–vvW"“°¢ÆWB×6rÒ76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚g7&2’çVçw&öW'"‚’“°¢76W'B†×6ræ6öçF–ç2‡vçB’Â'·G&–vvW'Ó¢¶×6wÒ"“° ¢òò–ç7FçF–FVBÂc&VgW6W2—BFöò(	Bv†–6‚—2v†BF†Rf—'7@¢òòfW'6–öâöbF†—2FW7BÖV7W&VBÂæBÆÂ—BÖV7W&VBà¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚g7&2’’æW‡V7EöW'"‚'c&VgW6W2F†R–ç7FçF–FVBf÷&Ò"“°¢76W'B€¢f÷&ÖB‚'·cÒ"’æ6öçF–ç2‚&×W7B&W6öÇfRFò†öö¶&ÆV"’À¢'·G&–vvW'Ó¢·cÒ ¢“° ¢òòTä”å5DåD”DTBÂcVÖ—G2F†Rv†öÆRFW7F&Væ6‚(	B6òF†P¢òò7VvvW7F–öâ—2†öæW7BæB–çfÆ–Fv2æ÷BâG&÷–ærF†P¢òò6÷ff–VÆBæB—G2&VFW'2—2F†Rv†öÆRF–ffW&Væ6Rà¢ÆWBVæ–ç7C¢7G&–ærÒ7&0¢æÆ–æW2‚¢æf–ÇFW"‡ÆÇÂÂæ6öçF–ç2‚&6÷b"’bbÂæ6öçF–ç2‚&6÷bâ"’¢æ6öÆÆV7C££ÅfV3Åóãâ‚¢æ¦ö–â‚%Æâ"“°¢76W'B€¢Væ–ç7Bæ6öçF–ç2‚&6÷b¢7FW6÷b"’À¢'·G&–vvW'Ó¢F†R–ç7FçF–F–öâ×W7B7GVÆÇ’&RvöæR ¢“°¢ÆWB÷WBÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‚gVæ–ç7B’¢çVçw&ö÷%öVÇ6R‡ÆWÂæ–2‚'·G&–vvW'Ó¢cVÖ—G2F†RVæ–ç7FçF–FVBf÷&Ó¢¶WÒ"’“°¢76W'B€¢÷WBæ6öçF–ç2‚&–çBÖ–â‚"’À¢'·G&–vvW'Ó¢cVÖ—G2v†öÆRFW7F&Væ6‚Âæ÷B7GV" ¢“°¢òòD"Ô•"7F–ÆÂ&VgW6W2—BÂv†–6‚—2F†RvF†—2&V6÷&G2à¢76W'E÷Vç7W÷'FVB‚fÆ÷vW%÷7&2‚gVæ–ç7B’çVçw&öW'"‚’“°¢Ð ¢òòF†Rf÷W"F†R%4U"6Æ–×2f—'7BÂ6òÆ÷vW&–æræWfW"6VW2F†VÒà¢òò–ææVB&V6W6RF†B—2F†Rv†öÆR&V6öâF†÷6R&×2&Rææ÷FFV@¢òò2–çf&–çBwV&G2&F†W"F†âÖV7W&VBà¢f÷"‡G&–vvW"ÂvçB’–â°¢‚&G'bç7FW"Â&×W7B&RÖWF†öB6ÆÂ&Vf÷&R&V÷"÷7F"’À¢‚&G'bç7FW†â²’"Â&&wVÖVçG2×W7B&R–FVçF–f–W'2"’À¢Ò°¢ÆWBW'"Ò'6U÷6÷W&6R‚gv—F‚‡G&–vvW"’’æW‡V7EöW'"‚'F†R'6W"&VgW6W2—B"“°¢ÆWB×6rÒf÷&ÖB‚'¶W'#£÷Ò"“°¢76W'B†×6ræ6öçF–ç2‡vçB’Â'·G&–vvW'Ó¢¶×6wÒ"“°¢Ð§Ð ¢òòòG&ç67F÷"gVæ7F–öæ—26ÆÆ&ÆR'WB—2æ÷B†öö²F&vWBà¢òòð¢òòòF†RFVF–6FVBG&ç67F÷"•"W6VBFòF—66&B†öö¶&ÆTÖWF†öC£¦—5ö†öö¶&ÆV ¢òòòv†Vâ—B'V–ÇBG&ç67F÷$ÖWF†öE66†VÖâF†BÖFRÆ–âgVæ7F–öâFV6Æ&V@¢òòò–âv†Vâ7F—fV–æF—7F–æwV—6†&ÆRg&öÒ&VÂ†öö¶&ÆV¢D"Ô•"66WFV@¢òòòF†R&RÖ†öö²&VÆ÷ræBVÖ—GFVB†öö²fâÖ÷WBÂv†–ÆRc6÷'&V7FÇ’&V¦V7FVB—Bà¢5·FW7EÐ¦fâ÷Æ–å÷G&ç67F÷%ögVæ7F–öåö—5öæ÷Eöö†ööµ÷F&vWB‚’°¢ÆWB7&2Ò"2'G&ç67F÷"G'`¢GWB¢F÷ ¢v†Vâ7F—fP¢gVæ7F–öâÆ–â†ã¢V–çCÃƒâ¢Æör†–æfòÂ'Æ–âG¶çÒ"¢VæBgVæ7F–öâÆ–à¢VæBv†Và¦VæBG&ç67F÷"G'` §FW7F&Væ6‚F ¢GWB¢F÷ ¢G'b¢G'b7F—fP¦VæBFW7F&Væ6‚F  ¦–×ÂBf÷"F ¢öâG'bçÆ–â&P¢Æör†–æfòÂ&×W7Bæ÷B&Vv—7FW""¢VæBöà ¢'Và¢G'bæGWBÒGW@¢G'bçÆ–âƒ¢VæB'Và¦VæB–×ÂB"3° ¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‡7&2’’æW‡V7EöW'"‚'c&VgW6W2†öö²öâgVæ7F–öæ"“°¢76W'B€¢f÷&ÖB‚'·cÒ"’æ6öçF–ç2‚&×W7B&W6öÇfRFò†öö¶&ÆV"’À¢'cW7F&Æ—6†W2F†RÆæwVvR6öçG&7C¢·cÒ ¢“° ¢ÆWBW'"ÒÆ÷vW%÷7&2‡7&2’æW‡V7EöW'"‚%D"Ô•"×W7BÇ6ò&VgW6R†öö²öâgVæ7F–öæ"“°¢76W'B€¢76W'Eö–çfÆ–B‚fW'"’æ6öçF–ç2‚&FöW2æ÷BæÖR†öö¶&ÆV"’À¢'F†RF–væ÷7F–2×W7BF—7F–æwV—6‚Æ–âgVæ7F–öâg&öÒ†öö¶&ÆS¢¶W''Ò ¢“°§Ð ¢òòòfÇVR×&V6÷&Bf–VÆB—2W'6—7FVçB†÷7B×6–FRG&ç67F÷"7FFRÂæ÷BWf–FVæ6P¢òòòF†Bâ÷WGWBÖWfVçBæÇ—6—26÷W&6R÷vç2EUB†æFÆRâF†R&V6÷&B×W7Bæ÷@¢òòòF—fW'BF†—26÷W&6Rv’g&öÒ6ö×÷6—FRÖ6ö×öæVçBÆ÷vW&–ærà¢5·FW7EÐ¦fâæÇ—6—5÷6÷W&6U÷v—F…÷&V6÷&E÷7FFUöÆ÷vW'5öæEöVÖ—G2‚’°¢ÆWB7&2Ò"2 §7G'V7B6×ÆP¢fÇVR¢V–çCÃƒâFVfVÇB ¦VæB7G'V7B6×ÆP §G&ç67F÷"&V6÷&E6÷W&6P¢ö'6W'fVB¢÷WBWfVçCÇV–çCÃƒãà¢7W'&VçB¢6×ÆP ¢v†Vâ7F—fP¢†öö¶&ÆRV&Æ—6‚‡c¢V–çCÃƒâ¢7W'&VçBçfÇVRÒ`¢VÖ—Bö'6W'fVB‡b¢VæBV&Æ—6€¢VæBv†Và¦VæBG&ç67F÷"&V6÷&E6÷W&6P §FW7B&V6÷&E6÷W&6UFW7@¢ÆWBGWB¢F÷ ¢ÆWB7&2¢&V6÷&E6÷W&6R7F—fP¢'Và¢7&2çV&Æ—6‚ƒr¢76W'B7&2æ7W'&VçBçfÇVRÓÒrVÇ6Rf–Â‚'&V6÷&B7FFRÆ÷7B"¢VæB'Và¦VæBFW7B&V6÷&E6÷W&6UFW7@¢"3° ¢ÆWB&örÒÆ÷vW%÷7&2‡7&2’æW‡V7B‚'&V6÷&B7FFR×W7B&W6W'fRæÇ—6—2×6÷W&6R&÷WF–ær"“°¢fW&–g“£§fW&–g•÷&öw&Ò‚g&ör’æW‡V7B‚'&V6÷&B×7FFRæÇ—6—26÷W&6RfW&–f–W2"“°¢F&—#£¦VÖ—B‚g&örÂfÖW&vVE÷7&2‡7&2’Âf7÷F#£¤VÖ—D÷G3£¦FVfVÇB‚’¢æW‡V7B‚'&V6÷&B×7FFRæÇ—6—26÷W&6RVÖ—G2"“°§Ð ¢òòò&V6÷&B7FFR—2f—'7BÖ6Æ72fÇVRÂæ÷BöæÇ’6öÆÆV7F–öâöb–æFWVæFVçFÇ¢òòòFG&W76&ÆRÆVfW2ââæÇ—6—26÷W&6R6âV&Æ—6‚—G27W'&VçB&V6÷&Böâ¢òòò&V6÷&B×–ÆöBWfVçBÂ2F†RÆVv7’&6¶VæBFöW2à¢5·FW7EÐ¦fâæÇ—6—5÷6÷W&6Uö6åöVÖ—E÷v†öÆU÷&V6÷&E÷7FFR‚’°¢ÆWB7&2Ò"2 §7G'V7B6×ÆP¢fÇVR¢V–çCÃƒâFVfVÇB ¦VæB7G'V7B6×ÆP §G&ç67F÷"&V6÷&E6÷W&6P¢ö'6W'fVB¢÷WBWfVçCÅ6×ÆSà¢7W'&VçB¢6×ÆP¢v†Vâ7F—fP¢†öö¶&ÆRV&Æ—6‚‡c¢V–çCÃƒâ¢7W'&VçBçfÇVRÒ`¢VÖ—Bö'6W'fVB†7W'&VçB¢VæBV&Æ—6€¢VæBv†Và¦VæBG&ç67F÷"&V6÷&E6÷W&6P §FW7B@¢ÆWBGWB¢F÷ ¢ÆWB7&2¢&V6÷&E6÷W&6R7F—fP¢'Và¢7&2çV&Æ—6‚ƒr¢VæB'Và¦VæBFW7B@¢"3° ¢VÖ—Eö7÷7&2‡7&2“°§Ð ¢òòò6ö×öæVçB×&÷WFVBG&ç67F÷"w2&V6÷&B7FFR&VÖ–ç2W‡FW&æÆÇ’w&—F&ÆRÀ¢òòòÖF6†–ærâ÷&F–æ'’Væ&÷VæBG&ç67F÷"&V6÷&Bf–VÆBæBF†RÆVv7’&6¶VæBà¢5·FW7EÐ¦fâæÇ—6—5÷6÷W&6U÷&V6÷&EöÆVeö—5÷w&—F&ÆUög&öÕ÷FW7E÷66÷R‚’°¢ÆWB7&2Ò"2 §7G'V7B6×ÆP¢fÇVR¢V–çCÃƒâFVfVÇB ¦VæB7G'V7B6×ÆP §G&ç67F÷"&V6÷&E6÷W&6P¢ö'6W'fVB¢÷WBWfVçCÇV–çCÃƒãà¢7W'&VçB¢6×ÆP¢v†Vâ7F—fP¢†öö¶&ÆRV&Æ—6‚‚¢VÖ—Bö'6W'fVB†7W'&VçBçfÇVR¢VæBV&Æ—6€¢VæBv†Và¦VæBG&ç67F÷"&V6÷&E6÷W&6P §FW7B@¢ÆWBGWB¢F÷ ¢ÆWB7&2¢&V6÷&E6÷W&6R7F—fP¢'Và¢7&2æ7W'&VçBçfÇVRÒ0¢VæB'Và¦VæBFW7B@¢"3° ¢VÖ—Eö7÷7&2‡7&2“°§Ð ¢òòòF†R6ÖRW‡FW&æÂw&—FR×W7B7W'f—fR7G'V7GW&Â6ö×öæVçBFƒ²&÷WF–æp¢òòòF‡&÷Vv‚âVçb6ææ÷BGW&âF†R&V6÷&Bf–VÆB–çFò7V"Ö6ö×öæVçB&V6V—fW"à¢5·FW7EÐ¦fâæW7FVEöæÇ—6—5÷6÷W&6U÷&V6÷&EöÆVeö—5÷w&—F&ÆUög&öÕ÷FW7E÷66÷R‚’°¢ÆWB7&2Ò"2 §7G'V7B6×ÆP¢fÇVR¢V–çCÃƒâFVfVÇB ¦VæB7G'V7B6×ÆP §G&ç67F÷"&V6÷&E6÷W&6P¢ö'6W'fVB¢÷WBWfVçCÇV–çCÃƒãà¢7W'&VçB¢6×ÆP¢v†Vâ7F—fP¢†öö¶&ÆRV&Æ—6‚‚¢VÖ—Bö'6W'fVB†7W'&VçBçfÇVR¢VæBV&Æ—6€¢VæBv†Và¦VæBG&ç67F÷"&V6÷&E6÷W&6P ¦Vçbw&W ¢7&2¢&V6÷&E6÷W&6R7F—fP¦VæBVçbw&W  §FW7B@¢ÆWBGWB¢F÷ ¢ÆWBVçb¢w&W ¢'Và¢Vçbç7&2æ7W'&VçBçfÇVRÒ@¢VæB'Và¦VæBFW7B@¢"3° ¢VÖ—Eö7÷7&2‡7&2“°§Ð ¢òòò6–væVFæW72föÆÆ÷w2F†RFW&Ö–æÂ&V6÷&BÆVbF‡&÷Vv‚6ö×öæVçBf–VÆBF‚à¢òòò÷F†W'v—6R6–çCÃƒâãâ6–ÆVçFÇ’&V6öÖW2Æöv–6Â&–v‡B6†–gB–âD$•"2²²à¢5·FW7EÐ¦fâ6ö×öæVçE÷&V6÷&E÷6–væVEöÆVe÷W6W5ö&—F†ÖWF–5÷&–v‡E÷6†–gB‚’°¢ÆWB7&2Ò"2 §7G'V7B6–væVE6×ÆP¢&–2¢6–çCÃƒâFVfVÇB ¦VæB7G'V7B6–væVE6×ÆP §G&ç67F÷"6†–gE6÷W&6P¢ö'6W'fVB¢÷WBWfVçCÇ6–çCÃƒãà¢7W'&VçB¢6–væVE6×ÆP¢6†–gFVB¢6–çCÃƒâFVfVÇB ¢v†Vâ7F—fP¢öâ7–6ÆW0¢7W'&VçBæ&–2ÒÓ€¢6†–gFVBÒ7W'&VçBæ&–2ãâ¢VæBöà¢VæBv†Và¦VæBG&ç67F÷"6†–gE6÷W&6P §FW7B@¢ÆWBGWB¢F÷ ¢ÆWB7&2¢6†–gE6÷W&6R7F—fP¢'Và¢v—B"7–6ÆW0¢76W'B7&2ç6†–gFVBÓÒÓBVÇ6Rf–Â‚'6–væVB6†–gB"¢VæB'Và¦VæBFW7B@¢"3° ¢ÆWB7ÒVÖ—Eö7÷7&2‡7&2“°¢76W'B€¢7æ6öçF–ç2‚"‚†–çCcE÷B’‡6VÆbæ7W'&VçBæ&–2’’ãâ"’À¢'6–væVB&V6÷&BÆVb×W7BvWBâ&—F†ÖWF–26†–gC¥Æç¶7Ò ¢“°§Ð ¢òòòæöâ×W&–öF–27–6ÆR×G&–vvW"&öF–W2÷vâF†R6ÖR6VÆb×&VÆF—fR6ö×öæVç@¢òòò7FFR2ÖWF†öG2æBW&–öF–2†æFÆW'2â6–væVFæW72×W7BF†W&Vf÷&R&W6öÇfP¢òòòF‡&÷Vv‚F†R7–6ÆRÖ†æFÆW"gVæ7F–öâ–B2vVÆÂà¢5·FW7EÐ¦fâ6ö×öæVçE÷&V6÷&E÷6–væVEöÆVeö–åö7–6ÆU÷G&–vvW%÷W6W5ö&—F†ÖWF–5÷&–v‡E÷6†–gB‚’°¢ÆWB7&2Ò"2 §7G'V7B6–væVE6×ÆP¢&–2¢6–çCÃƒâFVfVÇBÓ€¦VæB7G'V7B6–væVE6×ÆP §G&ç67F÷"6†–gE6÷W&6P¢ö'6W'fVB¢÷WBWfVçCÇ6–çCÃƒãà¢7W'&VçB¢6–væVE6×ÆP¢6†–gFVB¢6–çCÃƒâFVfVÇB ¢v†Vâ7F—fP¢öâ7W'&VçBæ&–2Â ¢6†–gFVBÒ7W'&VçBæ&–2ãâ¢VæBöà¢VæBv†Và¦VæBG&ç67F÷"6†–gE6÷W&6P §FW7B@¢ÆWBGWB¢F÷ ¢ÆWB7&2¢6†–gE6÷W&6R7F—fP¢'Và¢v—B"7–6ÆW0¢76W'B7&2ç6†–gFVBÓÒÓBVÇ6Rf–Â‚'6–væVB6†–gBÆ÷7B"¢VæB'Và¦VæBFW7B@¢"3° ¢ÆWB7ÒVÖ—Eö7÷7&2‡7&2“°¢76W'B€¢7æ6öçF–ç2‚"‚†–çCcE÷B’‡6VÆbæ7W'&VçBæ&–2’’ãâ"’À¢'6–væVB&V6÷&BÆVb×W7BvWBâ&—F†ÖWF–26†–gB–â7–6ÆR†æFÆW#¥Æç¶7Ò ¢“°§Ð ¢òòòF–væ÷7F–2×W7Bæ÷B&V6öÖÖVæBVÆVÖVçB×v—6RfV666W72v†–ÆR–æFW†V@¢òòò6ö×öæVçB×&V6÷&BÖVÖ&W'2&RF†V×6VÇfW27F–ÆÂ÷WG6–FRF†RD$•"7V'6WBà¢5·FW7EÐ¦fâ6ö×öæVçE÷&V6÷&E÷fV5öF–væ÷7F–5öFöW5öæ÷E÷&öÖ—6Uöå÷Vç7W÷'FVE÷v÷&¶&÷VæB‚’°¢ÆWBv†öÆRÒ"2 §7G'V7B–ææW ¢'—FW2¢fV3ÇV–çCÃƒâÂ#à¦VæB7G'V7B–ææW §7G'V7B÷WFW ¢–ææW"¢–ææW ¦VæB7G'V7B÷WFW  §G&ç67F÷"fV56÷W&6P¢ö'6W'fVB¢÷WBWfVçCÇV–çCÃƒãà¢7W'&VçB¢÷WFW ¢v†Vâ7F—fP¢†öö¶&ÆRF÷V6‚‚¢7W'&VçBæ–ææW"æ'—FW2Ò7W'&VçBæ–ææW"æ'—FW0¢VæBF÷V6€¢VæBv†Và¦VæBG&ç67F÷"fV56÷W&6P §FW7B@¢ÆWBGWB¢F÷ ¢ÆWB7&2¢fV56÷W&6R7F—fP¢'Và¢7&2çF÷V6‚‚¢VæB'Và¦VæBFW7B@¢"3°¢ÆWB–æFW†VBÒv†öÆRç&WÆ6R€¢&7W'&VçBæ–ææW"æ'—FW2Ò7W'&VçBæ–ææW"æ'—FW2"À¢&7W'&VçBæ–ææW"æ'—FW5³ÒÒ"À¢“° ¢ÆWBv†öÆUöW'"ÒÆ÷vW%÷7&2‡v†öÆR’æW‡V7EöW'"‚'v†öÆRfV27FFR—2÷WG6–FRF†—27V'6WB"“°¢–bÆWBW'"†–æFW†VEöW'"’ÒÆ÷vW%÷7&2‚f–æFW†VB’°¢76W'B€¢v†öÆUöW'"çFõ÷7G&–ær‚’æ6öçF–ç2‚&VÆVÖVçB×v—6R"’À¢&F–væ÷7F–2&V6öÖÖVæG2–æFW†VB66W72F†BÇ6òf–Ç2‡¶–æFW†VEöW''Ò“¢·v†öÆUöW''Ò ¢“°¢Ð§Ð ¢òòò†öö²×G&–vvW&VB6÷fW&w&÷W2ö&W’F†R6ÖR†öö¶&ÆV&÷fVææ6R'VÆR0¢òòòFW7B×66÷R&R÷÷7B†æFÆW'3²Æ–âG&ç67F÷"gVæ7F–öâ×W7Bæ÷B7V—&P¢òòòâ–×Æ–6—B6÷fW&vR7V'67&—F–öâÖW&VÇ’&V6W6R—B6†&W2F†RÖWF†öB•"à¢5·FW7EÐ¦fâ÷Æ–å÷G&ç67F÷%ögVæ7F–öåö—5öæ÷Eöö6÷fW&w&÷Wö†ööµ÷F&vWB‚’°¢ÆWB7&2Ò"2&6÷fW&w&÷W2†G'bçÆ–â†â’÷7B¢7¢6÷fW"GWBæ6÷VçEö÷W@¢&–ç0¢¦W&òÒ³Ð¢VæB&–ç0¦VæB6÷fW&w&÷W0 §G&ç67F÷"G'`¢GWB¢F÷ ¢v†Vâ7F—fP¢gVæ7F–öâÆ–â†ã¢V–çCÃƒâ¢Æör†–æfòÂ'Æ–âG¶çÒ"¢VæBgVæ7F–öâÆ–à¢VæBv†Và¦VæBG&ç67F÷"G'` §FW7F&Væ6‚F ¢GWB¢F÷ ¢G'b¢G'b7F—fP¢6÷b¢0¦VæBFW7F&Væ6‚F  ¦–×ÂBf÷"F ¢'Và¢G'bæGWBÒGW@¢G'bçÆ–âƒ¢VæB'Và¦VæB–×ÂB"3° ¢ÆWBcÒ7÷F#£¦VÖ—B‚fÖW&vVE÷7&2‡7&2’’æW‡V7EöW'"‚'c&VgW6W26÷fW"†öö²öâgVæ7F–öæ"“°¢76W'B€¢f÷&ÖB‚'·cÒ"’æ6öçF–ç2‚&×W7B&W6öÇfRFò†öö¶&ÆV"’À¢'cW7F&Æ—6†W2F†RÆæwVvR6öçG&7C¢·cÒ ¢“° ¢ÆWBW'"ÒÆ÷vW%÷7&2‡7&2’æW‡V7EöW'"‚%D"Ô•"×W7B&VgW6R6÷fW"†öö²öâgVæ7F–öæ"“°¢76W'B€¢76W'Eö–çfÆ–B‚fW'"’æ6öçF–ç2‚&FöW2æ÷BæÖR†öö¶&ÆV"’À¢'F†RF–væ÷7F–2×W7BF—7F–æwV—6‚Æ–âgVæ7F–öâg&öÒ†öö¶&ÆS¢¶W''Ò ¢“°§Ð