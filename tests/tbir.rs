//! TB-IR pipeline tests: AST → IR lowering snapshots, verifier checks,
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
/// needs for the constraint-IR / randomize seam — empty otherwise).
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
/// providing the bus decl by parsing `stdlib/<Bus>.arch` alongside it —
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
/// `--codegen v1` — that suggestion is only honest when v1 actually
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
/// precise message — and must NOT point the user at `--codegen v1`.
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
/// form: `Eq(x)` → `("eq", Some(x), None)`, a constant `Range` → its two
/// optional bounds, and any runtime bound → `u64::MAX` sentinel so a
/// stray non-constant fold is visible rather than silently dropped.
fn bin_repr(v: &ir::CovBinValue) -> (&'static str, Option<u64>, Option<u64>) {
    fn bound(b: &ir::CovBinBound) -> Option<u64> {
        match b {
            ir::CovBinBound::Const(x) => Some(*x),
            ir::CovBinBound::Runtime(_) => Some(u64::MAX),
        }
    }
    match v {
        ir::CovBinValue::Eq(x) => ("eq", Some(*x), None),
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
/// (A typed `let : uint<8>` assignment does NOT itself mask — the residue is
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

/// harc#473: the wrap width is `max(W(lhs), W(rhs))` — a literal is
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
/// hard lowering error — never a silent no-wrap (an un-masked scoreboard
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
/// lowering and substitute as that literal at use sites — the same value
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
/// const and the unknown reference — never silently accepted.
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
/// initializer, at `max(W(lhs), W(rhs))` per spec §2.4, whenever both
/// operand widths are statically known — a literal is self-sized, an
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
/// — the `const` table carries values, not declared types, so a
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
/// assigning one into an explicitly width-typed local must verify — this
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
/// even when `wide` is `uint<128>` — the guard must not reject it (it
/// did, by taking the max over `&` operands). The unmasked wide operand
/// must still reject, in BOTH directions — `<<` previously had no guard
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
/// range check — the mod-2^64 conversion C++20 defines and v1 performs.
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

/// #521: signed constants fold with the declared signedness — `>>` on a
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
    // value-transparent here — `(int64_t)((uint64_t)(-1))` is `-1`.
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

/// #521: the folded value must fit the declared width — out-of-range
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
    // S8_MIN is a signed const — its use site must emit the signed
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

/// #521: `as uint<W>` / `as sint<W>` relabel casts fold — the value is
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
    // BACK = (255 as sint<64>) >> 8 — the arithmetic shift of a
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
/// the RHS signedness (v1's `auto` → int64_t).
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
/// comparisons compute signed — not just `>>` (which the shift emitter's
/// forced `(int64_t)` cast already saved). A `bool` return stays
/// `uint64_t` (a 0/1 value — no signedness divergence); only a `sint`
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
    // `sint` param + `sint` return → int64_t.
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
/// members — `_tb` scalar fields, component/scoreboard scalar fields —
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
/// masked value again — so two independent 8-bit masks appear. This locks
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

/// `release` of a read-only probe is a hard error — only a force probe
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
/// unconditionally — `lower_src` skips `resolve_use_imports`, so every
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
/// harc-com#493) — existing fixtures with a dangling `use arc.stdlib.X`
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

// ── Emitted-C++ snapshots — the emission surface for the original
//    five fixtures of the equivalence matrix
//    (tests/tbir_equiv_fixtures.txt). Full files,
//    so any future emitter refactor diffs visibly here instead of
//    silently shifting shapes the marker tests don't cover. ──────────

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
/// → sync tick loops), and the DUT handle field pokes the test DUT.
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
    // Two `on bus.<ch>.handshake` observers → two monitor cycle handlers
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
    // transactor_active_test binds `active`, so it lowers — sanity that
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
/// where the event payload is `event<TinyTxn>` — a *transaction*
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
///      (`ComponentFieldKind::ScoreboardSub`) — accessed by the nested
///      run-scope path `top.sb.expected` (not `_tb.sb`);
///   2. `<env>.quiesced(N)` — expands to an AND of `idle(N)` over every
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
/// timed-out any-of means no predicate ever fired — v1 lists them all
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
/// `fail("…")` header) carry the single unguarded "none of:" line.
#[test]
fn wait_any_of_timeout_dump_ir_snapshot() {
    let prog = lower_src(&fixture("wait_any_of_timeout_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("wait_any_of_timeout_dump_ir", format!("{prog}"));
}

// ── lower_coroutine pass: CFG → tagged-FSM metadata. Snapshots lock
//    the `harc dump-ir --pass lower-coroutine` suffix (the metadata
//    section the pass appends after the regular IR dump, which the
//    *_dump_ir snapshots above already lock). ───────────────────────

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
/// seam — `docs/tbir-mvp.md` §"randomize"). A bare `randomize(t)` of a
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

// ── Transaction value-records (non-randomize usage) ─────────────────

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
    let err = lower_src(non_hookable).expect_err("must reject non-hookable sink");
    let msg = assert_unsupported(&err);
    assert!(msg.contains("not hookable"), "unexpected diagnostic: {msg}");

    let mismatched_payload = non_hookable.replace(
        "function accept(v: uint<8>)\n    end accept",
        "hookable accept(v: sint<8>)\n    end accept",
    );
    let err = lower_src(&mismatched_payload).expect_err("must reject incompatible payloads");
    let msg = assert_unsupported(&err);
    assert!(msg.contains("payload"), "unexpected diagnostic: {msg}");
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
/// instance — IR identical to the test-scope-let form (tbir emits every
/// component at run scope regardless of binding shape).
#[test]
fn tb_field_agent_dump_ir_snapshot() {
    let prog = lower_src(&fixture("tb_field_agent_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let dump = format!("{prog}");
    // The agent bound as a testbench field still lowers as a component,
    // and every access resolved to the BARE `tagger` instance (the `_tb`
    // prefix stripped) — not `_tb.tagger`, not a DUT port.
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
/// analysis-source shape — `out event<T>` port + a hookable method that
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
/// source. The `passive` case deliberately does NOT use this helper — it
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
/// ownership annotation on this path — it must NOT suppress the
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
    // skips in CI — so without this the stated CI guarantee has a hole.
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
/// Note this is not independent coverage of the passive case: `passive`
/// and no-mode lower byte-identically (the gate accepts both and records
/// neither), so this documents that a composite component needs no
/// transactor mode rather than exercising a distinct path.
#[test]
fn modeless_analysis_monitor_testbench_field_lowers() {
    let prog = lower_src(&passive_monitor_src("")).expect("a mode-less component field lowers");
    verify::verify_program(&prog).expect("verifies");
    assert!(format!("{prog}").contains("component c0 LifecycleMonitor (transactor)"));
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
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("`active` mode on composite-component"),
        "diagnostic should name the offending mode and construct: {msg}"
    );
    assert!(
        msg.contains("LargeTb.lifecycle"),
        "diagnostic should locate the offending field: {msg}"
    );
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
/// `HarcQueue<Struct>` — reusing the same record-queue seam as the
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
/// transaction — a `let s : S` default-constructs and `s.field`
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
    assert_eq!(prog.records.len(), 1, "struct → one record schema");
    let rec = &prog.records[0];
    assert_eq!(rec.name, "Pkt");
    assert_eq!(rec.fields.len(), 3, "fields not double-counted from body");
    assert_eq!(rec.fields[0].default, Some(1), "bool true → 1");
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
/// widthless element, a nested record, a list) is still rejected —
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
/// `Vec` with a widthless element — no defined packed width) is still
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
/// rejected precisely — never emitted as `Entry = <scalar>`.
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
/// STRUCTURALLY INVALID in every backend — the generated C++ struct is
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
/// (`outer[i].entries[j].tag` — a `Vec<Table, N>` whose element record
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
/// inside an impl-form body, desugared to `_tb.tbl…`): the chain resolver's
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
/// UB — v1 included, so the message must NOT suggest `--codegen v1`).
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
/// is the same infinitely-sized by-value cycle through the array member —
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

/// A DIAMOND (shared but acyclic) nesting — `Outer { a : Mid, b : Mid }`
/// where `Mid` is reached twice but the graph has no cycle — must NOT be
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
/// arg) must be REJECTED with a structured diagnostic — NOT lowered into
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
/// (`dst.data = 5`) must be REJECTED — NOT lowered into
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
    // Same site as the width-mismatch case above, which v1 compiles —
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
/// (different element width) must be rejected — the C++ `std::array`
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
/// element type and length) STILL lowers cleanly — the #443 feature is
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
/// temp — same no-inline-ports discipline as `Assign`.
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
/// compile failure; the IR rejects at lowering) — both on writes and
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
/// — v1's by-value C++ struct copy. A non-record initializer into a
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
    // a record-typed local — the generic record-local declare + struct
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

    // Untyped bare copy `let t3 = t1` — tbir types the dest from the
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
    // dest is annotated `Req` but the source is `Other`) — not deferred
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

/// `when` subtype blocks stay outside the lowered record shape.
#[test]
fn record_when_subtype_is_unsupported() {
    let src = r#"
transaction Req
    op : uint<2>
    when op == 1
        addr : uint<32>
    end when
end transaction Req

test WhenTest
    let dut : Top
    run
        wait 1 cycle
    end run
end test WhenTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(msg.contains("`when` subtype"), "names the construct: {msg}");
}

/// Record locals cannot live in *pure* helpers (they emit as
/// scalar-only file-scope C++ functions in the tbir backend). Note the
/// body must stay inside the pure scan subset to reach this gate — a
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

// ── Helper functions: pure C++ calls vs CFG inlining ────────────────

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
    // body — inlined waits take v1's synchronous lambda path) and
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
/// params — two calls must not share the `addr` slot.
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
/// inlining a cycle does not terminate. v1 has no answer here either —
/// it emits every helper as an `auto` lambda, which cannot name itself
/// in its own initializer — so the diagnostic must not point at v1.
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
/// ahead of every body, so it can call itself — direct or mutual. v1
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

    // Mutual recursion among pure helpers is the same story — every
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
/// at the call site — helpers are free functions.
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
/// call — and sync, not co_await, mirroring v1's lambda-body waits).
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

// ── `wait N cycles on <clock>` ──────────────────────────────────────

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
            wc.index = 7; // out of range — only 2 clocks declared
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
/// (`{[1..3], 7}`). (Open-high `[a..]` does not parse — the `..` infix
/// requires a right operand; only the bracket-prefix `[..b]`/`[..]`
/// forms produce open bounds.) Bounds match v1's hit test
/// (`_v >= lo && _v <= hi` — inclusive on both ends).
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
    // `sel_lo = [dut.en .. 7]` — runtime LOW bound (Port), const HIGH bound.
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
    // `sel_expr = [(dut.en + 4) .. 15]` — runtime LOW bound is a Binary
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
    assert!(
        err.to_string()
            .contains("width must be >= the source width"),
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
/// rejection shifted to the first construct still out of subset — the
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
/// reads — all shapes that must match v1's runtime-observable output.
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
/// `--mt` output is byte-identical to the cooperative default — `--mt`
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

// ── placement pass snapshots — tier/timing annotation per block plus
//    the capability-diagnostic surface under both built-in profiles. ─

/// top_counter under the default single-site profile: pin-driving
/// blocks anchored by WaitCycles classify cycle-exact / Tier 0; pure
/// logging blocks land in Tier 2. Diagnostics must be `none` — the
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

// ── Bus construct: bindings, protocol-typed signal access, channel
//    handshakes, and TLM method-call edges ───────────────────────────

/// Locks the dump-ir text for the Scope-A bus fixture: an inline bus
/// declaration, a `bind dut` binding on the testbench schema, and
/// two-level `<bind>.<ch>.<sig>` accesses lowering to flat-path
/// DutRead/DutWrite (`dut.axil.aw.valid` → `axil_aw_valid`).
#[test]
fn axilite_bus_dump_ir_snapshot() {
    let prog = lower_src(&fixture("axilite_bus_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("axilite_bus_dump_ir", format!("{prog}"));
}

/// Locks the dump-ir text for the blocking TLM fixture: the
/// `TransactorMethod` call edges survive lowering UNINLINED — each
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
/// divergence from v1's payload struct — equivalent for everything
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
/// — rejected precisely at the end of the function rather than
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
/// fork before one `join_all` is rejected — the two routing strategies
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
/// mode, naming the mode and the call site — and NOT by pointing at v1,
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

// ── Transactor declarations + method call edges ─────────────────────

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
    assert_eq!((pulse.n_params, pulse.has_ret), (1, false));
    assert_eq!((readv.n_params, readv.has_ret), (0, true));

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
/// still lower cleanly — the cycle guard must not over-reject legitimate
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
/// `cam_value_basic` snapshot only locks the `auto`→`std::function`
/// predeclaration in isolation (no actual sibling call); this asserts
/// the generated code for (a) a FORWARD sibling call (`first()` calls
/// `second()`, declared later) and (b) a VALUE-returning sibling call
/// (`use_readv()` calls `readv() -> uint<32>`). The forward case only
/// compiles because every method is predeclared as a `std::function`
/// slot BEFORE any lambda is assigned — assert that ordering directly.
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

/// Named arguments in a sibling call are out of the TB-IR subset and
/// rejected precisely at lowering.
#[test]
fn transactor_sibling_call_named_argument_is_rejected() {
    let src = r#"
transactor Xt
    dut : Top

    when active
        hookable inner(n: uint<8>)
            dut.x = n
            wait 1 cycle
        end inner

        hookable outer()
            inner(n = 5)
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
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("named arguments in transactor sibling method call"),
        "diagnostic should name the named-argument rejection: {msg}"
    );
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
/// lambdas with synchronous waits (`for (...) tick();` — v1's hookable
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
/// substituted in the expression — the seam rule's sanctioned home (the
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
    assert_eq!(m.n_params, 1);
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
/// (its passive surface — persistent state + always-on handlers — is
/// lowered; #494 P0a/P1b), but calling one of its `when active` methods
/// on the passive instance is rejected at the call site as `Invalid`
/// (the method structurally does not exist there). A mode-less field has
/// nothing to inherit from at testbench scope, so it stays rejected.
#[test]
fn transactor_instance_mode_rules() {
    // `XACTOR_SRC` calls `xt.pulse(...)` / `xt.readv()` — both `when
    // active` methods — so a passive `xt` is rejected at the CALL site.
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
    let msg = assert_unsupported(&lower_src(&modeless).unwrap_err());
    assert!(msg.contains("without an `active`/`passive` mode"), "{msg}");
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

/// Unknown methods and arity mismatches are hard lowering errors —
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
/// SYNCHRONOUS shape (spec §7.4's "synchronous context"): a method body
/// has no scheduler to defer to, so the budget is read once and the
/// predicate polled with `tick()` per cycle — not the coroutine
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
    // The coroutine awaiter must NOT appear in the method body — a
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
/// `remap` table records the `(channel, signal) → port` override so the
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

/// A remap path must be exactly `<channel>.<signal>` (2 segments) —
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
/// flat name (`aw.valid` → `s_axi_awvalid`); an UNMAPPED channel signal
/// keeps the `<bind>_<ch>_<sig>` convention, and the already-flattened
/// `<ch>_<sig>` form is never remapped — mirroring v1's
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
    // single-segment path — dump-ir renders it verbatim).
    assert!(dump.contains("DutWrite(dut.s_axi_awaddr, 7)"), "{dump}");
    assert!(dump.contains("DutWrite(dut.s_axi_awvalid, 1)"), "{dump}");
    // Unmapped channel signal (`ready`) keeps the canonical 3-segment
    // path (dump-ir renders segments dotted; the backend joins with `_`
    // → `s_axi_aw_ready`).
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
/// persistent state — a `queue<Record>` and a `queue<scalar>`. The state
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
/// the scoreboard/component queue machinery verbatim — the per-instance
/// state struct carries `harc_rt::HarcQueue<Beat>` / `HarcQueue<uint64_t>`
/// members, and push/pop/size/empty emit the same calls scoreboards do.
#[test]
fn target_nonscalar_queue_state_emits_harcqueue() {
    let cpp = emit_fixture_cpp("target_nonscalar_state_test.harc");
    // Per-instance state struct members are HarcQueue<T> — record element
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
/// subfield read/write emit `<instance>.<field>.<sub>` — no `HarcQueue`,
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
/// passive instance per bound transactor — lowering rejects the second
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
/// lowering: the binding line carries the sorted `(channel, signal) →
/// port` table. The fixture binds with name `m`, so the
/// `<field>_<channel>_<signal>` convention would produce `m_read_*` —
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
/// `dut->mem_read_*` / `dut->mem_poke_*` — the `m_read_*` convention
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

    // 1. Nested in an expression → seam violation.
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

    // 2. Unresolved binding → seam violation.
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

    // 3. Arity drift against the schema → seam violation.
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

    // 4. Call edge in a non-Run/Check function → seam violation
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
/// timing-tolerant — the boundary the lowering now actually produces.
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
/// lower (state-field slice) — asserted positively here.
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
    assert!(msg.contains("event/directional field `req`"), "{msg}");

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
/// a `BadTransactorCall` — lowering never produces it, so reaching it
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
/// host state — `dut.x` through it must NOT silently lower to a DUT
/// access (v1 surfaces the shadowing as a C++ compile error; the IR
/// rejects at lowering).
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
    let err = lower_src(src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("assignment to a non-port, non-local target"),
        "shadowed name must not resolve to the DUT: {msg}"
    );
}

// ── Singleton-blocker batch (ternary, time/wide literals, const/enum,
//    test-scope lets, indexed lanes, testbench methods/fields, width
//    methods): one dump-ir snapshot per newly-registered fixture. ────

/// Ternary expressions inside CFG-inlined impure helpers, plus the
/// `WaitCyclesSync` terminator (v1's synchronous helper-lambda waits).
#[test]
fn linklist_basic_dump_ir_snapshot() {
    let prog = lower_src(&fixture("linklist_basic_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("linklist_basic_dump_ir", format!("{prog}"));
}

/// Wall-clock waits (`wait 80ns` → `WaitTimePs`) and the `debug` log
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
/// lowering records `None` — feeding `0` into the sext shift-fill would
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
/// zero width is a usable operand width (`max(0, 1)` → a 1-bit mask), not
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

// ── regblock construct (register-level frontdoor subset) ─────────────

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
/// write — v1's bit-slice insert. The mirror cell stays whole-register
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
    // bus write carries the updated whole-register word — never the
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
    // The WO read must NOT issue a bus read — it serves from the mirror
    // cell (a shifted RecordField extract), so no RegRead is emitted.
    assert!(
        !dump.contains("RegRead"),
        "WO field read must serve from the mirror, not the bus: {dump}"
    );
}

/// A register read outside `let`-RHS position (here an assert condition)
/// now lowers to an `Expr::RegRead` — v1's inline assignment-expression
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

/// The corpus `regblock_basic_test` fixture — initiator-side BFM `via`
/// helper PLUS register reads in assert conditions and `${...}` format
/// args (`assert (regs.DMACR & 1) == 1 else fail("...0x${regs.DMACR}")`)
/// — now FULLY lowers (this slice). Register reads outside `let`-RHS
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
        "expected ≥3 assert-cond RegReads, got {cond_reads}"
    );
    assert!(
        fail_arg_reads >= 3,
        "expected ≥3 fail-message RegReads, got {fail_arg_reads}"
    );
}

/// The corpus `regblock_access_test` fixture — same initiator-side BFM
/// `via` helper, but every register read sits in `let`-RHS position
/// (`let v = regs.MM2S_LEN`) — FULLY lowers with this slice: the BFM
/// helper's `hookable write/read` bodies drive the bound AXI-Lite bus
/// channels and the regblock frontdoor's `Helper.write`/`read` call
/// edges resolve. (The end-to-end v1↔tbir trace equivalence is gated by
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
/// emission materializes one per-instance state struct (shared with the
/// bus-driving method lambdas). End-to-end v1↔tbir trace equivalence is
/// gated by the registry harness; this asserts lowering structure.
#[test]
fn bound_initiator_transactor_with_state_lowers() {
    let prog = lower_with_stdlib_bus(
        "transactor_bound_initiator_state_test.harc",
        "BusAxiLite.arch",
    )
    .expect("lowers");
    verify::verify_program(&prog).expect("verifies");

    let helper = prog
        .transactors
        .iter()
        .find(|x| x.name == "AxilHelper")
        .expect("AxilHelper transactor lowered");
    assert_eq!(helper.bound_bus.as_deref(), Some("BusAxiLite"));
    let names: Vec<&str> = helper
        .state_fields
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["last_read", "read_count"],
        "state fields on schema"
    );

    // The stateful bound-initiator instance is recorded for per-instance
    // state materialization (the same table the unbound form uses).
    assert_eq!(
        prog.testbenches[0].unbound_state_actors,
        vec![("helper".to_string(), ir::TransactorId(0))],
        "stateful bound-initiator instance recorded for materialization",
    );

    // The `read` body's state writes are instance-filled with `helper`
    // (the bound instance name), not the empty pre-bind placeholder.
    let read_fn = prog
        .functions
        .iter()
        .find(|f| f.name == "AxilHelper_read")
        .expect("read method function");
    let state_writes = read_fn
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter(|s| matches!(s, ir::Stmt::TransactorStateWrite { instance, .. } if instance == "helper"))
        .count();
    assert_eq!(
        state_writes, 2,
        "two instance-filled state writes in read body"
    );
}

/// The corpus `regblock_bitbash_test` fixture — `bitbash(regs)` over a
/// regblock with 3 RW + 1 RO + 1 WO register — FULLY lowers (this
/// slice). The walk unrolls to write/read both patterns + compare per
/// RW register; RO/WO are skipped. The trailing `assert errors == 0`
/// lowers via the new `Expr::ErrorCount` framework value.
#[test]
fn regblock_bitbash_corpus_lowers() {
    let prog =
        lower_with_stdlib_bus("regblock_bitbash_test.harc", "BusAxiLite.arch").expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let run = prog
        .functions
        .iter()
        .find(|f| matches!(f.kind, ir::FunctionKind::Run))
        .expect("run function");
    // 3 RW registers × 2 patterns = 6 write+read pairs. Count the
    // discarded `Helper.write(...)` call edges (dest=None) the bitbash
    // walk emits (RO/WO skipped → exactly 6).
    let writes = run
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter(|s| matches!(s, ir::Stmt::TransactorCall { dest: None, .. }))
        .count();
    assert_eq!(
        writes, 6,
        "expected 6 bitbash write call edges (3 RW × 2 patterns)"
    );
    // The `errors == 0` check resolves the framework counter.
    let has_errcount = run.blocks.iter().any(|b| {
        b.stmts.iter().any(|s| {
            if let ir::Stmt::AssertCheck { cond, .. } = s {
                fn has(e: &ir::Expr) -> bool {
                    match e {
                        ir::Expr::ErrorCount => true,
                        ir::Expr::Binary(_, a, b) => has(a) || has(b),
                        _ => false,
                    }
                }
                has(cond)
            } else {
                false
            }
        })
    });
    assert!(
        has_errcount,
        "expected an `errors == 0` AssertCheck (ErrorCount)"
    );
}

/// The corpus `regblock_record_test` fixture carries a per-register
/// `on regs.REG` write callback. It now LOWERS via host-state promotion:
/// the callback is a `FunctionKind::TestHook` function back-patched onto
/// the binding, and a `record_write` targeting a callback-bearing binding
/// lowers to `Stmt::RecordWriteCb` (mirror update + recursion-depth guard
/// + callback dispatch) instead of a plain mirror write.
#[test]
fn regblock_record_corpus_lowers_with_callback() {
    let prog = lower_with_stdlib_bus("regblock_record_test.harc", "BusAxiLite.arch")
        .expect("regblock record callback lowers");
    let dump = format!("{prog}");
    // The MM2S_SA write routes through RecordWriteCb with the callback fn.
    assert!(
        dump.contains("RecordWriteCb(%regs.MM2S_SA") && dump.contains("cb=fn"),
        "expected a RecordWriteCb dispatching the MM2S_SA callback: {dump}"
    );
    // The binding records the callback in its schema.
    assert!(
        dump.contains("[on MM2S_SA=fn"),
        "expected the binding to carry the MM2S_SA callback: {dump}"
    );
    // The callback body itself writes MM2S_LEN via record_write (still a
    // RecordWriteCb because the binding is callback-bearing, though that
    // register carries no own callback).
    assert!(
        dump.contains("TestHook") && dump.contains("RecordWriteCb(%regs.MM2S_LEN"),
        "expected the callback body's mirror write into MM2S_LEN: {dump}"
    );
}

/// The passive `record_write`/`record_read` API — WITHOUT a per-register
/// callback — fully lowers: a constant-address `record_write` decodes to
/// a masked mirror `RecordFieldWrite` and `record_read` to a mirror
/// `RecordField` read, with no bus traffic and no callback dispatch.
#[test]
fn regblock_record_api_lowers() {
    let prog = lower_with_stdlib_bus("regblock_record_api_test.harc", "BusAxiLite.arch")
        .expect("passive record API lowers");
    let dump = format!("{prog}");
    // Masked mirror write from `record_write(0x18, 0x12345678)` and a
    // mirror read feeding the `record_read` dest.
    assert!(
        dump.contains("RecordFieldWrite(%regs.MM2S_SA"),
        "expected a mirror RecordFieldWrite from record_write: {dump}"
    );
    assert!(
        dump.contains("& 4294967295"),
        "expected the record_write value masked to the register width: {dump}"
    );
}

/// The corpus `regblock_fields_test` fixture — field-level decomposition
/// (`regs.DMACR.RS` / `regs.DMACR.MODE`) coexisting with whole-register
/// access (`regs.MM2S_SA`) — FULLY lowers. Closes the field-level half of
/// divergence 12.
#[test]
fn regblock_fields_corpus_lowers() {
    let prog =
        lower_with_stdlib_bus("regblock_fields_test.harc", "BusAxiLite.arch").expect("lowers");
    let dump = format!("{prog}");
    // Masked RMW on the whole-register mirror `DMACR`, plus an inline
    // RegRead for the bus-reading field extracts.
    assert!(
        dump.contains("DMACR ="),
        "expected mirror RMW of DMACR: {dump}"
    );
    assert!(
        dump.contains("RegRead"),
        "expected an inline field RegRead: {dump}"
    );
}

/// The corpus `regblock_addrmap_test` fixture — two `DmaChan` instances
/// at distinct bases composed by an `addrmap`, accessed 3-level
/// (`chip.inst.REG`) and 4-level (`chip.inst.REG.FIELD`) — FULLY lowers.
/// Closes the addrmap half of divergence 12.
#[test]
fn regblock_addrmap_corpus_lowers() {
    let prog =
        lower_with_stdlib_bus("regblock_addrmap_test.harc", "BusAxiLite.arch").expect("lowers");
    let dump = format!("{prog}");
    // Two distinct per-instance mirror locals (mangled), not one.
    assert!(
        dump.contains("__addrmap_chip_mm2s") && dump.contains("__addrmap_chip_s2mm"),
        "expected two per-instance addrmap mirror locals: {dump}"
    );
}

/// The corpus `regblock_alias_test` fixture — `instance mm2s_view :
/// DmaChan @ 0x30 alias of mm2s` shares the primary's mirror cell while
/// keeping its own bus base — FULLY lowers. Closes the `alias of` half
/// of the addrmap residual.
#[test]
fn regblock_alias_corpus_lowers() {
    let prog =
        lower_with_stdlib_bus("regblock_alias_test.harc", "BusAxiLite.arch").expect("lowers");
    let dump = format!("{prog}");
    // The alias instance shares the primary's mirror — only ONE mirror
    // local is declared (no `__addrmap_chip_mm2s_view`).
    assert!(
        dump.contains("__addrmap_chip_mm2s"),
        "expected the primary mirror local: {dump}"
    );
    assert!(
        !dump.contains("__addrmap_chip_mm2s_view"),
        "alias must NOT get its own mirror local (shares the target's cell): {dump}"
    );
}

// ── singletons batch 2: relation / property / extern fn / transactor ──

/// `relation_inlining_test` — free-standing `relation` declarations are
/// inert at the file gate; a `randomize(r) with BoundedAndHigh(r)` call
/// inlines all three relations' constraints in the typed solver backend
/// (block + alias + alias-of-relations forms). The IR records only the
/// `Randomize` terminator with a `ConstraintRef`; the relation decls
/// themselves carry no IR shape. Snapshotted end-to-end.
#[test]
fn relation_inlining_dump_ir_snapshot() {
    let prog = lower_src(&fixture("relation_inlining_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("relation_inlining_dump_ir", format!("{prog}"));
}

/// `pipe_reg_test` — an SVA-style `property pipe_depth_3 ... end
/// property` declaration is accepted at the file gate but inert: it is
/// never referenced via `assert property` (which the test-body lowering
/// still rejects), so it contributes no IR. The run body is plain
/// wait/assert against the DUT. Locks that the property declaration is
/// observably a no-op.
#[test]
fn pipe_reg_dump_ir_snapshot() {
    let prog = lower_src(&fixture("pipe_reg_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("pipe_reg_dump_ir", format!("{prog}"));
}

/// `extern_fn_ref_test` — an `extern function ref_crc8_step(...) -> ...`
/// (spec §9) call lowers to `CallTarget::ExternFn` (raw symbol name,
/// emitted `extern:ref_crc8_step(...)` in the dump), distinct from the
/// plain HARC helper `harc_crc8_step` call beside it. The forward
/// declaration is emitted file-scope at codegen; the decl is inert at
/// the gate.
#[test]
fn extern_fn_ref_dump_ir_snapshot() {
    let prog = lower_src(&fixture("extern_fn_ref_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("extern_fn_ref_dump_ir", format!("{prog}"));
}

/// `transactor_parse_test` — the spec §8.1 transactor surface (two
/// `on bus.<ch>.handshake` monitor bodies each doing `let c :
/// Completion` + `emit`, plus a `when active` `on req` body) lowers and
/// emits cleanly. Locks the component schema + on-handler/method-body
/// `RecordInit` shapes that the record-table-visibility codegen fix
/// unblocked.
#[test]
fn transactor_parse_dump_ir_snapshot() {
    let prog =
        lower_with_stdlib_bus("transactor_parse_test.harc", "BusAxiLite.arch").expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("transactor_parse_dump_ir", format!("{prog}"));
}

/// `scoreboard_typed_queue_test` — a method-bearing `scoreboard` with a
/// `queue<CheckerError>` (record element) plus an `on 1 cycles phase
/// post_eval` periodic handler on a checker transactor. Locks: the
/// `queue<Record>` component field, the `ComponentQueuePush`/`Pop`/
/// `QueueQuery` ops, the self-relative `sb.record_error(...)`
/// sub-component method call, the `checker.sb = sb` whole-value copy, and
/// the periodic handler carrying `phase post_eval`.
#[test]
fn scoreboard_typed_queue_dump_ir_snapshot() {
    let prog = lower_src(&fixture("scoreboard_typed_queue_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("scoreboard_typed_queue_dump_ir", format!("{prog}"));
}

/// `post_eval_provider_test` — the composite cluster: a function-library
/// transactor (`ProtocolModel`: a pure `predict_read(addr) ->
/// ReadResponse` method, no DUT/event/`on`) held as a sub-field and a
/// testbench field, a component-typed method parameter (`observe(addr,
/// model: ProtocolModel)`) dispatched on (`model.predict_read(addr)`), a
/// component passed by value as a method arg (`sb.observe(addr, model)`),
/// a record-returning method bound by `let r : ReadResponse =
/// model.predict_read(...)`, and an `on 1 cycles phase post_eval` handler.
/// Locks: `IrType::Component` params, `ComponentBase::Local` dispatch,
/// `Expr::ComponentValue` args, record-typed `ComponentCall` dest.
#[test]
fn post_eval_provider_dump_ir_snapshot() {
    let prog = lower_src(&fixture("post_eval_provider_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("post_eval_provider_dump_ir", format!("{prog}"));
}

/// Issue #485: a `testbench`-scoped `on <N> cycles phase post_eval`
/// periodic handler must lower under TB-IR (it was rejected before, with
/// "TB-IR lowering does not support testbench item ... yet"). The body
/// lowers to a flow-owned `TestHook` function registered as a periodic
/// service on the testbench schema; the backend fires it once per cycle
/// in the post_eval phase. Locks: the `periodic_services` schema entry
/// (period 1, phase post_eval), and that the handler body — which touches
/// testbench component instances directly and via a testbench helper
/// method — lowers without the `--codegen v1` workaround.
#[test]
fn testbench_periodic_post_eval_dump_ir_snapshot() {
    let prog = lower_src(&fixture("testbench_periodic_post_eval_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let tb = &prog.testbenches[0];
    assert_eq!(
        tb.periodic_services.len(),
        1,
        "the testbench-scoped periodic handler registers one service"
    );
    let svc = &tb.periodic_services[0];
    assert_eq!(svc.period, 1, "`on 1 cycles` → period 1");
    assert_eq!(svc.phase, ir::HandlerPhase::PostEval, "`phase post_eval`");
    // The service body is a flow-owned zero-param TestHook function.
    let body = prog.function(svc.function);
    assert!(matches!(body.kind, ir::FunctionKind::TestHook));
    assert_eq!(body.owner, Some(ir::TestbenchId(0)));
    assert!(body.params.is_empty(), "the handler body takes no params");
    insta::assert_snapshot!("testbench_periodic_post_eval_dump_ir", format!("{prog}"));
}

/// Issue #485: the emitted C++ registers the testbench periodic handler as
/// a `_post_eval_services` closure, gated on a per-service last-fire stamp
/// (`cycle_count - last >= period`), firing the handler's free lambda. The
/// body lambda is emitted AFTER the composite scoreboard instances +
/// method lambdas it references (an ordering the early test-hook loop
/// would otherwise get wrong).
#[test]
fn testbench_periodic_post_eval_emitted_cpp_snapshot() {
    let cpp = emit_fixture_cpp("testbench_periodic_post_eval_test.harc");
    assert!(
        cpp.contains("_post_eval_services.push_back"),
        "registers a post_eval service"
    );
    assert!(
        cpp.contains("_tbper_"),
        "uses the per-service last-fire stamp tag"
    );
    insta::assert_snapshot!("testbench_periodic_post_eval_emitted_cpp", cpp);
}

/// Issue #494 P2b: a testbench-scoped `on <bool-expr>` cycle-trigger
/// handler (a NON-periodic on-handler) now lowers to a flow-owned
/// `TestHook` function plus a `TbCycleServiceSchema` carrying the trigger
/// predicate + edge mode. Mirrors the periodic-service lowering, but the
/// predicate is re-evaluated every cycle in the backend's registration
/// closure. (Previously this form was rejected — issue #485 only lowered
/// the periodic `on <N> cycles` form.)
#[test]
fn testbench_cycle_trigger_on_handler_lowers() {
    let src = r#"testbench TbCyc
    dut : Top
    hit : uint<32> default 0
    on dut.count_out == 7
        hit = hit + 1
    end on
end testbench TbCyc

impl TbCycTest for TbCyc
    run
        dut.rst = 1
        wait 3 cycles
        log(info, "done")
    end run
end impl TbCycTest"#;
    let prog = lower_src(src).expect("a testbench cycle-trigger on-handler must lower");
    let tb = &prog.testbenches[0];
    assert_eq!(
        tb.cycle_services.len(),
        1,
        "one cycle-trigger service is recorded on the testbench schema"
    );
    let svc = &tb.cycle_services[0];
    assert_eq!(
        svc.edge,
        harc::ir::CycleEdge::Rising,
        "an `on <bool-expr>` handler defaults to the rising edge"
    );
    assert_eq!(
        svc.phase,
        harc::ir::HandlerPhase::Checker,
        "default (no `phase post_eval`) is the Checker phase"
    );
    // The trigger predicate lowered into the service (not appended to the
    // body function), reading the DUT port.
    assert!(
        matches!(
            &svc.trigger,
            harc::ir::Expr::Binary(harc::ir::BinOp::Eq, ..)
        ),
        "the trigger is the lowered `dut.count_out == 7` predicate: {:?}",
        svc.trigger
    );
    let body = prog.function(svc.function);
    assert!(
        matches!(body.kind, harc::ir::FunctionKind::TestHook),
        "the handler body is a flow-owned TestHook function"
    );
}

/// Issue #494 P2b: the emitted C++ for a testbench-scoped `on <bool-expr>`
/// handler registers a per-cycle `_checkers` closure that re-evaluates the
/// predicate and fires the body on the rising edge (matching v1's
/// `emit_cycle_trigger`).
#[test]
fn testbench_cycle_trigger_on_handler_emitted_cpp() {
    let cpp = emit_fixture_cpp("tb_on_expr_test.harc");
    assert!(
        cpp.contains("_checkers.push_back"),
        "registers a per-cycle checker closure"
    );
    assert!(
        cpp.contains("_tbcyc_"),
        "uses the per-service cycle-trigger prev-state tag"
    );
    assert!(
        cpp.contains("_prev") && cpp.contains("_curr"),
        "renders the rising-edge prev/curr gate"
    );
}
/// A value-returning component method call can assign into an existing
/// record-typed local (`t = sqr.make(...)`). The `let t : Txn = ...`
/// shape was already lowered as `Stmt::ComponentCall { dest: Some(t) }`;
/// this locks the assignment form used by reusable sequencer APIs.
#[test]
fn component_record_method_assignment_to_existing_local() {
    let src = r#"
transaction Txn
    value : uint<8> default 0
end transaction Txn

sequencer TxnSource
    hookable make(v: uint<8>) -> Txn
        let t : Txn
        t.value = v
        return t
    end make
end sequencer TxnSource

testbench Tb
    dut : Top
    sqr : TxnSource
end testbench Tb

impl ComponentRecordAssignTest for Tb
    run
        let t : Txn
        t = sqr.make(7)
        assert t.value == 7 else fail("value=${t.value}")
    end run
end impl ComponentRecordAssignTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");

    let run = prog.function(prog.tests[0].run);
    let record_local = run
        .locals
        .iter()
        .position(|l| l.name == "t")
        .map(|idx| ir::LocalId(idx as u32))
        .expect("record local t");
    let saw_component_assign = run.blocks.iter().flat_map(|b| &b.stmts).any(|s| {
        matches!(
            s,
            ir::Stmt::ComponentCall {
                method,
                dest: Some(dest),
                ..
            } if method == "make" && *dest == record_local
        )
    });
    assert!(
        saw_component_assign,
        "assignment form should lower to ComponentCall dest=t:\n{run}"
    );
}

#[test]
fn component_method_assignment_allows_scalar_widening() {
    let src = r#"
sequencer ScalarSource
    hookable tiny() -> uint<8>
        return 7
    end tiny
end sequencer ScalarSource

testbench Tb
    dut : Top
    sqr : ScalarSource
end testbench Tb

impl ComponentScalarWidenAssignTest for Tb
    run
        let w : uint<16>
        w = sqr.tiny()
        assert w == 7 else fail("value=${w}")
    end run
end impl ComponentScalarWidenAssignTest
"#;
    let prog = lower_src(src).expect("scalar-compatible method assignment lowers");
    verify::verify_program(&prog).expect("verifies");
}

#[test]
fn component_method_typed_let_allows_scalar_widening() {
    let src = r#"
sequencer ScalarSource
    hookable tiny() -> uint<8>
        return 7
    end tiny
end sequencer ScalarSource

testbench Tb
    dut : Top
    sqr : ScalarSource
end testbench Tb

impl ComponentScalarWidenLetTest for Tb
    run
        let w : uint<16> = sqr.tiny()
        assert w == 7 else fail("value=${w}")
    end run
end impl ComponentScalarWidenLetTest
"#;
    let prog = lower_src(src).expect("scalar-compatible method initializer lowers");
    verify::verify_program(&prog).expect("verifies");
}

#[test]
fn component_method_typed_let_rejects_scalar_narrowing() {
    let src = r#"
sequencer ScalarSource
    hookable wide() -> uint<16>
        return 257
    end wide
end sequencer ScalarSource

testbench Tb
    dut : Top
    sqr : ScalarSource
end testbench Tb

impl ComponentScalarNarrowLetTest for Tb
    run
        let w : uint<8> = sqr.wide()
    end run
end impl ComponentScalarNarrowLetTest
"#;
    let err = lower_src(src).expect_err("narrowing method initializer must be rejected");
    let msg = assert_unsupported(&err);
    assert!(msg.contains("incompatible type"), "{msg}");
}

#[test]
fn component_method_typed_let_rejects_record_to_scalar_local() {
    let src = r#"
transaction Txn
    value : uint<8> default 0
end transaction Txn

sequencer TxnSource
    hookable make() -> Txn
        let t : Txn
        t.value = 7
        return t
    end make
end sequencer TxnSource

testbench Tb
    dut : Top
    sqr : TxnSource
end testbench Tb

impl ComponentRecordToScalarLetTest for Tb
    run
        let w : uint<16> = sqr.make()
    end run
end impl ComponentRecordToScalarLetTest
"#;
    let err = lower_src(src).expect_err("record method result must not initialize scalar local");
    let msg = assert_unsupported(&err);
    assert!(msg.contains("incompatible type"), "{msg}");
}

#[test]
fn component_method_assignment_rejects_scalar_to_record_local() {
    let src = r#"
transaction Txn
    value : uint<8> default 0
end transaction Txn

sequencer TxnSource
    hookable scalar() -> uint<8>
        return 7
    end scalar
end sequencer TxnSource

testbench Tb
    dut : Top
    sqr : TxnSource
end testbench Tb

impl ComponentScalarToRecordAssignTest for Tb
    run
        let t : Txn
        t = sqr.scalar()
    end run
end impl ComponentScalarToRecordAssignTest
"#;
    let err = lower_src(src).expect_err("scalar method result must not assign to record local");
    let msg = assert_unsupported(&err);
    assert!(msg.contains("incompatible type"), "{msg}");
}

#[test]
fn component_method_let_rejects_scalar_to_record_local() {
    let src = r#"
transaction Txn
    value : uint<8> default 0
end transaction Txn

sequencer TxnSource
    hookable scalar() -> uint<8>
        return 7
    end scalar
end sequencer TxnSource

testbench Tb
    dut : Top
    sqr : TxnSource
end testbench Tb

impl ComponentScalarToRecordLetTest for Tb
    run
        let t : Txn = sqr.scalar()
    end run
end impl ComponentScalarToRecordLetTest
"#;
    let err = lower_src(src).expect_err("scalar method result must not initialize record local");
    let msg = assert_unsupported(&err);
    assert!(msg.contains("incompatible type"), "{msg}");
}

#[test]
fn component_method_assignment_rejects_wrong_record_local() {
    let src = r#"
transaction A
    value : uint<8> default 0
end transaction A

transaction B
    value : uint<8> default 0
end transaction B

sequencer TxnSource
    hookable make_b() -> B
        let t : B
        t.value = 7
        return t
    end make_b
end sequencer TxnSource

testbench Tb
    dut : Top
    sqr : TxnSource
end testbench Tb

impl ComponentWrongRecordAssignTest for Tb
    run
        let t : A
        t = sqr.make_b()
    end run
end impl ComponentWrongRecordAssignTest
"#;
    let err = lower_src(src).expect_err("wrong record method result must not assign");
    let msg = assert_unsupported(&err);
    assert!(msg.contains("incompatible type"), "{msg}");
}

#[test]
fn component_method_let_rejects_wrong_record_local() {
    let src = r#"
transaction A
    value : uint<8> default 0
end transaction A

transaction B
    value : uint<8> default 0
end transaction B

sequencer TxnSource
    hookable make_b() -> B
        let t : B
        t.value = 7
        return t
    end make_b
end sequencer TxnSource

testbench Tb
    dut : Top
    sqr : TxnSource
end testbench Tb

impl ComponentWrongRecordLetTest for Tb
    run
        let t : A = sqr.make_b()
    end run
end impl ComponentWrongRecordLetTest
"#;
    let err = lower_src(src).expect_err("wrong record method result must not initialize local");
    let msg = assert_unsupported(&err);
    assert!(msg.contains("incompatible type"), "{msg}");
}
/// Issue #452: a test-scope `let` read (un-shadowed) at the top level of the
/// check phase must still be promoted to a `_tb` host field even when an
/// *unrelated nested* `let` of the same name shadows it elsewhere in the
/// check body. The old promotion decision flattened every check read and
/// every check decl into two order-free sets, so any nested same-named decl
/// suppressed promotion of the outer let — making the top-level read fail to
/// resolve ("test-scope `let` referenced in the check phase"), a loud
/// over-rejection of a pattern v1 accepts. The read-site lowering already
/// resolves an in-scope local first, so the inner shadow's own reads are
/// handled correctly without promotion; only the decision needed to become
/// scope-aware. (Verified e2e: this pattern runs clean under both `--codegen
/// v1` and the default tbir backend.)
#[test]
fn check_phase_let_promotes_despite_nested_shadow() {
    let src = r#"
domain SysDomain
  freq_mhz: 100
end domain SysDomain

test ShadowPromote
    let dut : Top
    let expected_total = 0

    clock clk = SysDomain

    run
        dut.en = 1
        expected_total = expected_total + 1
        wait 1 cycle
    end run

    check
        assert expected_total == 1 else fail("outer ${expected_total}")
        if expected_total == 1
            let expected_total = 99
            assert expected_total == 99 else fail("inner ${expected_total}")
        end if
    end check
end test ShadowPromote
"#;
    // Old behavior: LowerError::Unsupported ("test-scope `let expected_total`
    // referenced in the check phase"). With the scope-aware decision it
    // lowers, verifies, and emits cleanly through the full tbir pipeline.
    let prog = lower_src(src).expect("scope-aware promotion lets the outer read resolve");
    verify::verify_program(&prog).expect("verifies");
    let cpp = emit_cpp_src(src);
    assert!(cpp.contains("ShadowPromote"), "emitted the test driver");
}

/// Issue #458 (same class as #452, closure-hook side): a test-scope `let`
/// captured (bare, un-shadowed) by a method-hook body must still be promoted
/// to a `_tb` host field even when an *unrelated nested* `let` of the same
/// name shadows it elsewhere in the hook body. The old hook-promotion
/// decision flattened every hook-body read and every hook-body decl into two
/// order-free sets, so any nested same-named `let` suppressed promotion of
/// the captured outer let — leaving the hook's bare read unresolved. The
/// read-site lowering resolves an in-scope local first, so the inner shadow
/// is handled without promotion; only the decision needed to be scope-aware.
/// (Verified e2e: runs clean under both `--codegen v1` and tbir.)
#[test]
fn method_hook_let_promotes_despite_nested_shadow() {
    let src = r#"
transaction Op
    value : uint<32>
end transaction Op

transactor Drv
    dut : Top
    when active
        hookable send(t: Op)
            dut.en = 1
            wait 1 cycle
            dut.en = 0
        end send
    end when
end transactor Drv

testbench HookShadowTb
    dut : Top
    drv : Drv active
end testbench HookShadowTb

impl HookLetShadowTest for HookShadowTb
    let acc : uint<32> = 0

    on drv.send post
        acc = acc + t.value
        if t.value == 7
            let acc = 99
            log(info, "inner acc=${acc}")
        end if
    end on

    run
        drv.dut = dut
        let op : Op
        op.value = 7
        drv.send(op)
        wait 1 cycle
        assert acc == 7 else fail("acc=${acc}")
    end run
end impl HookLetShadowTest
"#;
    // Old behavior: the nested `let acc` suppressed promotion, so the hook's
    // `acc` read failed to resolve (LowerError). With the scope-aware
    // decision the outer `acc` is promoted and the hook lowers/verifies.
    let prog = lower_src(src).expect("scope-aware hook promotion resolves the captured `acc`");
    verify::verify_program(&prog).expect("verifies");
}

// ── v1 ↔ TB-IR parity fixes (review of #543) ─────────────────────────

/// A constant-bounded bit slice has a known width, so `sext` must fill
/// from the slice's MSB. TB-IR's inference lacked the arm v1 has, so
/// `p[7:0].sext<64>()` skipped the fill and yielded `0x00..AB` where v1
/// produced `0xFF..AB` — same source, two values.
#[test]
fn bit_slice_receiver_width_drives_the_sign_fill() {
    let cpp = emit_cpp_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let p : uint<32> = 0xAB
        let x = p[7:0].sext<64>()
        log(info, "${x:016x}")
    end run
end impl T"#,
    );
    assert!(
        cpp.contains("<< 56)) >> 56"),
        "expected an 8-bit shift-fill from the slice width; got:\n{cpp}"
    );
}

/// The same inference feeds the direction check, so a no-op `.trunc<N>()`
/// on an N-bit slice is now caught here exactly as v1 catches it.
#[test]
fn bit_slice_receiver_width_drives_the_direction_check() {
    let err = lower_src(
        r#"test T
    let dut : Top
    run
        let p : uint<32> = 0xAB
        let t = p[7:0].trunc<8>()
    end run
end test T"#,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("on a 8-bit value"),
        "expected the wrong-direction diagnostic, got: {err}"
    );
}

/// Narrowing into a declared-width local is a user error with a fix, not
/// the verifier's internal-error channel.
#[test]
fn narrowing_assignment_is_a_lowering_diagnostic() {
    for (src, want) in [
        (
            r#"test T
    let dut : Top
    run
        let a : uint<256> = 5
        let b : uint<200> = a
    end run
end test T"#,
            "use `.trunc<200>()`",
        ),
        (
            r#"test T
    let dut : Top
    run
        let a : uint<32> = 5
        let b : uint<8> = 0
        b = a
    end run
end test T"#,
            "use `.trunc<8>()`",
        ),
    ] {
        let err = lower_src(src).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("narrows") && msg.contains(want),
            "expected a narrowing diagnostic containing `{want}`, got: {msg}"
        );
    }
}

/// The narrowing check is scoped to the shapes the verifier itself types.
/// A value provably masked below the declared width (`(wide & 0xFF) >> 4`
/// into a `uint<8>`) must still lower — lowering's own expression typing
/// over-approximates a binary as its left operand's declared width.
#[test]
fn masked_wide_value_is_not_a_narrowing_assignment() {
    let prog = lower_src(
        r#"test T
    let dut : Top
    run
        let wide : uint<128> = 240
        let x : uint<8> = (wide & 0xFF) >> 4
        assert x == 15 else fail("x=${x}")
    end run
end test T"#,
    )
    .expect("masked wide value lowers");
    verify::verify_program(&prog).expect("verifies");
}

/// TB-IR's half of the full-width sign fill: the outer cast must be
/// signed, matching v1's, or the two backends' `> 0` / `/` / `>>` on a
/// filled `sext<64>` disagree. Nothing else in the suite pins it.
#[test]
fn full_width_sign_fill_is_signed() {
    let cpp = emit_cpp_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let p : uint<32> = 0xAB
        let x = p[7:0].sext<64>()
        assert x > 0 else fail("neg")
    end run
end impl T"#,
    );
    assert!(
        cpp.contains("((int64_t)(((int64_t)(") && !cpp.contains("((uint64_t)(((int64_t)("),
        "the width-64 fill must be signed in TB-IR too; got:\n{cpp}"
    );
}

/// A nested wrap composes — `(1 +% 2) +% 3` masks at each step's own
/// operand width. v1 has always folded these; without the recursive arm
/// TB-IR rejected chains v1 accepts.
#[test]
fn const_wrap_folds_through_nested_wraps() {
    let prog = lower_src(
        r#"const N : uint<8> = (1 +% 2) +% 3

test T
    let dut : Top
    run
        assert N == 2 else fail("n")
    end run
end test T"#,
    )
    .expect("nested const wraps fold");
    assert!(
        format!("{prog}").contains("(2 == 2)"),
        "nested wrap must fold at each step's own width"
    );
}

/// Guards the backout in `5bad8ab`. An overflow guard was briefly added to
/// the const fold to stop TB-IR folding what v1's `constexpr` cannot
/// compile; it modelled operand VALUES rather than the C++ types v1
/// emits, so it rejected this — an ordinary 32-bit wrap that v1 compiles
/// to the same 1. Re-introducing any such guard must fail here.
#[test]
fn ordinary_const_wraps_fold_even_when_the_unmasked_product_is_large() {
    let prog = lower_src(
        r#"const K : uint<32> = 0xFFFFFFFF *% 0xFFFFFFFF

test T
    let dut : Top
    run
        assert K == 1 else fail("k")
    end run
end test T"#,
    )
    .expect("a 32-bit wrap must fold regardless of the unmasked product");
    assert!(
        format!("{prog}").contains("(1 == 1)"),
        "must fold to the masked value"
    );
}

// ── Concurrent verification statements (spec §5) ─────────────────────
//
// `assert property NAME` / a bare-identifier `assert` naming a declared
// property / an inline temporal `assert` all register a per-clock-edge
// `_checkers` closure, matching v1's `emit_property_check`. `assume`
// follows the same dispatch with the `ASSUME-FAIL` severity and no error
// bump; `cover` registers a witness counter reported at end of test.

/// The source used by most of the concurrent-check tests: one declared
/// property plus a run body that references it.
fn concurrent_src(body: &str) -> String {
    format!(
        r#"property never_x
    dut.a |-> dut.b
end property never_x

test T
    let dut : Top
    run
{body}
        wait 2 cycles
    end run
end test T"#
    )
}

#[test]
fn named_property_assert_lowers_to_a_concurrent_check() {
    for site in ["        assert never_x", "        assert property never_x"] {
        let prog = lower_src(&concurrent_src(site)).expect("named property assert lowers");
        assert_eq!(
            prog.property_checks.len(),
            1,
            "one registered check for `{site}`"
        );
        let p = &prog.property_checks[0];
        assert_eq!(p.label, "never_x");
        assert_eq!(p.severity, ir::PropertySeverity::Fail);
        assert!(
            matches!(p.shape, ir::PropertyShape::Implies { .. }),
            "`a |-> b` must classify as a same-cycle implication; got {:?}",
            p.shape
        );
        let dump = format!("{prog}");
        assert!(
            dump.contains("PropertyCheck(p0)"),
            "the run body must carry the registration statement; got:\n{dump}"
        );
    }
}

/// A bare-identifier `assert` that does NOT name a declared property is
/// the ordinary immediate check — v1 dispatches on the property table,
/// not on the syntactic shape, and TB-IR must agree.
#[test]
fn bare_identifier_assert_without_a_property_stays_immediate() {
    let prog = lower_src(
        r#"test T
    let dut : Top
    run
        let flag = 1
        assert flag else fail("flag")
    end run
end test T"#,
    )
    .expect("a non-property identifier assert is an immediate check");
    assert!(
        prog.property_checks.is_empty(),
        "no concurrent check should be registered"
    );
    assert!(
        format!("{prog}").contains("AssertCheck"),
        "must lower to the immediate form"
    );
}

/// `assert property NAME` with no such declaration is a program error
/// under every backend — v1's dispatch ignores the `property` keyword and
/// emits the bare identifier, a name that does not exist in the generated
/// C++ — so it surfaces as `Invalid`, NOT as an `Unsupported` that
/// suggests `--codegen v1`.
#[test]
fn unknown_named_property_is_a_program_error_not_a_subset_gap() {
    let err = lower_src(
        r#"property p
    dut.a
end property p

test T
    let dut : Top
    run
        assert property nope
        wait 1 cycle
    end run
end test T"#,
    )
    .expect_err("an undeclared property name must be rejected");
    let msg = assert_invalid(&err);
    assert!(
        msg.contains("no property declaration with that name"),
        "got: {msg}"
    );
}

#[test]
fn implication_shapes_and_severities_match_v1() {
    let prog = lower_src(
        r#"test T
    let dut : Top
    run
        assert dut.a |=> dut.b
        assume dut.a |-> dut.b
        wait 1 cycle
    end run
end test T"#,
    )
    .expect("inline temporal assert/assume lower");
    assert_eq!(prog.property_checks.len(), 2);
    assert!(matches!(
        prog.property_checks[0].shape,
        ir::PropertyShape::ImpliesNext { .. }
    ));
    assert_eq!(prog.property_checks[0].severity, ir::PropertySeverity::Fail);
    assert_eq!(prog.property_checks[0].label, "<inline>");
    assert_eq!(
        prog.property_checks[1].severity,
        ir::PropertySeverity::AssumeFail
    );

    let cpp = emit_cpp_src(
        r#"test T
    let dut : Top
    run
        assert dut.a |=> dut.b
        assume dut.a |-> dut.b
        wait 1 cycle
    end run
end test T"#,
    );
    assert!(
        cpp.contains(r#"sim_log_line("FAIL", "property `<inline>` failed (|=>)")"#),
        "the |=> failure line must match v1's; got:\n{cpp}"
    );
    assert!(
        cpp.contains(r#"sim_log_line("ASSUME-FAIL", "property `<inline>` failed (|->)")"#),
        "a concurrent assume must log ASSUME-FAIL; got:\n{cpp}"
    );
    // Exactly one error bump — the assert's. The assume must not add one.
    let bumps = cpp.matches("ctx.errors++;").count();
    let assert_bumps = cpp
        .lines()
        .skip_while(|l| !l.contains("failed (|=>)"))
        .take(2)
        .filter(|l| l.contains("ctx.errors++"))
        .count();
    assert_eq!(assert_bumps, 1, "the concurrent assert bumps once");
    let assume_bumps = cpp
        .lines()
        .skip_while(|l| !l.contains("failed (|->)"))
        .take(2)
        .filter(|l| l.contains("ctx.errors++"))
        .count();
    assert_eq!(
        assume_bumps, 0,
        "a concurrent assume must NOT bump the error counter; got {bumps} total in:\n{cpp}"
    );
}

/// `past`/`rose`/`fell`/`stable` become latch slots: one `static`
/// previous-value cell plus a per-cycle current-value local, written back
/// at the end of the closure. Byte-for-byte the state machine v1 emits.
#[test]
fn temporal_readings_become_latch_slots() {
    let prog = lower_src(
        r#"test T
    let dut : Top
    run
        assert rose(dut.a) |-> dut.b == past(dut.c)
        wait 1 cycle
    end run
end test T"#,
    )
    .expect("temporal readings lower");
    let p = &prog.property_checks[0];
    assert_eq!(p.temporals.len(), 2, "one slot per occurrence");
    let dump = format!("{prog}");
    assert!(
        dump.contains("rose(#0)") && dump.contains("past(#1)"),
        "{dump}"
    );

    let cpp = emit_cpp_src(
        r#"test T
    let dut : Top
    run
        assert rose(dut.a) |-> dut.b == past(dut.c)
        wait 1 cycle
    end run
end test T"#,
    );
    for needle in [
        "static int64_t _harc_ps0 = 0;",
        "static int64_t _harc_ps1 = 0;",
        "(!_harc_ps0 && _harc_cur0)",
        "_harc_ps1",
        "_harc_ps0 = _harc_cur0;",
        "_harc_ps1 = _harc_cur1;",
    ] {
        assert!(cpp.contains(needle), "missing `{needle}` in:\n{cpp}");
    }
}

/// A nested `past(past(x))` would need slot-of-slot accounting the model
/// deliberately does not carry (v1's occurrence walk does not recurse into
/// operands either), so it is rejected rather than silently aliasing.
#[test]
fn nested_temporal_readings_are_rejected() {
    let err = lower_src(
        r#"test T
    let dut : Top
    run
        assert past(past(dut.a))
        wait 1 cycle
    end run
end test T"#,
    )
    .expect_err("nested temporal readings must not lower");
    let msg = assert_invalid(&err);
    assert!(
        msg.contains("not nested inside another temporal reading"),
        "got: {msg}"
    );
}

/// A temporal reading outside any concurrent check has no per-cycle latch
/// to read. v1 emits nothing for it (its `emit_expr` has no arm outside a
/// property check), so this is a program error, not a subset gap — the
/// diagnostic must not send the user to `--codegen v1`.
#[test]
fn a_temporal_reading_outside_a_check_body_is_a_program_error() {
    let err = lower_src(
        r#"test T
    let dut : Top
    run
        let x = past(dut.a)
        wait 1 cycle
    end run
end test T"#,
    )
    .expect_err("a bare temporal reading must be rejected");
    let msg = assert_invalid(&err);
    assert!(
        msg.contains("only meaningful in the CONDITION of a concurrent"),
        "got: {msg}"
    );
}

/// A concurrent check body runs inside a per-cycle closure with no
/// statement slot, so anything needing a statement-level step (here an
/// inlined impure helper) must be rejected rather than mis-lowered into
/// running once at registration.
#[test]
fn a_check_body_needing_a_statement_step_is_rejected() {
    let err = lower_src(
        r#"function settle() -> uint<8>
    wait 1 cycle
    return 1
end function settle

test T
    let dut : Top
    run
        assert rose(dut.a) |-> settle() == 1
        wait 1 cycle
    end run
end test T"#,
    )
    .expect_err("a suspending helper inside a check body must not lower");
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("concurrent") && msg.contains("statement-level step"),
        "got: {msg}"
    );
}

/// `cover` registers a witness counter and reports it at end of test. The
/// counter is FILE-scope, unlike v1's coroutine-local `static` (which the
/// end-of-test summary cannot see — v1's `cover` emission does not
/// compile; see docs/tbir-mvp.md).
#[test]
fn cover_registers_a_witness_counter_and_reports_it() {
    let src = r#"property hit_max
    dut.value == 15
end property hit_max

test T
    let dut : Top
    run
        cover hit_max
        cover dut.value == 7
        wait 2 cycles
    end run
end test T"#;
    let prog = lower_src(src).expect("cover lowers");
    assert_eq!(prog.cover_checks.len(), 2);
    assert_eq!(prog.cover_checks[0].label, "hit_max");
    assert!(
        prog.cover_checks[1].label.starts_with("cov_"),
        "an inline cover is labelled by source span; got {}",
        prog.cover_checks[1].label
    );
    assert_eq!(
        prog.tests[0].cover_checks.len(),
        2,
        "both covers belong to this test's end-of-test summary"
    );

    let cpp = emit_cpp_src(src);
    let counter = format!("_cov_{}_hits", prog.cover_checks[0].tag);
    assert!(
        cpp.contains(&format!("static uint64_t {counter} = 0;")),
        "the hit counter must be declared at file scope; got:\n{cpp}"
    );
    // File scope means it is declared before `run_T`, not inside it.
    let decl = cpp
        .find(&format!("static uint64_t {counter} = 0;"))
        .unwrap();
    let run_fn = cpp.find("run_T(").expect("the test's run function");
    assert!(
        decl < run_fn,
        "the counter must precede the run function so the summary can read it"
    );
    assert!(
        cpp.contains("harc_print_cover_summary(_cov_hit, _cov_total)")
            && cpp.contains(r#"harc_print_cover_point("hit_max""#),
        "the end-of-test summary must report each cover point; got:\n{cpp}"
    );
}

/// v1 does not translate temporal readings inside a `cover` body (its
/// `emit_expr` has no arm for them, so the operand vanishes). TB-IR gives
/// a cover body the same latch machinery as a property body.
#[test]
fn cover_bodies_carry_temporal_latches() {
    let cpp = emit_cpp_src(
        r#"test T
    let dut : Top
    run
        cover fell(dut.rst)
        wait 2 cycles
    end run
end test T"#,
    );
    assert!(
        cpp.contains("(_harc_ps0 && !_harc_cur0)"),
        "`fell` in a cover body must lower to its latch reading; got:\n{cpp}"
    );
}

/// The immediate `assume` form logs `ASSUME` and, unlike `assert`, does
/// not bump the error counter (v1's `emit_inline_assume`).
#[test]
fn immediate_assume_logs_without_bumping_errors() {
    let cpp = emit_cpp_src(
        r#"test T
    let dut : Top
    run
        assume dut.rst == 0
        wait 1 cycle
    end run
end test T"#,
    );
    let idx = cpp
        .find(r#"sim_log_line("ASSUME", "assumption failed")"#)
        .unwrap_or_else(|| panic!("missing the ASSUME line in:\n{cpp}"));
    let after = &cpp[idx..idx + 200.min(cpp.len() - idx)];
    assert!(
        !after.lines().take(2).any(|l| l.contains("ctx.errors++")),
        "an immediate assume must not bump the error counter; got:\n{after}"
    );
}

/// A bare `cover` alongside a `run` block lowers now that the hit counter
/// lives at file scope — the rejection that used to guard v1's
/// out-of-scope-counter bug no longer applies.
#[test]
fn bare_cover_alongside_a_run_block_lowers() {
    let prog = lower_fixtures(&["counter_test.harc", "counter_test_covers.harc"])
        .expect("bare covers merged into a test with a run block lower");
    assert_eq!(prog.cover_checks.len(), 4);
    assert_eq!(
        prog.tests[0].cover_checks.len(),
        4,
        "all four are reported by this test"
    );
}

/// The property-demo extension fixture exercises every temporal reading
/// and both implication shapes through the real merge path.
#[test]
fn the_property_demo_fixture_lowers_and_verifies() {
    let prog = lower_fixtures(&["counter_test.harc", "counter_test_props.harc"])
        .expect("counter property demo lowers");
    verify::verify_program(&prog).expect("verifies");
    assert_eq!(prog.property_checks.len(), 6);
    let shapes: Vec<&str> = prog
        .property_checks
        .iter()
        .map(|p| match p.shape {
            ir::PropertyShape::Implies { .. } => "|->",
            ir::PropertyShape::ImpliesNext { .. } => "|=>",
            ir::PropertyShape::Invariant(_) => "inv",
        })
        .collect();
    assert_eq!(shapes, ["inv", "|->", "|=>", "|->", "|->", "|->"]);
}

// ── Statement-position `on` handlers ─────────────────────────────────
//
// `on <bool-expr> ... end on` and `on <N> cycles ... end on` written
// inside a run body arm a per-cycle `_checkers` closure at the statement's
// position, exactly where v1's `emit_cycle_trigger` pushes it. (The
// testbench-DECLARATION-scoped forms are a separate path — they arm during
// test setup; see `TestbenchSchema::{periodic_services, cycle_services}`.)

#[test]
fn statement_position_on_handlers_lower_to_cycle_handlers() {
    let src = r#"test T
    let dut : Top
    run
        on dut.rst == 1
            log(info, "rst high")
        end on
        on 3 cycles
            log(info, "tick")
        end on
        wait 6 cycles
    end run
end test T"#;
    let prog = lower_src(src).expect("statement-position on handlers lower");
    verify::verify_program(&prog).expect("verifies");
    assert_eq!(prog.cycle_handlers.len(), 2);
    assert!(matches!(
        prog.cycle_handlers[0].kind,
        ir::CycleHandlerKind::Trigger {
            edge: ir::CycleEdge::Rising,
            ..
        }
    ));
    assert!(matches!(
        prog.cycle_handlers[1].kind,
        ir::CycleHandlerKind::Periodic { period: 3 }
    ));
    // Each body is its own zero-parameter TestHook function, so the
    // per-cycle closure can call it without capturing a block-scoped
    // lambda that dies with the enclosing `case`.
    for h in &prog.cycle_handlers {
        let f = prog.function(h.function);
        assert_eq!(f.kind, ir::FunctionKind::TestHook);
        assert!(f.params.is_empty());
    }
    let dump = format!("{prog}");
    assert!(
        dump.contains("CycleHandler(h0)") && dump.contains("CycleHandler(h1)"),
        "the run body must carry both registrations; got:\n{dump}"
    );

    let cpp = emit_cpp_src(src);
    for needle in [
        "static bool _onh_0_prev = false;",
        "if (!_onh_0_prev && _onh_0_curr) {",
        "_onh_0_prev = _onh_0_curr;",
        "static int64_t _onh_1_last = 0;",
        "if ((int64_t)cycle_count - _onh_1_last >= 3) {",
    ] {
        assert!(cpp.contains(needle), "missing `{needle}` in:\n{cpp}");
    }
    // The body lambdas must be declared before the run coroutine, not
    // inside the switch case that registers them.
    let lambda = cpp.find("_on_handler_0 = [&]").expect("body lambda");
    let run_lambda = cpp.find("_run_slot_lambda = ").expect("run lambda");
    assert!(
        lambda < run_lambda,
        "a body lambda declared inside the coroutine would dangle once its \
         `case` block ended"
    );
}

/// A `falling`/`level` edge mode selects the matching latch shape, and
/// `phase post_eval` routes the registration to the other service vector.
#[test]
fn on_handler_edge_modes_and_phase_route_like_v1() {
    let cpp = emit_cpp_src(
        r#"test T
    let dut : Top
    run
        on dut.rst falling
            log(info, "fell")
        end on
        on dut.rst phase post_eval level
            log(info, "held")
        end on
        wait 2 cycles
    end run
end test T"#,
    );
    assert!(
        cpp.contains("if (_onh_0_prev && !_onh_0_curr) {"),
        "a falling-edge handler must latch the inverse transition; got:\n{cpp}"
    );
    assert!(
        cpp.contains("_post_eval_services.push_back"),
        "`phase post_eval` must register in the post-eval vector; got:\n{cpp}"
    );
}

/// An `on` body is its own function, so it cannot see the enclosing run
/// function's locals — v1's shared `[&]` capture is not representable.
/// The reference must be reported, not silently dropped.
#[test]
fn an_on_body_referencing_an_enclosing_local_is_rejected() {
    let err = lower_src(
        r#"test T
    let dut : Top
    run
        let seen = 0
        on dut.rst == 1
            log(info, "seen=${seen}")
        end on
        wait 2 cycles
    end run
end test T"#,
    )
    .expect_err("an enclosing-local reference in a handler body must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("seen"),
        "the message must name the binding: {msg}"
    );
}

/// The two `on` shapes that need machinery a statement position cannot
/// provide are rejected with messages that say what to do instead.
#[test]
fn unsupported_on_shapes_are_rejected_precisely() {
    let hook = lower_src(
        r#"transactor Drv
    dut : Top
    when active
        hookable send(v: uint<8>)
            dut.a = v
        end send
    end when
end transactor Drv

testbench Tb
    dut : Top
    drv : Drv active
end testbench Tb

impl T for Tb
    run
        on drv.send post
            log(info, "sent")
        end on
        wait 1 cycle
    end run
end impl T"#,
    )
    .expect_err("a statement-position method hook must be rejected");
    assert!(
        assert_unsupported(&hook).contains("pre/post` hook in statement position"),
        "got: {}",
        assert_unsupported(&hook)
    );

    let period = lower_src(
        r#"test T
    let dut : Top
    run
        let n = 4
        on n cycles
            log(info, "tick")
        end on
        wait 2 cycles
    end run
end test T"#,
    )
    .expect_err("a non-literal period must be rejected");
    assert!(
        assert_unsupported(&period).contains("non-literal period"),
        "got: {}",
        assert_unsupported(&period)
    );
}

// ── Diagnostic honesty: `--codegen v1` only when v1 has it ───────────
//
// TB-IR's `Unsupported` diagnostic ends in "re-run with `--codegen v1`".
// That is only useful advice when v1 implements the construct. For the
// constructs below it does not — v1 raises its own error, or accepts the
// source and emits something that does not compile or does not mean what
// was written — so they carry `LowerError::NotImplemented` instead, which
// says what v1 actually does.

/// Every activity-composition operator (spec §17.1) plus the block form
/// of `fork` and `apply`: v1 hits its "statement not supported in v0
/// cpp_tb" fallback for each.
#[test]
fn statements_no_backend_implements_do_not_suggest_v1() {
    let cases = [
        (
            "parallel\n            dut.a = 1\n            dut.b = 2\n        end parallel",
            "`parallel`",
        ),
        (
            "schedule\n            dut.a = 1\n            dut.b = 2\n        end schedule",
            "`schedule`",
        ),
        (
            "select\n            dut.a == 1 => dut.b = 1\n            dut.a == 0 => dut.b = 2\n        end select",
            "`select`",
        ),
        (
            "fork\n            branch\n                dut.a = 1\n            end branch\n            branch\n                dut.b = 2\n            end branch\n        join_all",
            "block-form `fork",
        ),
    ];
    for (stmt, want) in cases {
        let src = format!(
            r#"test T
    let dut : Top
    run
        {stmt}
        wait 1 cycle
    end run
end test T"#
        );
        let err = lower_src(&src).unwrap_err();
        let msg = assert_not_implemented(&err, lower::V1Status::Rejects);
        assert!(msg.contains(want), "expected `{want}` in: {msg}");
    }
}

/// `apply` is reached only once the enclosing `package` is accepted —
/// which it now is, inert, exactly as v1 treats it (v1 has no
/// `Item::Package` arm at all). The gap the user actually has is the
/// `apply`, and that is what the diagnostic names.
#[test]
fn a_package_is_inert_and_apply_is_the_reported_gap() {
    let src = r#"transaction Txn
    a : uint<8>
end transaction Txn

package Short
    extend Txn
        keep a < 4
    end extend Txn
end package Short

test T
    let dut : Top
    run
        wait 1 cycle
    end run
end test T"#;
    lower_src(src).expect("an unapplied package is inert under both backends");

    let applied = src.replace(
        "        wait 1 cycle",
        "        apply Short\n        wait 1 cycle",
    );
    let err = lower_src(&applied).unwrap_err();
    let msg = assert_not_implemented(&err, lower::V1Status::Rejects);
    assert!(msg.contains("`apply`"), "got: {msg}");
    assert!(
        msg.contains("never take effect"),
        "the message must say the aspect does not apply: {msg}"
    );
}

/// The value-producing `randomize` form. The STATEMENT form lowers; v1
/// has no `emit_expr` arm for the expression form.
#[test]
fn randomize_in_expression_position_does_not_suggest_v1() {
    let err = lower_src(
        r#"transaction Txn
    a : uint<8>
end transaction Txn

test T
    let dut : Top
    run
        let t : Txn
        let ok = randomize(t)
        wait 1 cycle
    end run
end test T"#,
    )
    .unwrap_err();
    let msg = assert_not_implemented(&err, lower::V1Status::Rejects);
    assert!(msg.contains("expression position"), "got: {msg}");
}

/// Two constructs v1 accepts and then quietly gets wrong. Pointing a user
/// at v1 for these is worse than a plain rejection: they would get a
/// testbench that builds and lies.
#[test]
fn constructs_v1_silently_mis_lowers_are_flagged_as_such() {
    let bind = lower_src(
        r#"test T
    let dut : Top
    run
        let x = bind dut.a
        wait 1 cycle
    end run
end test T"#,
    )
    .unwrap_err();
    let msg = assert_not_implemented(&bind, lower::V1Status::SilentlyMisLowers);
    assert!(msg.contains("statement position"), "got: {msg}");
    assert!(
        msg.contains("silently emits something else"),
        "the message must say what v1 does: {msg}"
    );

    // v1's `RangeLit` arm emits `/* range a..b */ 0` — the range becomes
    // zero and the test keeps running.
    let range = lower_src(
        r#"test T
    let dut : Top
    run
        let r = 0 .. 7
        wait 1 cycle
    end run
end test T"#,
    )
    .unwrap_err();
    assert_not_implemented(&range, lower::V1Status::SilentlyMisLowers);
}

/// Constraint-only forms written in ordinary value position. Each is
/// lowered fine INSIDE `randomize ... with` (the typed constraint backend
/// handles them, never `lower_expr`), so the rejection here is about
/// position, and the message says so.
#[test]
fn constraint_forms_in_value_position_do_not_suggest_v1() {
    // `soft` / `dist` / `solve_order` only PARSE inside a constraint
    // body, so their value-position arms are unreachable from source;
    // `inside` parses as an ordinary binary operator anywhere.
    for (expr, want) in [("dut.a inside {1, 2}", "membership test in value position")] {
        let src = format!(
            r#"test T
    let dut : Top
    run
        let v = {expr}
        wait 1 cycle
    end run
end test T"#
        );
        let err = lower_src(&src).unwrap_err();
        let msg = assert_not_implemented(&err, lower::V1Status::Rejects);
        assert!(msg.contains(want), "expected `{want}` in: {msg}");
    }
}

/// The counterpart guarantee: a construct v1 DOES implement keeps the
/// `Unsupported` shape and the `--codegen v1` suggestion, so the two
/// classes stay meaningfully distinct.
#[test]
fn constructs_v1_implements_still_suggest_v1() {
    let err = lower_src(
        r#"transactor Drv
    dut : Top
    when active
        hookable send(v: uint<8>)
            dut.a = v
        end send
    end when
end transactor Drv

testbench Tb
    dut : Top
    drv : Drv active
end testbench Tb

impl T for Tb
    run
        on drv.send post
            log(info, "sent")
        end on
        wait 1 cycle
    end run
end impl T"#,
    )
    .unwrap_err();
    assert_unsupported(&err);
}

// ── Test-scope event channels (spec §3.4) ────────────────────────────
//
// `let e : event<T>` declares a subscriber list local to the enclosing
// function; `on e(v) ... end on` pushes a subscriber and `emit e(x)` fans
// out synchronously in subscription order. Same shape v1 emits — a
// `std::vector<std::function<void(payload)>>` plus a `for` loop — with
// the subscriber body factored into its own function.

#[test]
fn test_scope_event_channels_lower() {
    let src = r#"test T
    let dut : Top
    run
        let e : event<uint<8>>
        on e(v)
            log(info, "got ${v}")
        end on
        emit e(3)
        wait 1 cycle
    end run
end test T"#;
    let prog = lower_src(src).expect("a test-scope event channel lowers");
    verify::verify_program(&prog).expect("verifies");
    let dump = format!("{prog}");
    assert!(dump.contains("EventSubscribe"), "{dump}");
    assert!(dump.contains("EventEmit"), "{dump}");

    // The subscriber body is a ONE-parameter TestHook — the payload is
    // its parameter, so the pushed closure can forward it.
    let handler = prog
        .functions
        .iter()
        .find(|f| f.kind == ir::FunctionKind::TestHook)
        .expect("a subscriber body function");
    assert_eq!(handler.params.len(), 1);

    let cpp = emit_cpp_src(src);
    for needle in [
        "std::vector<std::function<void(uint64_t)>> e;",
        "e.push_back([&](uint64_t _p) { _on_event_0(_p); });",
        "for (auto& _s : e) _s(3);",
    ] {
        assert!(cpp.contains(needle), "missing `{needle}` in:\n{cpp}");
    }
}

/// A record payload carries the record struct by value, matching v1's
/// `std::function<void(Txn)>`.
#[test]
fn a_record_payload_event_channel_lowers() {
    let cpp = emit_cpp_src(
        r#"transaction Txn
    a : uint<8>
end transaction Txn

test T
    let dut : Top
    run
        let e : event<Txn>
        let t : Txn
        on e(x)
            log(info, "a=${x.a}")
        end on
        emit e(t)
        wait 1 cycle
    end run
end test T"#,
    );
    assert!(
        cpp.contains("std::vector<std::function<void(Txn)>> e;"),
        "a record payload is carried by value; got:\n{cpp}"
    );
    assert!(cpp.contains("e.push_back([&](Txn _p)"), "{cpp}");
}

/// The channel is a value, not a variable: an initializer, a wrong arity,
/// or a non-name payload binding are program errors, not subset gaps.
#[test]
fn malformed_event_channel_use_is_a_program_error() {
    for (src, want) in [
        (
            r#"test T
    let dut : Top
    run
        let e : event<uint<8>>
        emit e(1, 2)
        wait 1 cycle
    end run
end test T"#,
            "exactly one",
        ),
        (
            r#"test T
    let dut : Top
    run
        let e : event<uint<8>>
        on e(1)
            log(info, "x")
        end on
        wait 1 cycle
    end run
end test T"#,
            "payload binding must be a name",
        ),
    ] {
        let err = lower_src(src).unwrap_err();
        let msg = assert_invalid(&err);
        assert!(msg.contains(want), "expected `{want}` in: {msg}");
    }
}

/// A bare `emit` with no channel and no enclosing component names both
/// places a channel could come from — and does not send the user to v1,
/// which emits the fan-out over a symbol that does not exist.
#[test]
fn emit_with_no_channel_names_both_sources() {
    let err = lower_src(
        r#"test T
    let dut : Top
    run
        emit nope(1)
        wait 1 cycle
    end run
end test T"#,
    )
    .unwrap_err();
    let msg = assert_not_implemented(&err, lower::V1Status::EmitsUncompilable);
    assert!(
        msg.contains("test-scope event channel"),
        "the message must mention the test-scope form too: {msg}"
    );
}

// ── Review-gate regressions ──────────────────────────────────────────
//
// Each test below pins one defect found in the pre-PR review of the
// concurrent-check / `on`-handler / event-channel work.

/// A registration installs a closure that outlives the statement, so it
/// is only sound where the statement runs exactly once. In a transactor
/// method it would re-register on every call and its `[&]` capture would
/// read parameters that died with the call — and v1 does exactly that,
/// so the diagnostic must not offer v1 as an escape hatch.
#[test]
fn registrations_outside_the_test_body_are_rejected() {
    let cases = [
        ("cover dut.rst == 1", "a `cover` witness"),
        ("assert dut.a |-> dut.b", "a concurrent `assert`"),
        (
            "on dut.rst == 1\n                log(info, \"x\")\n            end on",
            "an `on ... end on` handler",
        ),
    ];
    for (stmt, want) in cases {
        let src = format!(
            r#"transactor Drv
    dut : Top
    when active
        hookable go()
            {stmt}
            dut.a = 1
        end go
    end when
end transactor Drv

testbench Tb
    dut : Top
    drv : Drv active
end testbench Tb

impl T for Tb
    run
        drv.go()
        wait 1 cycle
    end run
end impl T"#
        );
        let err = lower_src(&src).unwrap_err();
        let msg = assert_not_implemented(&err, lower::V1Status::SilentlyMisLowers);
        assert!(msg.contains(want), "expected `{want}` in: {msg}");
        assert!(
            msg.contains("run"),
            "the message must say where it belongs: {msg}"
        );
    }
}

/// One source `cover` inlined at two call sites is TWO registrations.
/// Keying the counter on the source span alone gave both the same
/// file-scope static name — a C++ redefinition, and a summary that read
/// one counter twice.
#[test]
fn covers_inlined_at_two_call_sites_get_distinct_counters() {
    let src = r#"function poke(d: Top)
    cover d.rst == 1
    d.rst = 1
    wait 1 cycle
end function poke

testbench Tb
    dut : Top
end testbench Tb

impl T for Tb
    run
        poke(dut)
        poke(dut)
        wait 1 cycle
    end run
end impl T"#;
    let prog = lower_src(src).expect("lowers");
    assert_eq!(
        prog.cover_checks.len(),
        2,
        "one registration per inline site"
    );
    assert_ne!(
        prog.cover_checks[0].tag, prog.cover_checks[1].tag,
        "two file-scope statics may not share a name"
    );
    let cpp = emit_cpp_src(src);
    let decls: Vec<&str> = cpp
        .lines()
        .filter(|l| l.starts_with("static uint64_t _cov_"))
        .collect();
    assert_eq!(decls.len(), 2, "{decls:?}");
    assert_ne!(
        decls[0], decls[1],
        "duplicate counter declaration: {decls:?}"
    );
}

/// A `wait` inside a statement-position `on` body lowers to `tick()`,
/// and `tick()` runs the checker pass that called the body — unbounded
/// recursion once the trigger fires. v1 emits the same shape; refuse
/// rather than reproduce it.
#[test]
fn a_wait_inside_an_on_handler_body_is_rejected() {
    let err = lower_src(
        r#"test T
    let dut : Top
    run
        on dut.rst == 1
            wait 1 cycle
            log(info, "x")
        end on
        wait 2 cycles
    end run
end test T"#,
    )
    .expect_err("a suspending handler body must not lower");
    let msg = assert_unsupported(&err);
    assert!(msg.contains("re-enters that same pass"), "got: {msg}");
}

/// A check body may legitimately read a local of the function that
/// registered it (the emitted closure captures it by reference, exactly
/// as v1 does). `harc dump-ir` renders check bodies outside any local
/// table, so it must fall back rather than index one that lacks them.
#[test]
fn dump_ir_renders_a_check_body_that_reads_a_local() {
    let prog = lower_src(
        r#"test T
    let dut : Top
    run
        let x = 3
        cover x == 3
        wait 1 cycle
    end run
end test T"#,
    )
    .expect("lowers");
    let dump = format!("{prog}"); // must not panic
    assert!(dump.contains("cover c0"), "{dump}");
}

/// Emitting a record into a scalar channel would pass a struct to a
/// `std::function<void(uint64_t)>`. The shape check must catch it;
/// signedness alone must NOT be rejected (both backends widen a scalar
/// payload to a 64-bit slot).
#[test]
fn event_payload_shape_is_checked_but_signedness_is_not() {
    let err = lower_src(
        r#"transaction Txn
    a : uint<8>
end transaction Txn

test T
    let dut : Top
    run
        let e : event<uint<8>>
        let t : Txn
        emit e(t)
        wait 1 cycle
    end run
end test T"#,
    )
    .expect_err("a record payload into a scalar channel must be rejected");
    let msg = assert_invalid(&err);
    assert!(msg.contains("event<uint>"), "got: {msg}");

    lower_src(
        r#"test T
    let dut : Top
    run
        let e : event<uint<8>>
        let s : sint<8> = 0 - 1
        emit e(s)
        wait 1 cycle
    end run
end test T"#,
    )
    .expect("a signedness difference is the benign widening v1 also performs");
}

/// A probe read that appears ONLY inside an `on <trigger>` predicate is
/// still a program probe access: the trigger renders in the registration
/// closure as `dut->rootp->…`, which needs the root header included.
#[test]
fn a_probe_read_only_in_an_on_trigger_still_pulls_the_root_header() {
    let cpp = emit_cpp_src(
        r#"testbench Tb
end testbench Tb

impl T for Tb
    let dut : CpuPipe
        probe alu_a : uint<32> at alu0.a
    end let dut

    run
        on dut.alu_a == 7
            log(info, "hit")
        end on
        wait 1 cycle
    end run
end impl T"#,
    );
    assert!(
        cpp.contains("___024root.h"),
        "the probe accessor needs the root header; got:\n{cpp}"
    );
    assert!(
        cpp.contains("rootp"),
        "the trigger must read through the probe accessor"
    );
}

/// `for t in S()` — the generator call written inline, rather than bound
/// to a `let` first. v1's `for (auto& t : S())` binds the returned vector
/// to a temporary that lives for the whole loop, so the generator runs
/// ONCE; TB-IR materializes it into a synthesized local, which has the
/// same shape and the same single evaluation.
#[test]
fn for_over_an_inline_tseq_call_lowers() {
    let src = r#"transaction Txn
    a : uint<8>
end transaction Txn

tseq S -> TSeq<Txn>
    let t : Txn
    yield t
end tseq S

test T
    let dut : Top
    run
        for t in S()
            dut.a = t.a
            wait 1 cycle
        end for
    end run
end test T"#;
    let prog = lower_src(src).expect("an inline tseq call lowers as the loop's iterable");
    verify::verify_program(&prog).expect("verifies");
    let dump = format!("{prog}");
    assert!(
        dump.contains("tseq:S()"),
        "the call must be lowered: {dump}"
    );
    assert!(
        dump.contains("SeqLen(") && dump.contains("SeqIndex("),
        "iteration must go through the seq accessors: {dump}"
    );

    let cpp = emit_cpp_src(src);
    // The generator is called exactly once, into the materialized local —
    // calling it in the loop header would re-run it every iteration.
    assert_eq!(
        cpp.matches("= S();").count(),
        1,
        "the generator must run once; got:\n{cpp}"
    );
    assert!(cpp.contains(".size()"), "{cpp}");

    // Binding it to a `let` first is the same program.
    let bound = src.replace(
        "        for t in S()",
        "        let xs = S()\n        for t in xs",
    );
    let bound_prog = lower_src(&bound).expect("the bound form still lowers");
    assert_eq!(
        bound_prog.functions.len(),
        prog.functions.len(),
        "both forms produce the same function set"
    );
}

/// Constructs no backend implements, found by probing v1 directly. Each
/// v1 outcome below was checked by emitting the construct with
/// `--codegen v1` and reading the generated C++.
#[test]
fn more_constructs_no_backend_implements() {
    // v1 emits `return <expr>;` inside the run COROUTINE, which only
    // accepts `co_return` — the TB does not compile.
    let ret = lower_src(
        r#"test T
    let dut : Top
    run
        return 1
    end run
end test T"#,
    )
    .unwrap_err();
    assert_not_implemented(&ret, lower::V1Status::EmitsUncompilable);

    // v1 emits the DUT POINTER into an integer slot: `int64_t x = dut;`.
    let bare = lower_src(
        r#"test T
    let dut : Top
    run
        let x = dut
        wait 1 cycle
    end run
end test T"#,
    )
    .unwrap_err();
    let msg = assert_not_implemented(&bare, lower::V1Status::EmitsUncompilable);
    assert!(msg.contains("bare DUT reference"), "{msg}");

    // v1 casts the bound to `uint32_t` with no range check, so a bound
    // past 2^32 silently wraps and slices the wrong bits.
    let slice = lower_src(
        r#"test T
    let dut : Top
    run
        let x = dut.a[5000000000:0]
        wait 1 cycle
    end run
end test T"#,
    )
    .unwrap_err();
    assert_not_implemented(&slice, lower::V1Status::SilentlyMisLowers);

    // v1 has no emission for a non-scalar cast: it drops the cast and
    // emits the operand alone.
    let cast = lower_src(
        r#"test T
    let dut : Top
    run
        let x = dut.a as Top
        wait 1 cycle
    end run
end test T"#,
    )
    .unwrap_err();
    assert_not_implemented(&cast, lower::V1Status::SilentlyMisLowers);

    // v1 raises its own "not supported in v0 cpp_tb" for a stray yield.
    let y = lower_src(
        r#"test T
    let dut : Top
    run
        yield 1
    end run
end test T"#,
    )
    .unwrap_err();
    assert_not_implemented(&y, lower::V1Status::Rejects);
}

/// Review-gate regressions for the sync timed-wait emission.
///
/// The poll loop introduces `_wu_start` alongside `_wu_budget`; without
/// reserving it, a user local of that name in a method body shadows the
/// cycle-count snapshot and the elapsed-cycle bound compares the wrong
/// value. And a component method / `on` handler is the OTHER synchronous
/// body emitter — lowering no longer gates the terminator out of method
/// bodies, so both emitters must render it or the second falls into an
/// internal "lowering gate failed" error.
#[test]
fn sync_timed_wait_reserves_its_state_and_covers_both_body_emitters() {
    let shadowing = XACTOR_SRC.replace(
        "wait 1 cycle\n            dut.en = 0",
        "let _wu_start = 7\n            wait until dut.count_out == 1 timeout 5 cycles\n            dut.en = 0",
    );
    let cpp = emit_cpp_src(&shadowing);
    assert!(
        cpp.contains("int64_t _wu_start = (int64_t)cycle_count;"),
        "the snapshot must keep its own name; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("uint64_t _wu_start = 0;"),
        "a user local must be renamed away from the reserved snapshot:\n{cpp}"
    );

    // A component method body — the second synchronous emitter.
    let cpp = emit_cpp_src(
        r#"scoreboard Sb
    hits : uint<32> default 0
    function watch()
        wait until hits == 1 timeout 5 cycles
        hits = hits + 1
    end function watch
end scoreboard Sb

testbench Tb
    dut : Top
    sb : Sb
end testbench Tb

impl T for Tb
    run
        sb.watch()
        wait 1 cycle
    end run
end impl T"#,
    );
    assert!(
        cpp.contains("int64_t _wu_start = (int64_t)cycle_count;")
            && cpp.contains(") < _wu_budget) tick();"),
        "a component method must render the same sync poll loop; got:\n{cpp}"
    );
}

/// Second probe sweep: six more constructs where `--codegen v1` is not an
/// escape hatch. Each `V1Status` below was established by emitting the
/// construct with `--codegen v1` and reading the generated C++ — the
/// comment on each case records what that output was.
#[test]
fn probe_sweep_two_constructs_v1_does_not_really_support() {
    // v1 emits the fixed text "fail() with non-string arg" and DROPS the
    // expression, so the failure line says nothing about the value.
    let f = lower_src(
        r#"test T
    let dut : Top
    run
        let m = 1
        fail(m)
    end run
end test T"#,
    )
    .unwrap_err();
    let msg = assert_not_implemented(&f, lower::V1Status::SilentlyMisLowers);
    assert!(msg.contains("${v}"), "the message must show the fix: {msg}");

    // Same drop on the `else fail(...)` clause — v1 emits the default
    // "assertion failed" text.
    let ef = lower_src(
        r#"test T
    let dut : Top
    run
        let m = 1
        assert dut.a == 1 else fail(m)
        wait 1 cycle
    end run
end test T"#,
    )
    .unwrap_err();
    let msg = assert_not_implemented(&ef, lower::V1Status::SilentlyMisLowers);
    assert!(
        msg.contains("`else fail(...)` message"),
        "the fixture must trip the else-fail gate, not another          SilentlyMisLowers site: {msg}"
    );

    // v1 parses the `with { … }` clause and emits nothing for it, so the
    // TB drives the un-remapped port name.
    let remap = lower_src(
        r#"testbench Tb
end testbench Tb
impl T for Tb
    let dut : Top = bind top with { a: "aa" }
    run
        wait 1 cycle
    end run
end impl T"#,
    )
    .unwrap_err();
    let msg = assert_not_implemented(&remap, lower::V1Status::SilentlyMisLowers);
    assert!(
        msg.contains("bind remaps on `let dut`"),
        "the fixture must trip the dut-remap gate: {msg}"
    );

    // v1 emits a call to a `bitbash(...)` function it never defines.
    let bb = lower_src(
        r#"test T
    let dut : Top
    run
        bitbash(dut.a)
        wait 1 cycle
    end run
end test T"#,
    )
    .unwrap_err();
    let msg = assert_not_implemented(&bb, lower::V1Status::EmitsUncompilable);
    assert!(msg.contains("bitbash"), "{msg}");
}

/// `log(<unknown severity>, …)` is neither a TB-IR gap nor a v1 escape
/// hatch: spec §7.7 defines `Severity` as a closed five-variant enum, so
/// `trace` names no severity. v1 "accepts" it only by uppercasing
/// whatever ident it finds, which is exactly what makes a typo
/// dangerous — `log(errror, …)` prints an `ERRROR` line and never bumps
/// the failure counter, so a test that should fail passes. The
/// diagnostic must be an `Invalid`, never a pointer at `--codegen v1`.
#[test]
fn an_unknown_log_severity_is_invalid_not_a_v1_gap() {
    for (sev, call) in [
        ("trace", r#"log(trace, "x")"#),
        ("errror", r#"log(errror, "x")"#),
        ("trace", r#"logf("f.log", trace, "x")"#),
    ] {
        let err = lower_src(&format!(
            r#"test T
    let dut : Top
    run
        {call}
        wait 1 cycle
    end run
end test T"#
        ))
        .unwrap_err();
        let lower::LowerError::Invalid(msg) = &err else {
            panic!("`{call}` must be Invalid, not a backend gap: {err:?}");
        };
        assert!(
            msg.contains(&format!("`{sev}` is not a log severity"))
                && msg.contains("debug, info, warn, error, fatal"),
            "{msg}"
        );
        assert!(
            !msg.contains("codegen v1"),
            "a typo'd severity must not send the user to a backend that accepts it \
             silently: {msg}"
        );
    }
}

/// Probes and bind remaps on a NON-`dut` binding are one family with one
/// rule, and the family is large (bus, regblock, addrmap, initiator BFM,
/// bound-to transactor, target responder, component, transactor). Fixing
/// one arm and leaving the rest saying "re-run with `--codegen v1`" is
/// the failure mode this pins.
///
/// v1's behavior is uniform: it emits no probe accessor for any binding
/// but `dut` (so the declaration is inert and a read of it does not
/// compile), and it drops a `with { … }` remap clause entirely. Most
/// arms need a differently-shaped fixture to reach, so the family rule is
/// checked structurally over the lowering sources — one representative
/// arm is exercised end-to-end below it.
#[test]
fn probe_and_remap_rejections_are_one_family_with_one_rule() {
    let lower_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ir/lower");
    let mut offenders = Vec::new();
    let mut seen = 0usize;
    for entry in std::fs::read_dir(&lower_dir).expect("read src/ir/lower") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read lowering source");
        for (i, _) in src
            .match_indices("\"probe declarations on")
            .chain(src.match_indices("\"bind remaps on"))
        {
            seen += 1;
            // Walk back to the nearest constructor call and check which
            // one it is.
            let head = &src[..i];
            let ni = head
                .rfind("not_implemented(")
                .map(|x| x as isize)
                .unwrap_or(-1);
            let un = head.rfind("unsupported(").map(|x| x as isize).unwrap_or(-1);
            if un > ni {
                let line = src[..i].matches('\n').count() + 1;
                offenders.push(format!("{}:{line}", path.display()));
            }
        }
    }
    assert!(
        seen >= 12,
        "expected to find the whole probe/bind-remap family; found {seen} sites"
    );
    assert!(
        offenders.is_empty(),
        "these probe/bind-remap rejections still send the user to `--codegen v1`, \
         which emits no probe accessor and drops remap clauses: {offenders:?}"
    );
}

/// One arm of that family exercised end-to-end, so the structural scan
/// above cannot pass over dead code.
#[test]
fn a_probe_on_a_transactor_instance_is_rejected_without_pointing_at_v1() {
    let err = lower_src(
        r#"transactor Drv
    dut : Top
    when active
        hookable go()
            dut.a = 1
        end go
    end when
end transactor Drv

testbench Tb
    dut : Top
end testbench Tb

impl T for Tb
    let drv : Drv active
        probe p : uint<8> at inner.sig
    end let drv
    run
        wait 1 cycle
    end run
end impl T"#,
    )
    .expect_err("probes on a transactor instance are rejected");
    let msg = assert_not_implemented(&err, lower::V1Status::EmitsUncompilable);
    assert!(
        msg.contains("probe declarations on a transactor instance"),
        "{msg}"
    );
    assert!(
        msg.contains("no other binding gets a probe accessor"),
        "the family's shared explanation must be present: {msg}"
    );
}

// ---------------------------------------------------------------------
// Runtime-bounded bit slices, `else fail(...)` on a concurrent check,
// and the diagnostics around them.
// ---------------------------------------------------------------------

/// A bit slice whose bounds are not both literals lowers through the
/// runtime `harc_bits` helper — the same shape v1 emits for EVERY slice,
/// constant bounds included. Before this, `x[i:0]` was rejected as
/// "not supported yet" with a pointer at a backend that had always
/// handled it.
#[test]
fn a_runtime_bounded_bit_slice_lowers_through_the_helper() {
    for slice in ["dut.a[i:0]", "dut.a[3:i]", "dut.a[i:i]", "dut.a[i + 1:i]"] {
        let cpp = emit_cpp_src(&format!(
            r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let i = 1
        let b = {slice}
        wait 1 cycle
    end run
end impl T"#
        ));
        assert!(
            cpp.contains("harc_rt::harc_bits("),
            "`{slice}` must lower to the runtime helper:\n{cpp}"
        );
    }

    // Literal bounds keep folding into the shift-and-mask form — the
    // helper is the fallback for unknown widths, not a replacement.
    let cpp = emit_cpp_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let b = dut.a[3:1]
        wait 1 cycle
    end run
end impl T"#,
    );
    assert!(
        !cpp.contains("harc_rt::harc_bits("),
        "a constant slice must still fold to a shift and mask:\n{cpp}"
    );
}

/// A reversed literal slice names no bits. Neither backend "supports"
/// it: v1 emits `harc_bits(v, 0, 3)`, whose `hi < lo` guard returns 0,
/// so the read is silently always-zero. The diagnostic must be an
/// `Invalid` that says so, not a gap report.
#[test]
fn a_reversed_literal_bit_slice_is_invalid() {
    let err = lower_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let b = dut.a[0:3]
        wait 1 cycle
    end run
end impl T"#,
    )
    .unwrap_err();
    let lower::LowerError::Invalid(msg) = &err else {
        panic!("a reversed slice is malformed, not a backend gap: {err:?}");
    };
    assert!(
        msg.contains("`[0:3]` is reversed") && msg.contains("`[3:0]`"),
        "{msg}"
    );
    assert!(!msg.contains("codegen v1"), "{msg}");
}

/// `lower_property_check` destructures the top-level `|->` / `|=>` into
/// a `PropertyShape`, so one reaching the expression lowering sat
/// somewhere the subset does not lower: a value position, or nested
/// inside another implication (legal property syntax, just not lowered
/// here). v1 accepts BOTH and emits the C++ comma operator
/// (`a /* unsupported-op */ , b`), which compiles and evaluates to the
/// right operand alone — so the antecedent is silently dropped and the
/// check runs on half the expression. Pointing a user there would be
/// worse than saying nothing.
#[test]
fn an_implication_outside_the_top_level_does_not_point_at_v1() {
    let nested = [
        // Value position.
        "let x = (dut.a == 1) |-> (dut.b == 1)",
        // Nested inside another implication.
        "assert dut.a |-> (dut.b |-> dut.c)",
        // Nested under the `|=>` spelling.
        "assert dut.a |=> (dut.b |=> dut.c)",
    ];
    for stmt in nested {
        let err = lower_src(&format!(
            r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        {stmt}
        wait 1 cycle
    end run
end impl T"#
        ))
        .unwrap_err();
        let msg = assert_not_implemented(&err, lower::V1Status::SilentlyMisLowers);
        assert!(
            msg.contains("outside the top level of an `assert` / `assume`"),
            "`{stmt}`: {msg}"
        );
    }
}

/// The SVA sequence operators reach no emitter in either backend. v1
/// accepts them by emitting the C++ comma operator, so
/// `assert a throughout b` compiles into a check on `b` alone — the
/// worst outcome available, and exactly why the diagnostic must not
/// name v1 as an escape hatch.
#[test]
fn the_sequence_operators_are_not_implemented_by_either_backend() {
    for (op, name) in [
        ("throughout", "throughout"),
        ("within", "within"),
        ("intersect", "intersect"),
    ] {
        let err = lower_src(&format!(
            r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        assert (dut.a == 1) {op} (dut.b == 1)
        wait 1 cycle
    end run
end impl T"#
        ))
        .unwrap_err();
        let msg = assert_not_implemented(&err, lower::V1Status::SilentlyMisLowers);
        assert!(
            msg.contains(&format!("the `{name}` sequence operator")),
            "{msg}"
        );
    }
}

/// An index or field access on something neither backend can index is
/// not a v1 escape hatch: v1 passes the syntax straight through, so
/// `a[0]` on a scalar local emits `int64_t b = a[0];` and `x.foo` emits
/// `int64_t y = x.foo;` — neither compiles.
#[test]
fn indexing_and_field_access_on_a_scalar_do_not_point_at_v1() {
    let err = lower_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let a = 5
        let b = a[0]
        wait 1 cycle
    end run
end impl T"#,
    )
    .unwrap_err();
    let msg = assert_not_implemented(&err, lower::V1Status::EmitsUncompilable);
    assert!(msg.contains("index expressions"), "{msg}");

    let err = lower_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let x = 1
        let y = x.foo
        wait 1 cycle
    end run
end impl T"#,
    )
    .unwrap_err();
    let msg = assert_not_implemented(&err, lower::V1Status::EmitsUncompilable);
    assert!(
        msg.contains("field access on a non-DUT value ending in `.foo`"),
        "{msg}"
    );
}

/// `assert <concurrent> else fail("...")` reports the user's message.
/// v1 parses the clause and discards it, so every concurrent failure
/// there prints the same anonymous ``property `<inline>` failed`` line
/// no matter how many checks are registered.
#[test]
fn a_concurrent_assert_reports_its_else_fail_message() {
    let cpp = emit_cpp_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        assert past(dut.a) == 0 else fail("a moved")
        assert rose(dut.b) |-> dut.c else fail("rise without c")
        assert stable(dut.c)
        wait 1 cycle
    end run
end impl T"#,
    );
    assert!(cpp.contains(r#"sim_log_line("FAIL", "a moved")"#), "{cpp}");
    assert!(
        cpp.contains(r#"sim_log_line("FAIL", "rise without c")"#),
        "the message replaces the generic line INCLUDING its `(|->)` suffix:\n{cpp}"
    );
    // The check written without a clause keeps the generic line.
    assert!(
        cpp.contains("property `<inline>` failed"),
        "a check with no `else fail(...)` still reports generically:\n{cpp}"
    );
    assert!(
        !cpp.contains("(|->)"),
        "no generic implication line survives when both implications carry a message:\n{cpp}"
    );
}

/// The message is lowered inside the check body, so it obeys the same
/// rule as the condition: it may read locals and ports, but it may not
/// push statements into the test. A message whose interpolation would
/// need a hoisted call is rejected rather than silently evaluated once
/// at registration.
#[test]
fn a_concurrent_check_message_interpolates_from_the_closure() {
    let cpp = emit_cpp_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let lo = 0
        assert past(dut.a) == 0 else fail("a=${dut.a} lo=${lo}")
        wait 1 cycle
    end run
end impl T"#,
    );
    assert!(
        cpp.contains(r#"sim_log_line("FAIL", "a=%lld lo=%lld""#),
        "{cpp}"
    );
    assert!(
        cpp.contains("harc_rt::harc_read(dut->a)") && cpp.contains("harc_printf_ll(lo)"),
        "both captures render inside the closure:\n{cpp}"
    );
}

/// An `assume` honors the clause the same way, in both the immediate
/// and the concurrent form. v1 drops it in both, so an `assume` failure
/// there is always the bare word "assumption failed".
#[test]
fn an_assume_reports_its_else_fail_message() {
    let cpp = emit_cpp_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        assume dut.a == 1 else fail("immediate assume")
        assume past(dut.b) == 0 else fail("concurrent assume")
        wait 1 cycle
    end run
end impl T"#,
    );
    assert!(
        cpp.contains(r#"sim_log_line("ASSUME", "immediate assume")"#),
        "{cpp}"
    );
    // The concurrent form keeps its own `ASSUME-FAIL` tag; only the
    // message text comes from the clause.
    assert!(
        cpp.contains(r#"sim_log_line("ASSUME-FAIL", "concurrent assume")"#),
        "{cpp}"
    );
    // An `assume` never bumps the error counter, message or not.
    assert!(
        !cpp.contains("assumption failed"),
        "the clause replaces the generic wording:\n{cpp}"
    );
}

/// `dump-ir` must show the message, so a check's identity in the IR
/// dump matches the line it will print.
#[test]
fn the_ir_dump_shows_a_check_message() {
    let prog = lower_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        assert past(dut.a) == 0 else fail("a moved")
        wait 1 cycle
    end run
end impl T"#,
    )
    .expect("lowers");
    let dump = format!("{prog}");
    assert!(dump.contains(r#"else fail "a moved""#), "{dump}");
}

/// A runtime slice of a WIDE value must slice out of all its words. The
/// `HarcWide<N>` → `_harc_u128` conversion keeps only the low four, so
/// casting the target before the call would make `w[200:193]` on a
/// `uint<256>` read 0. The target is passed uncast so overload
/// resolution binds the wide `harc_bits`.
#[test]
fn a_runtime_slice_of_a_wide_value_is_not_cast_to_128_bits() {
    let cpp = emit_cpp_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let i = 200
        let w : uint<256> = 5
        let b = w[i:1]
        wait 1 cycle
    end run
end impl T"#,
    );
    assert!(
        cpp.contains("harc_rt::harc_bits((w), (uint32_t)(i), (uint32_t)(1))"),
        "a wide target must reach `harc_bits` uncast:\n{cpp}"
    );
    assert!(
        !cpp.contains("_harc_u128)(w)"),
        "casting to `_harc_u128` first drops every word above the low four:\n{cpp}"
    );
}

/// A transactor-method call inside a runtime slice bound is still a call
/// edge. `expr_has_transactor_edge` gates the `wait until` predicate on
/// exactly that, and a walker missing the new node would let one through
/// into a per-cycle predicate.
#[test]
fn a_transactor_call_in_a_slice_bound_is_still_a_call_edge() {
    let err = lower_src(&fixture_with_transactor_call_in_slice_bound()).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("transactor method call inside a `wait until` predicate"),
        "a call edge hidden in a slice bound must still trip the predicate gate: {msg}"
    );
}

fn fixture_with_transactor_call_in_slice_bound() -> String {
    // A `wait until` whose predicate slices a port by a bound that calls
    // a sibling method — the call is two levels down, inside the `hi`
    // operand of the slice, where a walker missing `BitSliceDyn` would
    // not find it.
    r#"
transactor Xt
    dut : Top

    when active
        hookable idx() -> uint<8>
            return 3
        end idx

        hookable wait_slice()
            wait until dut.a[idx():0] == 1
        end wait_slice
    end when
end transactor Xt

testbench XtTb
    dut : Top
    xt  : Xt active
end testbench XtTb

impl XtSliceBoundCallTest for XtTb
    run
        xt.dut = dut
        xt.wait_slice()
    end run
end impl XtSliceBoundCallTest
"#
    .to_string()
}

/// `cover` counts witnesses; it reports hit/total, not a failure, so an
/// `else fail(...)` clause has nothing to name. The parser accepts the
/// clause on any verify statement and v1 drops it silently — the exact
/// "written, accepted, lost" shape this sweep set out to remove.
#[test]
fn a_cover_rejects_an_else_fail_clause() {
    let err = lower_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        cover rose(dut.a) else fail("never covered")
        wait 1 cycle
    end run
end impl T"#,
    )
    .unwrap_err();
    let lower::LowerError::Invalid(msg) = &err else {
        panic!("a cover cannot carry a failure message: {err:?}");
    };
    assert!(
        msg.contains("`cover` counts witnesses") && msg.contains("use `assert`"),
        "{msg}"
    );
}

/// The `else fail(...)` message is lowered with NO temporal slot map:
/// `lower_fmt` re-parses each capture as a standalone fragment whose
/// spans are fragment-relative, so a capture's span can collide with a
/// real temporal occurrence's and get rewritten into that occurrence's
/// `Expr::TemporalSlot`. With the map empty, a `${past(x)}` reaches the
/// ordinary temporal gate and is rejected by name.
#[test]
fn a_check_message_cannot_read_a_temporal_slot() {
    let err = lower_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        assert past(dut.a) == 0 else fail("was ${past(dut.a)}")
        wait 1 cycle
    end run
end impl T"#,
    )
    .unwrap_err();
    let msg = assert_invalid(&err);
    assert!(
        msg.contains("not in that check's `else fail(...)` message"),
        "{msg}"
    );

    // The span-collision itself: a message whose capture is a plain
    // port read must stay a port read, never a latch. This is the
    // shape that silently emitted `_harc_ps0` before the map was
    // cleared.
    let prog = lower_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        assert past(dut.a) == 0 else fail("b=${dut.b}")
        wait 1 cycle
    end run
end impl T"#,
    )
    .expect("lowers");
    let msg = prog.property_checks[0]
        .message
        .as_ref()
        .expect("the clause lowered");
    assert!(
        msg.args
            .iter()
            .all(|a| !matches!(a.expr, ir::Expr::TemporalSlot { .. })),
        "a message capture must never alias a latch slot: {:?}",
        msg.args
    );
    // …and the emitted closure reads the port, not the latch.
    let cpp = emit_cpp_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        assert past(dut.a) == 0 else fail("b=${dut.b}")
        wait 1 cycle
    end run
end impl T"#,
    );
    assert!(
        cpp.contains(
            r#"sim_log_line("FAIL", "b=%lld", harc_rt::harc_printf_ll(harc_rt::harc_read(dut->b)))"#
        ),
        "{cpp}"
    );
}

// ---------------------------------------------------------------------
// Batch 4: `for x in <rec>.<vecfield>`, discarded queue pops, and the
// diagnostics around them.
// ---------------------------------------------------------------------

/// `for x in <rec>.<vecfield>` iterates a `Vec<T, N>` record field. v1
/// emits `for (auto& x : rec.data)` over the `std::array`; tbir lowers
/// it to a counted loop over the schema-constant length with the loop
/// variable bound to `<field>[i]`. `record_vec_field_iter_test` is the
/// equivalence fixture; this pins the emitted shape.
#[test]
fn for_in_a_record_vec_field_lowers_to_a_counted_loop() {
    let cpp = emit_cpp_src(
        r#"struct Bundle
    data : Vec<uint<32>, 4>
end struct Bundle

testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let r : Bundle
        let sum = 0
        for x in r.data
            sum = sum + x
        end for
        wait 1 cycle
    end run
end impl T"#,
    );
    // The bound is the schema length, not a runtime `size()` call.
    assert!(
        cpp.contains("< 4)"),
        "header counts to the Vec length:\n{cpp}"
    );
    assert!(
        cpp.contains("x = r.data[") && cpp.contains("sum = (sum + x)"),
        "each iteration binds the element, then runs the body:\n{cpp}"
    );
}

/// A nested path (`a.b.<vecfield>`) reaches the same loop.
#[test]
fn for_in_a_nested_record_vec_field_lowers() {
    let cpp = emit_cpp_src(
        r#"struct Inner
    data : Vec<uint<32>, 3>
end struct Inner
struct Outer
    inner : Inner
end struct Outer

testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let o : Outer
        let sum = 0
        for y in o.inner.data
            sum = sum + y
        end for
        wait 1 cycle
    end run
end impl T"#,
    );
    assert!(cpp.contains("< 3)"), "{cpp}");
    assert!(cpp.contains("y = o.inner.data["), "{cpp}");
}

/// The loop variable is a COPY of the element — v1 binds `auto& x`,
/// where a write lands back in the container, and the IR has no
/// by-reference local. Rather than drop such a write silently, lowering
/// rejects it, for `for t in <tseq-result>` as well as the new Vec form.
#[test]
fn a_write_to_a_for_loop_element_variable_is_rejected() {
    let err = lower_src(
        r#"struct Bundle
    data : Vec<uint<32>, 4>
end struct Bundle

testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let r : Bundle
        for x in r.data
            x = 7
        end for
        wait 1 cycle
    end run
end impl T"#,
    )
    .unwrap_err();
    let msg = assert_not_implemented(&err, lower::V1Status::SilentlyMisLowers);
    assert!(
        msg.contains("a write to a `for` loop's element variable"),
        "{msg}"
    );

    // A write nested inside the body's control flow is caught too — the
    // scan covers every block the body opened, not just its entry.
    let err = lower_src(
        r#"struct Bundle
    data : Vec<uint<32>, 4>
end struct Bundle

testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let r : Bundle
        for x in r.data
            if x == 1
                x = 7
            end if
        end for
        wait 1 cycle
    end run
end impl T"#,
    )
    .unwrap_err();
    assert_not_implemented(&err, lower::V1Status::SilentlyMisLowers);

    // Reading it, and writing an UNRELATED local, both stay legal.
    let cpp = emit_cpp_src(
        r#"struct Bundle
    data : Vec<uint<32>, 4>
end struct Bundle

testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let r : Bundle
        let sum = 0
        for x in r.data
            if x == 1
                sum = sum + x
            end if
        end for
        wait 1 cycle
    end run
end impl T"#,
    );
    assert!(cpp.contains("sum = (sum + x)"), "{cpp}");
}

/// A scalar record field is not iterable in either backend: v1 emits
/// `for (auto& x : rec.v)` over a `uint64_t`, which has no
/// `begin`/`end`.
#[test]
fn for_in_a_scalar_record_field_does_not_point_at_v1() {
    let err = lower_src(
        r#"struct B
    v : uint<8>
end struct B

testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let b : B
        for x in b.v
            log(info, "x")
        end for
        wait 1 cycle
    end run
end impl T"#,
    )
    .unwrap_err();
    let msg = assert_not_implemented(&err, lower::V1Status::EmitsUncompilable);
    assert!(msg.contains("over a scalar record field"), "{msg}");
}

/// A bare `q.pop()` in statement position discards its value but must
/// still RUN — the mutation is the point of writing it that way. v1
/// emits `_tb.q.pop();`; tbir now pops into a temp nothing reads.
#[test]
fn a_discarded_queue_pop_still_pops() {
    let cpp = emit_cpp_src(
        r#"testbench Tb
    dut : Top
    q : queue<uint<8>>
end testbench Tb
impl T for Tb
    run
        q.push(1)
        q.push(2)
        q.pop()
        let v = q.pop()
        assert v == 2 else fail("discarded pop must take the front")
        wait 1 cycle
    end run
end impl T"#,
    );
    let pops = cpp.matches("_tb.q.pop()").count();
    assert_eq!(pops, 2, "both pops are emitted:\n{cpp}");
}

/// The discarded pop is a FAMILY: testbench, scoreboard, component,
/// bare target-responder state, and instance-qualified target state all
/// carry the same rule. Fixing one flavor and leaving the rest saying
/// "bind the popped value" is the failure mode this pins — most need a
/// differently-shaped fixture to reach, so the family is checked
/// structurally over the lowering sources, with the scoreboard arm
/// exercised end-to-end below so the scan cannot pass over dead code.
#[test]
fn every_queue_flavor_lowers_a_discarded_pop() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ir/lower/stmts.rs"),
    )
    .expect("read the statement lowering");
    assert!(
        !src.contains("a discarded"),
        "a `discarded ... pop()` rejection is left in the statement lowering; every \
         flavor pops into an unread temp now (see `discard_slot`)"
    );
    // One arm end-to-end: a scoreboard queue reached through a
    // testbench field.
    let cpp = emit_cpp_src(
        r#"scoreboard Sb
    seen : queue<uint<8>>
end scoreboard Sb

testbench Tb
    dut : Top
    sb : Sb
end testbench Tb
impl T for Tb
    run
        sb.seen.push(1)
        sb.seen.pop()
        wait 1 cycle
    end run
end impl T"#,
    );
    assert!(cpp.contains(".seen.pop()"), "{cpp}");
}

/// A queue method that is not `push`/`pop`/`size`/`empty` is not a v1
/// escape hatch: `HarcQueue` implements exactly those four, so v1 emits
/// a call to a method the runtime never defines.
#[test]
fn an_unknown_queue_method_does_not_point_at_v1() {
    for stmt in ["q.front()", "let v = q.front()"] {
        let err = lower_src(&format!(
            r#"testbench Tb
    dut : Top
    q : queue<uint<8>>
end testbench Tb
impl T for Tb
    run
        {stmt}
        wait 1 cycle
    end run
end impl T"#
        ))
        .unwrap_err();
        let msg = assert_not_implemented(&err, lower::V1Status::EmitsUncompilable);
        assert!(msg.contains("front"), "`{stmt}`: {msg}");
    }
}

/// A `default` on a queue field: v1 emits it into the member
/// initializer (`HarcQueue<uint64_t> q = 0;`) and `HarcQueue` has no
/// such constructor.
#[test]
fn a_queue_field_default_does_not_point_at_v1() {
    let err = lower_src(
        r#"testbench Tb
    dut : Top
    q : queue<uint<8>> default 0
end testbench Tb
impl T for Tb
    run
        wait 1 cycle
    end run
end impl T"#,
    )
    .unwrap_err();
    let msg = assert_not_implemented(&err, lower::V1Status::EmitsUncompilable);
    assert!(
        msg.contains("default on testbench queue field `q`"),
        "{msg}"
    );
}

/// Names that resolve to nothing, and slots that were never declared,
/// are passed through verbatim by v1 — an undeclared identifier in the
/// emitted C++, which never compiles regardless of where it lands.
#[test]
fn unresolved_names_do_not_point_at_v1() {
    // `let x` with no type is NOT in this list: v1 emits only a comment
    // for the declaration, so whether its output compiles depends on
    // whether the name is later used — and the rejection fires at the
    // declaration, before that is known.
    let cases: [(&str, &str); 2] = [
        ("let x = nosuchthing", "unresolved name `nosuchthing`"),
        (
            "nosuchthing = 1",
            "assignment to unknown name `nosuchthing`",
        ),
    ];
    for (stmt, want) in cases {
        let err = lower_src(&format!(
            r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        {stmt}
        wait 1 cycle
    end run
end impl T"#
        ))
        .unwrap_err();
        let msg = assert_not_implemented(&err, lower::V1Status::EmitsUncompilable);
        assert!(msg.contains(want), "`{stmt}`: {msg}");
    }
}

/// Indexing a scalar record field, on either side of an assignment: v1
/// emits the subscript verbatim against a `uint64_t` member.
#[test]
fn indexing_a_scalar_record_field_does_not_point_at_v1() {
    for stmt in ["let d = b.v[1]", "b.v[1] = 3"] {
        let err = lower_src(&format!(
            r#"struct B
    v : uint<8>
end struct B

testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let b : B
        {stmt}
        wait 1 cycle
    end run
end impl T"#
        ))
        .unwrap_err();
        let msg = assert_not_implemented(&err, lower::V1Status::EmitsUncompilable);
        assert!(
            msg.contains("indexing the scalar record field `B.v`"),
            "{msg}"
        );
    }
}

/// A whole-`Vec` READ keeps its `--codegen v1` suggestion, because what
/// v1 does with one depends on where it lands: `assert r.data == r.data`
/// emits `r.data == r.data`, which compiles and works (`std::array` has
/// `operator==`). One site, several outcomes — so the honest label is
/// the one that is true somewhere, and the detail leads with the fix
/// that works everywhere.
#[test]
fn a_whole_vec_read_keeps_its_v1_suggestion() {
    let err = lower_src(
        r#"struct Bundle
    data : Vec<uint<32>, 4>
end struct Bundle

testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let r : Bundle
        assert r.data == r.data else fail("nope")
        wait 1 cycle
    end run
end impl T"#,
    )
    .unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("whole-`Vec` read of record field") && msg.contains("element-wise"),
        "{msg}"
    );
}

/// The loop-element write rejection covers every statement that names a
/// local DESTINATION, not just `Assign`. `lower_assign` routes
/// `x = sb.q.pop()` into `ScoreboardOp::QueuePop { dest }` and
/// `x = xact.m(...)` into `TransactorCall { dest }`, so a check that
/// only looked for `Assign` would pass exactly those writes through —
/// the container never updated, silently.
#[test]
fn a_loop_element_write_through_a_call_destination_is_rejected() {
    let err = lower_src(
        r#"struct Bundle
    data : Vec<uint<32>, 4>
end struct Bundle

scoreboard Sb
    seen : queue<uint<32>>
end scoreboard Sb

testbench Tb
    dut : Top
    sb : Sb
end testbench Tb
impl T for Tb
    run
        let r : Bundle
        sb.seen.push(1)
        for x in r.data
            x = sb.seen.pop()
        end for
        wait 1 cycle
    end run
end impl T"#,
    )
    .unwrap_err();
    let msg = assert_not_implemented(&err, lower::V1Status::SilentlyMisLowers);
    assert!(
        msg.contains("a write to a `for` loop's element variable"),
        "{msg}"
    );
}

/// `for t in <tseq-result>` deliberately does NOT get the write
/// rejection. Its by-copy loop variable predates this sweep, and
/// `for t in txns … t.addr = … end for` is an idiom both backends
/// accept today — turning it into an error would break working
/// programs, which is not what a gap-closing change may do.
#[test]
fn a_tseq_loop_element_write_still_lowers() {
    let cpp = emit_cpp_src(
        r#"transaction Txn
    addr : uint<32> default 0
end transaction Txn

tseq burst() -> TSeq<Txn>
    let t : Txn
    yield t
end tseq burst

testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        for t in burst()
            t.addr = 5
        end for
        wait 1 cycle
    end run
end impl T"#,
    );
    assert!(cpp.contains("addr = 5"), "{cpp}");
}

/// A non-leaf element selector (`t.entries[k].data`) is snapshotted into
/// a temp BEFORE the loop. v1's range-for evaluates the container
/// expression once; a selector left inside the per-iteration bind would
/// walk a different row each time the body changed `k`.
#[test]
fn a_vec_loop_snapshots_its_container_selector() {
    let cpp = emit_cpp_src(
        r#"struct Row
    data : Vec<uint<32>, 3>
end struct Row
struct Tbl
    entries : Vec<Row, 4>
end struct Tbl

testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let t : Tbl
        let k = 0
        let sum = 0
        for x in t.entries[k].data
            sum = sum + x
            k = k + 1
        end for
        wait 1 cycle
    end run
end impl T"#,
    );
    assert!(
        !cpp.contains("t.entries[k]"),
        "the selector must be snapshotted, not re-read each iteration:\n{cpp}"
    );
    assert!(
        cpp.contains("t.entries[__t") && cpp.contains("k = (k + 1)"),
        "the body still advances `k`; the loop just stops following it:\n{cpp}"
    );
}

/// A port read in the selector hoists into a `DutRead` ahead of the
/// loop. Left inside the bind it reached the verifier as
/// `PortInDisallowedPosition` — an internal-bug channel, printed to the
/// user as raw IR.
#[test]
fn a_port_in_a_vec_loop_selector_hoists() {
    let cpp = emit_cpp_src(
        r#"struct Row
    data : Vec<uint<32>, 3>
end struct Row
struct Tbl
    entries : Vec<Row, 4>
end struct Tbl

testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let t : Tbl
        let sum = 0
        for x in t.entries[dut.a].data
            sum = sum + x
        end for
        wait 1 cycle
    end run
end impl T"#,
    );
    assert!(
        cpp.contains("harc_rt::harc_read(dut->a)"),
        "the port read is emitted once, ahead of the loop:\n{cpp}"
    );
    assert!(
        !cpp.contains("entries[harc_rt::harc_read"),
        "and not inside the per-iteration bind:\n{cpp}"
    );
}

/// An uninitialized `let x` with no type keeps its `--codegen v1`
/// suggestion. v1 emits only a comment for the declaration, so whether
/// its output compiles depends on whether the name is later USED — and
/// the rejection fires at the declaration, before that is known.
#[test]
fn an_untyped_uninitialized_let_keeps_its_v1_suggestion() {
    let err = lower_src(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let x
        wait 1 cycle
    end run
end impl T"#,
    )
    .unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("uninitialized `let x` without a scalar type"),
        "{msg}"
    );
}

// ---------------------------------------------------------------------
// Batch 5: constant-folded field defaults, and the directional-field
// family.
// ---------------------------------------------------------------------

/// A field `default` folds through the file's constant table, on every
/// declaration that has one. v1 emits the default's SOURCE TEXT into the
/// C++ member initializer: a literal (`= 7`) and a `const` name (`= K`)
/// work there, but ANY other expression silently degrades to `= 0`, so a
/// `default 1 + 1` field starts at 0 rather than 2.
#[test]
fn a_field_default_folds_through_the_constant_table() {
    for (decl, field) in [
        // Component (env) field.
        (
            "env E\n    n : uint<8> default {}\nend env E\n\ntestbench Tb\n    dut : Top\n    top : E\nend testbench Tb",
            "n",
        ),
        // Scoreboard field.
        (
            "scoreboard E\n    n : uint<32> default {}\nend scoreboard E\n\ntestbench Tb\n    dut : Top\n    top : E\nend testbench Tb",
            "n",
        ),
    ] {
        for init in ["7", "K", "K + 2", "1 + 1", "K * 2 - 5"] {
            let cpp = emit_cpp_src(&format!(
                "const K = 7\n\n{}\nimpl T for Tb\n    run\n        wait 1 cycle\n    end run\nend impl T",
                decl.replace("{}", init)
            ));
            let want = match init {
                "7" | "K" => "= 7;",
                "K + 2" => "= 9;",
                "1 + 1" => "= 2;",
                _ => "= 9;",
            };
            assert!(
                cpp.contains(&format!("{field} {want}")),
                "`default {init}` must fold to `{want}`:\n{cpp}"
            );
        }
    }
}

/// The same on a transactor state field, which took a separate path.
#[test]
fn a_transactor_state_field_default_folds() {
    let cpp = emit_cpp_src(
        r#"const K = 9

transactor Xt
    dut : Top
    count : uint<8> default K + 1

    when active
        hookable go()
            wait 1 cycle
        end go
    end when
end transactor Xt

testbench Tb
    dut : Top
    xt  : Xt active
end testbench Tb
impl T for Tb
    run
        xt.dut = dut
        xt.go()
    end run
end impl T"#,
    );
    assert!(cpp.contains("count = 10;"), "{cpp}");
}

/// A default that is not constant at all is not a v1 escape hatch: v1
/// emits `= 0` and runs, so the field silently holds the wrong value.
/// An ILLEGAL constant evaluation is a different class again — it gets
/// the `LowerError::Invalid` a `const` declaration would get for the
/// same expression, including the field's own range check.
#[test]
fn a_bad_field_default_is_classified_by_why_it_is_bad() {
    let src = |init: &str| {
        format!(
            r#"const K = 7

env E
    n : uint<8> default {init}
end env E

testbench Tb
    dut : Top
    top : E
end testbench Tb
impl T for Tb
    run
        wait 1 cycle
    end run
end impl T"#
        )
    };

    // Not constant at all → a backend-gap report, and NOT a pointer at
    // v1, which accepts it and emits `= 0`.
    let err = lower_src(&src(r#""x""#)).unwrap_err();
    let msg = assert_not_implemented(&err, lower::V1Status::SilentlyMisLowers);
    assert!(
        msg.contains("non-constant default on component field `E.n`"),
        "{msg}"
    );

    // Constant, but illegal — the same three shapes a `const`
    // declaration rejects, reported the same way.
    for (init, want) in [
        ("1 / 0", "division by zero"),
        ("-1", "negative values cannot initialize an unsigned constant"),
        ("300", "does not fit `uint<8>` (max 255)"),
    ] {
        let err = lower_src(&src(init)).unwrap_err();
        let msg = assert_invalid(&err);
        assert!(msg.contains(want), "`default {init}`: {msg}");
        assert!(
            msg.contains("component field `E.n`"),
            "the diagnostic names the field: {msg}"
        );
    }
}

/// The fold reaches testbench fields too, so `default K` means the same
/// thing on a testbench field as on an `env` field in the same source.
#[test]
fn a_testbench_field_default_folds_like_a_component_one() {
    let cpp = emit_cpp_src(
        r#"const K = 7

testbench Tb
    dut : Top
    n : uint<8> default K + 1
end testbench Tb
impl T for Tb
    run
        wait 1 cycle
    end run
end impl T"#,
    );
    assert!(cpp.contains("n = 8;"), "{cpp}");
}


// ---------------------------------------------------------------------
// Batch 6: record-typed fields on an unbound transactor, and the
// `connect` endpoint diagnostics.
// ---------------------------------------------------------------------

/// A record-typed field on an UNBOUND `transactor` routes through the
/// same `lower_state_field` the bound-to path already used. v1 emits a
/// real struct member and it works, so this was a real gap.
///
/// Reaching it exposed a latent emitter bug:
/// `Stmt::TransactorStateRecordFieldWrite` interpolated its `instance`
/// raw, and a transactor's own method body carries an EMPTY instance for
/// a self-reference — so the write emitted `.cur.tag = 5;`, with a
/// leading dot, which is not C++.
#[test]
fn a_record_field_on_an_unbound_transactor_lowers() {
    let cpp = emit_cpp_src(
        r#"struct Beat
    tag : uint<8> default 0
end struct Beat

transactor Xt
    dut : Top
    cur : Beat

    when active
        hookable go()
            cur.tag = 5
            wait 1 cycle
        end go
    end when
end transactor Xt

testbench Tb
    dut : Top
    xt  : Xt active
end testbench Tb
impl T for Tb
    run
        xt.dut = dut
        xt.go()
        assert xt.cur.tag == 5 else fail("tag")
    end run
end impl T"#,
    );
    assert!(cpp.contains("Beat cur{};"), "the member is declared:\n{cpp}");
    assert!(
        cpp.contains("self_state.cur.tag = 5;"),
        "the self-write resolves its receiver — a raw empty instance emitted \
         `.cur.tag = 5;`:\n{cpp}"
    );
    assert!(
        !cpp.contains(" .cur."),
        "no leading-dot member access survives:\n{cpp}"
    );
    assert!(cpp.contains("xt.cur.tag == 5"), "the test reads it back:\n{cpp}");
}

/// A malformed `connect` endpoint keeps its `--codegen v1` suggestion,
/// and the reason is the whole point of the class system: what v1 does
/// with a bad edge depends on where the edge SITS, not on how it is
/// malformed.
///
///   - In an INSTANTIATED env, v1 emits the path verbatim into a
///     `push_back` or a range-for, and the result usually does not
///     compile.
///   - A SINGLE-SEGMENT endpoint resolves against the owner's own
///     hookable or `out event` and works — `E_take(_tb.top, _t)`.
///   - In an UNINSTANTIATED env, v1 emits no wiring at all, so every
///     malformed edge there is invisible and v1 simply succeeds. tbir
///     resolves `connect` for every env in the merged file, so it sees
///     edges v1 never reaches.
///
/// One site, three outcomes. No single `V1Status` is honest, so the
/// suggestion stays — it is true somewhere.
#[test]
fn a_malformed_connect_endpoint_keeps_its_v1_suggestion() {
    let src = |edge: &str, inst: &str| {
        format!(
            r#"transactor Src
    observed : out event<uint<8>>
    n : uint<32> default 0

    hookable publish(v: uint<8>)
        emit observed(v)
    end publish
end transactor Src

scoreboard Sink
    seen : uint<32> default 0
    hookable take(v: uint<8>)
        seen = seen + 1
    end take
end scoreboard Sink

env E
    src  : Src passive
    sink : Sink

    connect
        {edge}
    end connect
end env E

testbench Tb
    dut : Top
{inst}
end testbench Tb
impl T for Tb
    run
        wait 2 cycles
    end run
end impl T"#
        )
    };

    // The control: the well-formed edge lowers.
    emit_cpp_src(&src("src.observed -> sink.take", "    top : E"));

    for (edge, want) in [
        ("src.observed -> sink", "sink `sink` without a method"),
        ("src -> sink.take", "source `src` without an event field"),
        (
            "src.n -> sink.take",
            "source `src.n` that is not an `out event` port",
        ),
        (
            "nosuch.observed -> sink.take",
            "path segment `nosuch` that is not a sub-component",
        ),
        (
            "src.observed -> nosuch.take",
            "path segment `nosuch` that is not a sub-component",
        ),
        (
            "src.observed -> sink.take",
            "", // control; skipped by the empty `want`
        ),
    ] {
        if want.is_empty() {
            continue;
        }
        let err = lower_src(&src(edge, "    top : E")).unwrap_err();
        let msg = assert_unsupported(&err);
        assert!(msg.contains(want), "`{edge}`: {msg}");

        // The SAME edge in an UNINSTANTIATED env: v1 emits no wiring
        // there at all, so it succeeds where tbir still rejects. This
        // is the landing that makes any `NotImplemented` claim on these
        // sites false.
        let err = lower_src(&src(edge, "")).unwrap_err();
        assert_unsupported(&err);
    }
}

/// The sink SIGNATURE checks keep the suggestion too, for the same
/// reason: v1 raises its own error for a bad arity or a non-void return
/// only when the owning env is instantiated.
#[test]
fn a_bad_connect_sink_signature_keeps_its_v1_suggestion() {
    let src = |sink: &str, method: &str| {
        format!(
            r#"transactor Src
    observed : out event<uint<8>>
    hookable publish(v: uint<8>)
        emit observed(v)
    end publish
end transactor Src

scoreboard Sink
    seen : uint<32> default 0
{method}
end scoreboard Sink

env E
    src  : Src passive
    sink : Sink

    connect
        src.observed -> sink.{sink}
    end connect
end env E

testbench Tb
    dut : Top
    top : E
end testbench Tb
impl T for Tb
    run
        wait 2 cycles
    end run
end impl T"#
        )
    };

    let err = lower_src(&src(
        "two",
        "    hookable two(a: uint<8>, b: uint<8>)\n        seen = seen + 1\n    end two",
    ))
    .unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(msg.contains("with 2 parameters"), "{msg}");

    let err = lower_src(&src(
        "ret",
        "    hookable ret(v: uint<8>) -> uint<8>\n        return v\n    end ret",
    ))
    .unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(msg.contains("that returns a value"), "{msg}");
}

/// Record state is legal in BOTH transactor declaration positions —
/// above `when active` and inside it. v1 compiles either, so closing
/// only the outer one would leave half a feature.
#[test]
fn a_record_field_lowers_in_both_transactor_positions() {
    for decl in [
        // Above `when active`.
        "    cur : Beat\n\n    when active\n",
        // Inside it.
        "\n    when active\n        cur : Beat\n",
    ] {
        let cpp = emit_cpp_src(&format!(
            r#"struct Beat
    tag : uint<8> default 0
end struct Beat

transactor Xt
    dut : Top
{decl}        hookable go()
            cur.tag = 5
            wait 1 cycle
        end go
    end when
end transactor Xt

testbench Tb
    dut : Top
    xt  : Xt active
end testbench Tb
impl T for Tb
    run
        xt.dut = dut
        xt.go()
        assert xt.cur.tag == 5 else fail("tag")
    end run
end impl T"#
        ));
        assert!(cpp.contains("Beat cur{};"), "{cpp}");
        assert!(cpp.contains("self_state.cur.tag = 5;"), "{cpp}");
    }
}

/// The record branch makes the same duplicate-name check the scalar
/// branch does. Without it, `cur : uint<32>` plus `cur : Beat` emitted
/// TWO `cur` members into the state struct.
#[test]
fn a_duplicate_transactor_state_field_is_rejected() {
    let err = lower_src(
        r#"struct Beat
    tag : uint<8> default 0
end struct Beat

transactor Xt
    dut : Top
    cur : uint<32> default 0
    cur : Beat

    when active
        hookable go()
            wait 1 cycle
        end go
    end when
end transactor Xt

testbench Tb
    dut : Top
    xt  : Xt active
end testbench Tb
impl T for Tb
    run
        xt.dut = dut
        xt.go()
    end run
end impl T"#,
    )
    .unwrap_err();
    let msg = assert_invalid(&err);
    assert!(
        msg.contains("declares state field `cur` more than once"),
        "{msg}"
    );
}

/// A record-typed field on a transactor reached through an `env` comes
/// through the COMPONENT-field machinery, which has no record kind. It
/// used to fall through to the DUT-handle arm and report "more than one
/// DUT handle field (dut, cur)" — a diagnostic that names the wrong
/// problem entirely. v1 emits a plain member here and it works, so the
/// `--codegen v1` suggestion is honest; only the wording was wrong.
#[test]
fn an_env_held_transactor_record_field_names_the_real_problem() {
    let err = lower_src(
        r#"struct Beat
    tag : uint<8> default 0
end struct Beat

transactor Xt
    dut : Top
    cur : Beat

    when active
        hookable go()
            cur.tag = 5
        end go
    end when
end transactor Xt

env E
    xt : Xt active
end env E

testbench Tb
    dut : Top
    top : E
end testbench Tb
impl T for Tb
    run
        wait 2 cycles
    end run
end impl T"#,
    )
    .unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("a record-typed field `Xt.cur` of type `Beat`"),
        "{msg}"
    );
    assert!(
        !msg.contains("DUT handle"),
        "the old wording blamed the DUT handle: {msg}"
    );
}

/// A `watchdog` on a transactor is NOT a v1 escape hatch, and the reason
/// only shows up if you check whether the emitted code ever RUNS. v1
/// emits a complete `<T>_watchdog` lambda — pre/post hook vectors, the
/// `max_idle` check against `_last_in_cycle`/`_last_out_cycle`, the FAIL
/// line, the error bump — and then never calls it.
///
/// The control that makes this a v1 bug rather than a global design: an
/// AGENT watchdog DOES get a call site (`Producer_watchdog(_tb.prod)`
/// inside a periodic closure, in `watchdog_quiesce_test`). A transactor
/// watchdog gets none, so it compiles and silently never fires.
///
/// All five watchdog sites carry it — unbound, bound-to target, and
/// initiator-side. An earlier pass reclassified only the two unbound
/// ones on the belief that the others needed a sibling bus file to
/// reach; they do not (`bus … end bus` sits inline beside a bound-to
/// transactor in `dma_engine_tlm_target_test`), and single-file probes
/// of both bound flavors show the same defined-never-called lambda.
#[test]
fn a_transactor_watchdog_does_not_point_at_v1() {
    let src = |wd_pos: &str| {
        let (outer, inner) = match wd_pos {
            "outer" => (WD_BLOCK, ""),
            _ => ("", WD_BLOCK),
        };
        format!(
            r#"transactor Xt
    dut : Top
    n : uint<32> default 0
{outer}
    when active
{inner}        hookable go()
            n = n + 1
            wait 1 cycle
        end go
    end when
end transactor Xt

testbench Tb
    dut : Top
    xt  : Xt active
end testbench Tb
impl T for Tb
    run
        xt.dut = dut
        xt.go()
    end run
end impl T"#
        )
    };
    const WD_BLOCK: &str = "    watchdog\n        period 5 cycles\n        max_idle 100 cycles\n        log(info, \"wdog\")\n    end watchdog\n";

    for pos in ["outer", "when"] {
        let err = lower_src(&src(pos)).unwrap_err();
        let msg = assert_not_implemented(&err, lower::V1Status::SilentlyMisLowers);
        assert!(msg.contains("transactor `Xt` watchdogs"), "{pos}: {msg}");
        assert!(
            msg.contains("never schedules it"),
            "the diagnostic must say WHY v1 is not a way out: {msg}"
        );
    }
}

/// The control, pinned so the claim above stays true. It must check
/// **v1's** output, not tbir's: the claim is about what `cpp_tb` emits.
/// And it must count real CALL lines — a watchdog that is never
/// scheduled still emits its `_pre`/`_post` vector declarations and two
/// internal hook loops, so a bare "name appears" count is satisfied by
/// exactly the dead shape this test exists to distinguish from.
#[test]
fn an_agent_watchdog_is_scheduled_under_v1() {
    let merged = merged_src(&fixture("watchdog_quiesce_test.harc"));
    let cpp = cpp_tb::emit(&merged).expect("v1 emits");

    let is_call = |l: &&str| {
        l.contains("Producer_watchdog(")
            && !l.contains("auto Producer_watchdog")
            && !l.contains("Producer_watchdog_pre")
            && !l.contains("Producer_watchdog_post")
    };
    let calls: Vec<&str> = cpp.lines().filter(is_call).collect();
    assert_eq!(
        calls.len(),
        1,
        "an agent watchdog is CALLED exactly once, from its periodic closure; \
         got {calls:?}"
    );

    // And the transactor flavor, through the same emitter, has none —
    // which is the whole basis for the reclassification above.
    let merged = merged_src(
        r#"transactor Xt
    dut : Top
    n : uint<32> default 0

    watchdog
        period 5 cycles
        max_idle 100 cycles
        log(info, "wdog")
    end watchdog

    when active
        hookable go()
            n = n + 1
            wait 1 cycle
        end go
    end when
end transactor Xt

testbench Tb
    dut : Top
    xt  : Xt active
end testbench Tb
impl T for Tb
    run
        xt.dut = dut
        xt.go()
    end run
end impl T"#,
    );
    let cpp = cpp_tb::emit(&merged).expect("v1 emits");
    assert!(
        cpp.contains("auto Xt_watchdog"),
        "v1 defines the transactor watchdog body:\n{cpp}"
    );
    let calls: Vec<&str> = cpp
        .lines()
        .filter(|l| {
            l.contains("Xt_watchdog(")
                && !l.contains("auto Xt_watchdog")
                && !l.contains("Xt_watchdog_pre")
                && !l.contains("Xt_watchdog_post")
        })
        .collect();
    assert!(
        calls.is_empty(),
        "…and never calls it — if this ever fires, the reclassification is stale \
         and the diagnostic must go back to `Unsupported`; got {calls:?}"
    );
}

/// A `connect` block on a transactor is not a v1 escape hatch either,
/// and the evidence is a CONTROL DIFF rather than a grep: v1's emitted
/// C++ is byte-identical with and without the block.
///
/// Note what that does NOT rest on. v1 has a verbatim fallback for an
/// unresolvable env edge — it emits the path as written and lets the
/// C++ compiler complain — so "a backend that resolved anything would
/// have errored" is false, and the byte-identity is not explained by
/// the edge being nonsense. The positive anchor below is what makes the
/// comparison mean something: the SAME edge shape inside an `env` does
/// change v1's output. The emitter wires this edge when it owns it, and
/// drops it on a transactor.
#[test]
fn a_transactor_connect_block_does_not_point_at_v1() {
    let src = |conn_outer: &str, conn_inner: &str| {
        format!(
            r#"transactor Xt
    dut : Top
    n : uint<32> default 0
{conn_outer}
    when active
{conn_inner}        hookable go()
            n = n + 1
            wait 1 cycle
        end go
    end when
end transactor Xt

testbench Tb
    dut : Top
    xt  : Xt active
end testbench Tb
impl T for Tb
    run
        xt.dut = dut
        xt.go()
    end run
end impl T"#
        )
    };
    const OUTER: &str = "    connect\n        a.b -> c.d\n    end connect\n";
    const INNER: &str = "        connect\n            a.b -> c.d\n        end connect\n";

    // All five sites: unbound (both declaration positions), bound-to
    // target, and initiator-side. Claiming "five sites" while
    // exercising two is the overclaim this sweep keeps having to undo.
    for (outer, inner) in [(OUTER, ""), ("", INNER)] {
        let err = lower_src(&src(outer, inner)).unwrap_err();
        let msg = assert_not_implemented(&err, lower::V1Status::SilentlyMisLowers);
        assert!(msg.contains("connect blocks"), "{msg}");
        assert!(msg.contains("emits NOTHING for it"), "{msg}");
    }
    for (label, program) in [
        ("bound-to target", BOUND_TARGET_CONNECT),
        ("initiator-side", BOUND_INITIATOR_CONNECT),
    ] {
        let err = lower_src(program).unwrap_err();
        let msg = assert_not_implemented(&err, lower::V1Status::SilentlyMisLowers);
        assert!(msg.contains("connect blocks"), "{label}: {msg}");
        // The `env` advice is a dead end for a `bound to` transactor —
        // both backends reject one as an env sub-field.
        assert!(
            !msg.contains("wire the endpoints from an `env`"),
            "{label}: the suggested workaround must be reachable: {msg}"
        );
    }

    // The control, in BOTH declaration positions — they are distinct v1
    // paths (`include_active`), so one does not cover the other.
    let without = cpp_tb::emit(&merged_src(&src("", ""))).expect("v1 emits");
    for (label, outer, inner) in [("outer", OUTER, ""), ("when active", "", INNER)] {
        let with = cpp_tb::emit(&merged_src(&src(outer, inner))).expect("v1 emits");
        assert_eq!(
            with, without,
            "{label}: v1 emits identical C++ with and without a transactor `connect`"
        );
    }
    // Positive anchor: without this the equality above degenerates to a
    // tautology the day v1 stops emitting the transactor at all.
    assert!(
        without.contains("struct Xt") && without.contains("Xt_go"),
        "the control compares real output, not two empty strings:\n{without}"
    );

    // …and the negative anchor: the same edge in an `env` DOES change
    // v1's output, so byte-identity is a property of the transactor
    // path, not of the edge.
    let env_src = |conn: &str| {
        format!(
            r#"transactor Src
    observed : out event<uint<8>>
    hookable publish(v: uint<8>)
        emit observed(v)
    end publish
end transactor Src

scoreboard Sink
    seen : uint<32> default 0
    hookable take(v: uint<8>)
        seen = seen + 1
    end take
end scoreboard Sink

env E
    src  : Src passive
    sink : Sink
{conn}end env E

testbench Tb
    dut : Top
    top : E
end testbench Tb
impl T for Tb
    run
        wait 2 cycles
    end run
end impl T"#
        )
    };
    let env_with =
        cpp_tb::emit(&merged_src(&env_src("    connect\n        src.observed -> sink.take\n    end connect\n")))
            .expect("v1 emits");
    let env_without = cpp_tb::emit(&merged_src(&env_src(""))).expect("v1 emits");
    assert_ne!(
        env_with, env_without,
        "an `env` connect edge DOES change v1's output — that contrast is what \
         makes the transactor byte-identity meaningful"
    );
}

const BOUND_TARGET_CONNECT: &str = r#"bus MemBus
    tlm_method read(addr: uint<32>) -> uint<32>: blocking;
end bus MemBus

transactor MemTarget bound to MemBus
    n : uint<32> default 0

    connect
        a.b -> c.d
    end connect

    thread bus.read(addr)
        n = n + 1
        return 7
    end thread
end transactor MemTarget

testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    let mem : MemBus = bind dut
    let target : MemTarget passive = bind mem
    run
        wait 2 cycles
    end run
end impl T"#;

const BOUND_INITIATOR_CONNECT: &str = r#"bus RwBus
    handshake_channel w: send kind: valid_ready
        data: uint<8>
    end handshake_channel w
end bus RwBus

transactor RwDrv bound to RwBus
    n : uint<32> default 0

    connect
        a.b -> c.d
    end connect

    hookable go()
        n = n + 1
        wait 1 cycle
    end go
end transactor RwDrv

testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    let b : RwBus = bind dut
    let drv : RwDrv active = bind b
    run
        drv.go()
    end run
end impl T"#;

/// A `thread bus.<m>(...)` responder on an UNBOUND transactor is
/// discarded by v1 — its C++ is byte-identical with and without the
/// item. The NEGATIVE anchor is what makes that a statement about the
/// unbound path rather than about target threads in general: the same
/// item on a `bound to` transactor changes v1's output substantially,
/// so the emitter serves target threads where it owns them.
#[test]
fn a_target_thread_on_an_unbound_transactor_is_discarded_by_v1() {
    let src = |thread: &str| {
        format!(
            r#"transactor Xt
    dut : Top
    n : uint<32> default 0
{thread}
    when active
        hookable go()
            n = n + 1
            wait 1 cycle
        end go
    end when
end transactor Xt

testbench Tb
    dut : Top
    xt  : Xt active
end testbench Tb
impl T for Tb
    run
        xt.dut = dut
        xt.go()
    end run
end impl T"#
        )
    };
    const THREAD: &str = "    thread bus.read(addr)\n        n = n + 1\n        return 7\n    end thread\n";

    let err = lower_src(&src(THREAD)).unwrap_err();
    let msg = assert_not_implemented(&err, lower::V1Status::SilentlyMisLowers);
    assert!(msg.contains("TLM target threads"), "{msg}");

    // Control, with a positive anchor so equality cannot pass vacuously.
    let with = cpp_tb::emit(&merged_src(&src(THREAD))).expect("v1 emits");
    let without = cpp_tb::emit(&merged_src(&src(""))).expect("v1 emits");
    assert!(
        without.contains("struct Xt") && without.contains("Xt_go"),
        "the control compares real output:\n{without}"
    );
    assert_eq!(with, without, "v1 discards the thread on an unbound transactor");

    // Negative anchor: the SAME item on a bound-to transactor DOES
    // change v1's output. Without this, the equality above would be
    // consistent with v1 not implementing target threads at all.
    let bound = |item: &str| {
        format!(
            r#"bus MemBus
    tlm_method read(addr: uint<32>) -> uint<32>: blocking;
end bus MemBus

transactor MemTarget bound to MemBus
    n : uint<32> default 0
{item}
end transactor MemTarget

testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    let mem : MemBus = bind dut
    let target : MemTarget passive = bind mem
    run
        wait 2 cycles
    end run
end impl T"#
        )
    };
    let a = cpp_tb::emit(&merged_src(&bound(THREAD))).expect("v1 emits");
    let b = cpp_tb::emit(&merged_src(&bound(
        "    hookable go()\n        n = n + 1\n    end go\n",
    )))
    .expect("v1 emits");
    assert_ne!(
        a, b,
        "v1 DOES serve target threads on a bound-to transactor — that contrast is \
         what makes the unbound byte-identity meaningful"
    );
}


/// A non-literal `@ <base>` or `size` on an addrmap instance is not a
/// v1 escape hatch. The comment in `addrmap.rs` used to claim v1
/// "const-folds arbitrary expressions" — it does not: `@ 0x50 + 0x10`
/// folds to ZERO there, and v1 emits a register write against base 0
/// instead of 0x60. The testbench pokes the wrong address and reports
/// nothing.
///
/// Both anchors are in the assertions below: v1's output for the
/// expression matches the LITERAL ZERO base (positive — it really folded
/// to 0) and differs from the literal 0x60 base (negative — the base
/// genuinely affects the emitted address, so byte-equality is not a
/// property of the fixture).
#[test]
fn a_non_literal_addrmap_base_does_not_point_at_v1() {
    let err = lower_src(&addrmap_src("@ 0x50 + 0x10 size 0x30")).unwrap_err();
    let msg = assert_not_implemented(&err, lower::V1Status::SilentlyMisLowers);
    assert!(msg.contains("non-literal `@ <base>`"), "{msg}");
    assert!(msg.contains("folds it to ZERO"), "{msg}");

    let err = lower_src(&addrmap_src("@ 0x00 size 0x18 + 0x18")).unwrap_err();
    let msg = assert_not_implemented(&err, lower::V1Status::SilentlyMisLowers);
    assert!(msg.contains("non-literal `size`"), "{msg}");

    // The control lowers, so the probe is reaching the addrmap arm
    // rather than tripping an earlier gate.
    emit_cpp_src(&addrmap_src("@ 0x00 size 0x30"));

    // v1's evidence, pinned on the emitted ADDRESS rather than whole
    // files — `0` and `0x00` are the same value spelled two ways, and
    // the claim is about the value.
    let addr_of = |inst: &str| {
        let cpp = cpp_tb::emit(&merged_src(&addrmap_src(inst))).expect("v1 emits");
        let at = cpp
            .find("AxilHelper_write(helper, (")
            .unwrap_or_else(|| panic!("no addrmap write in v1 output:\n{cpp}"));
        let rest = &cpp[at + "AxilHelper_write(helper, (".len()..];
        rest[..rest.find(")").expect("the address expression closes")].to_string()
    };

    // The expression base folds to ZERO — the write lands at 0x18…
    assert_eq!(
        addr_of("@ 0x50 + 0x10 size 0x30").replace("0x00", "0"),
        "0 + 0x18",
        "v1 folds a non-literal addrmap base to 0"
    );
    // …where the base it was WRITTEN as would have put it at 0x78. That
    // contrast is what makes the fold a silent bug rather than a
    // harmless spelling difference.
    assert_eq!(addr_of("@ 0x60 size 0x30"), "0x60 + 0x18");
}

fn addrmap_src(inst: &str) -> String {
    ADDRMAP_TB.replace("@ 0x00 size 0x30", inst)
}

const ADDRMAP_TB: &str = r#"bus BusAxiLite
    handshake_channel aw: send kind: valid_ready
        addr: uint<8>
    end handshake_channel aw
    handshake_channel w: send kind: valid_ready
        data: uint<32>
        strb: uint<4>
    end handshake_channel w
    handshake_channel ar: send kind: valid_ready
        addr: uint<8>
    end handshake_channel ar
    handshake_channel r: receive kind: valid_ready
        data: uint<32>
    end handshake_channel r
end bus BusAxiLite

transactor AxilHelper bound to BusAxiLite
    when active
        hookable write(addr: uint<8>, data: uint<32>)
            bus.aw.addr = addr
            bus.aw.valid = 1
            bus.w.send(data, 15)
            bus.aw.valid = 0
            wait 1 cycle
        end write

        hookable read(addr: uint<8>) -> uint<32>
            bus.ar.addr = addr
            bus.ar.valid = 1
            let r = bus.r.recv()
            bus.ar.valid = 0
            wait 1 cycle
            return r.data
        end read
    end when
end transactor AxilHelper

regblock DmaChan via AxilHelper width 32
    /// DMACR — bit 0 is RS (run/stop).
    register DMACR @ 0x00 access rw
        field RS : bit @ 0 reset 0 access rw
    end register DMACR

    /// SA — full-word source address.
    register SA @ 0x18 access rw
end regblock DmaChan

addrmap Soc via AxilHelper
    /// MM2S channel at base 0x00 — register addresses align with the
    /// AxiLiteRegs MM2S register layout. `size 0x30` declares the
    /// window so the codegen can statically rule out overlap with
    /// the s2mm instance below.
    instance mm2s : DmaChan @ 0x00 size 0x30
    /// S2MM channel at base 0x30 — DMACR at 0x30, SA at 0x48
    /// (0x30 + 0x18) — also matches the AxiLiteRegs layout.
    instance s2mm : DmaChan @ 0x30 size 0x30
end addrmap Soc

testbench Tb
    dut : AxiLiteRegs
end testbench Tb
impl T for Tb
    let axil : BusAxiLite = bind dut
    let helper : AxilHelper active = bind axil
    let chip   : Soc = bind helper
    run
        chip.mm2s.SA = 0x1234
        wait 2 cycles
    end run
end impl T
"#;

/// A file-scope `const` as a coverpoint slice bound. v1 emits one as
/// `(uint32_t)(K)` against its own `static constexpr K` — identical
/// semantics to the literal — while TB-IR refused anything but a plain
/// integer.
///
/// The probe that established this mutates `cov_expr_targets_test`, a
/// fixture BOTH backends already pass and which the equivalence harness
/// trace-diffs. Starting from a registered fixture rather than a
/// synthetic one is what made the result trustworthy: two earlier
/// synthetic covergroup probes measured files in which neither backend
/// had emitted any sampling logic at all, because a covergroup that is
/// never instantiated emits none.
#[test]
fn a_const_coverpoint_slice_bound_folds() {
    let fixture = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/cov_expr_targets_test.harc"),
    )
    .expect("read the registered fixture");
    let with_const = |k: u32| {
        format!("const K = {k}\n\n")
            + &fixture.replace(
                "cover dut.count_out[3:0]\n",
                "cover dut.count_out[K:0]\n",
            )
    };

    // The fold gives byte-identical output to the literal bound…
    let literal = emit_cpp_src(&fixture);
    let k3 = emit_cpp_src(&with_const(3));
    assert_eq!(
        literal, k3,
        "`[K:0]` with `const K = 3` must lower exactly like `[3:0]`"
    );

    // …and the VALUE is genuinely used, so the equality above is not
    // the fold silently dropping the bound.
    let k7 = emit_cpp_src(&with_const(7));
    assert_ne!(k3, k7, "a different const must produce a different mask");
}

/// A non-literal regblock offset or reset value is not a v1 escape
/// hatch — v1 folds both to ZERO, exactly as it does an addrmap base
/// (divergence 39).
///
/// The offset case is the worse of the two: the address TABLE entry
/// becomes `{ "SRC", 0, 32 }` and the decode becomes `addr == 0`, so the
/// register aliases whatever lives at offset 0 and its reads and writes
/// silently hit a different register.
///
/// Probed by mutating `regblock_subset_test` — registered, and passing
/// under both backends — one token at a time, so the control is
/// known-good rather than synthetic.
#[test]
fn a_non_literal_regblock_offset_or_reset_does_not_point_at_v1() {
    let fixture = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/regblock_subset_test.harc"),
    )
    .expect("read the registered fixture");

    // Control: the unmutated fixture lowers, so the probe reaches the
    // regblock arms rather than tripping an earlier gate.
    emit_cpp_src(&fixture);

    let offset = |pre: &str, off: &str| {
        format!("{pre}{}", fixture.replace(
            "register SRC     @ 0x18 access rw",
            &format!("register SRC     @ {off} access rw"),
        ))
    };
    for (pre, off) in [("const K = 0x18\n\n", "K"), ("", "0x10 + 0x08")] {
        let err = lower_src(&offset(pre, off)).unwrap_err();
        let msg = assert_not_implemented(&err, lower::V1Status::SilentlyMisLowers);
        assert!(msg.contains("non-literal `@ <addr>` offset"), "`{off}`: {msg}");
        assert!(msg.contains("aliases offset 0"), "{msg}");
    }

    let err = lower_src(&format!(
        "const R = 7\n\n{}",
        fixture.replace(
            "register CTRL    @ 0x00 reset 7 access rw",
            "register CTRL    @ 0x00 reset R access rw",
        )
    ))
    .unwrap_err();
    let msg = assert_not_implemented(&err, lower::V1Status::SilentlyMisLowers);
    assert!(msg.contains("non-literal reset value"), "{msg}");

    // v1's evidence, with both anchors. The const offset emits the
    // table entry a LITERAL ZERO offset would…
    let v1 = |src: &str| cpp_tb::emit(&merged_src(src)).expect("v1 emits");
    let folded = v1(&offset("const K = 0x18\n\n", "K"));
    assert!(
        folded.contains(r#"{ "SRC", 0, 32 }"#),
        "v1 folds the const offset to 0:\n{folded}"
    );
    // …and NOT the one it was written as, which is what makes the fold
    // a silent bug rather than a spelling difference.
    let literal = v1(&fixture);
    assert!(
        literal.contains(r#"{ "SRC", 0x18, 32 }"#),
        "the literal offset survives, so the value genuinely matters:\n{literal}"
    );
}

/// A `const` default on a `transaction` / `struct` field now folds. v1
/// emits one as `uint64_t a = K;` against its own `static constexpr K`,
/// which is CORRECT — unlike the addrmap and regblock offsets, where the
/// same literals-only local folder sits in front of a v1 that folds to
/// ZERO. Four instances of one code pattern, three different v1
/// behaviours behind it, which is why each was probed separately rather
/// than classified by analogy.
#[test]
fn a_const_record_field_default_folds() {
    let fixture = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/record_let_copy_test.harc"),
    )
    .expect("read the registered fixture");
    const OLD: &str = "a : uint<8>  default 0";

    let with = |decl: &str, pre: &str| {
        format!("{pre}{}", fixture.replace(OLD, decl))
    };

    // Control: the unmutated registered fixture lowers.
    emit_cpp_src(&fixture);

    let literal = emit_cpp_src(&with("a : uint<8>  default 9", ""));
    let k9 = emit_cpp_src(&with("a : uint<8>  default K", "const K = 9\n\n"));
    assert_eq!(
        literal, k9,
        "`default K` with `const K = 9` must lower exactly like `default 9`"
    );

    // The value is genuinely used, so the equality is the fold working
    // rather than the default being dropped.
    let k3 = emit_cpp_src(&with("a : uint<8>  default K", "const K = 3\n\n"));
    assert_ne!(k9, k3, "a different const must change the emitted default");
}

/// `bus.rs`'s remaining out-of-subset arms, every one of them probed by
/// mutating a REGISTERED fixture (`tlm_method_bus_test`,
/// `stream_burst_mon_test`) one token at a time. Five of the six turn
/// out to be shapes v1 rejects too — its `try_emit_bus_tlm_fork` and
/// `emit_bus_call` carry the same guards, in the same order, with the
/// same granularity — so none of them is a v1 escape hatch. The sixth
/// (`= bind <non-dut>`) is the opposite: v1 accepts it and emits
/// uncompilable C++.
///
/// With these, `src/ir/lower/bus.rs` has no `LowerError::Unsupported`
/// site left.
#[test]
fn the_remaining_bus_shapes_are_not_v1_escape_hatches() {
    let tlm = fixture("tlm_method_bus_test.harc");
    const FORKED: &str = "let forked0 = fork mem.read_ooo(9)";

    // Control: the unmutated fixture lowers under both backends, so the
    // mutations below reach the bus arms rather than an earlier gate.
    emit_cpp_src(&tlm);
    cpp_tb::emit(&merged_src(&tlm)).expect("v1 emits the control");

    // A DIRECT (non-`fork`) call on an `out_of_order` method. The parser
    // admits only `blocking` and `out_of_order`, so this is the sole way
    // to reach `lower_tlm_method_call`'s mode guard.
    //
    // The three `fork` RHS shape guards follow: not a call, a call whose
    // callee is a bare ident, and a field chain that is not rooted at a
    // bus binding.
    for (mutation, needle) in [
        (
            "let forked0 = mem.read_ooo(9)",
            "`out_of_order` tlm_method calls",
        ),
        ("let forked0 = fork 9", "not a direct bus tlm_method call"),
        (
            "let forked0 = fork read_ooo(9)",
            "not `<bus>.<method>(args)`",
        ),
        (
            "let forked0 = fork mem.inner.read_ooo(9)",
            "not rooted at a bus binding",
        ),
    ] {
        let src = tlm.replace(FORKED, mutation);
        let err = lower_src(&src).unwrap_err();
        let msg = assert_not_implemented(&err, lower::V1Status::Rejects);
        assert!(msg.contains(needle), "`{mutation}`: {msg}");
        // The anchor that makes `Rejects` a claim and not an assumption:
        // v1 refuses the same source.
        assert!(
            cpp_tb::emit(&merged_src(&src)).is_err(),
            "v1 must reject `{mutation}` too, or this is a real gap"
        );
    }

    // A channel method outside `send` / `recv`. `stream_burst_mon_test`
    // declares its bus locally, so no `use` resolution is needed; adding
    // a `recv()` to it is itself a control (both backends emit), which
    // pins the rejections below on the METHOD NAME rather than on the
    // call site being new.
    //
    // This arm is `EmitsUncompilable` rather than `Rejects`, and the two
    // probes below are why: v1's behaviour splits on whether the name
    // happens to be a channel SIGNAL, and only the worse half sets the
    // status.
    let stream = fixture("stream_burst_mon_test.harc");
    const WAIT: &str = "        wait 12 cycles";
    let with_call =
        |m: &str| stream.replace(WAIT, &format!("        let d = strm.s.{m}()\n{WAIT}"));

    emit_cpp_src(&with_call("recv"));
    cpp_tb::emit(&merged_src(&with_call("recv"))).expect("v1 emits a channel recv");

    for m in ["poke", "data"] {
        let src = with_call(m);
        let err = lower_src(&src).unwrap_err();
        let msg = assert_not_implemented(&err, lower::V1Status::EmitsUncompilable);
        assert!(
            msg.contains(&format!("bus channel method `.{m}(...)`")),
            "{msg}"
        );
    }

    // `.poke` is not a signal on `s`, so v1 resolves it against the
    // channel's signal list and refuses — with a better message than
    // ours.
    assert!(
        cpp_tb::emit(&merged_src(&with_call("poke"))).is_err(),
        "v1 rejects a method name that is not a channel signal"
    );
    // `.data` IS a signal, so v1 emits — and emits a signal READ with
    // the call parens still attached. `harc_read` returns a value, so
    // `harc_read(...)()` is "expression cannot be used as a function".
    // That half is what makes the arm EmitsUncompilable.
    let data = cpp_tb::emit(&merged_src(&with_call("data")))
        .expect("v1 accepts a channel method that names a signal");
    assert!(
        data.contains("harc_rt::harc_read(dut->strm_s_data)()"),
        "v1 leaves the call parens on a signal read:\n{data}"
    );
}

/// `let <b> : <Bus> = bind <x>` where `x` is not `dut`. Unlike every
/// other arm in `bus.rs`, this one is a shape v1 ACCEPTS — and then
/// emits C++ that cannot compile. v1 does not resolve the bind target
/// at all: it substitutes the bind EXPRESSION where the DUT pointer
/// goes and dereferences it, which fails two different ways depending
/// on the shape, so both are probed.
#[test]
fn a_bus_bound_to_a_non_dut_target_does_not_point_at_v1() {
    let fixture = fixture("tlm_method_blocking_bus_test.harc");
    const BIND: &str = "= bind dut";
    assert!(fixture.contains(BIND), "fixture shape changed");

    // Control: the registered fixture lowers under both backends.
    emit_cpp_src(&fixture);
    let good = cpp_tb::emit(&merged_src(&fixture)).expect("v1 emits the control");
    assert!(
        good.contains("dut->mem_read_addr"),
        "the control proves the access is real, not dropped:\n{good}"
    );

    let bound_to = |target: &str| fixture.replace(BIND, &format!("= bind {target}"));
    for target in ["nope", "dut.core"] {
        let err = lower_src(&bound_to(target)).unwrap_err();
        let msg = assert_not_implemented(&err, lower::V1Status::EmitsUncompilable);
        assert!(msg.contains("non-DUT target"), "`{target}`: {msg}");
    }

    // A bare name: v1 prints it as the DUT pointer…
    let bare = cpp_tb::emit(&merged_src(&bound_to("nope"))).expect("v1 accepts a non-DUT bind");
    assert!(
        bare.contains("nope->mem_read_addr"),
        "v1 prints the bind name as the DUT pointer:\n{bare}"
    );
    // …and never brings it into scope.
    assert!(
        !bare.contains("nope ="),
        "if v1 ever declares `nope`, this is no longer uncompilable:\n{bare}"
    );

    // A field path: v1 emits a real DUT member read and then applies
    // `operator->` to it. `harc_read` returns a VALUE, so this is
    // uncompilable for a different reason than the bare-name case — and
    // the reason the diagnostic names both.
    let field = cpp_tb::emit(&merged_src(&bound_to("dut.core"))).expect("v1 accepts a field bind");
    assert!(
        field.contains("harc_rt::harc_read(dut->core)->mem_read_addr"),
        "v1 dereferences the bind expression itself:\n{field}"
    );
}
