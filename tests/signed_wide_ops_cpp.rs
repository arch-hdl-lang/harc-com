//! Emission gate for signed wide operators (harc#657).
//!
//! `signed_wide_ops_test.harc` only runs end-to-end under Verilator
//! (`run_fixtures.sh`), so this asserts, in plain `cargo test`, that the
//! TB-IR emitter routes a SIGNED wide `< / %` and a width-aware `==` to
//! the two's-complement runtime helpers rather than the carriers' native
//! (unsigned, by-magnitude) operators. The runtime helpers themselves
//! are exercised for value-correctness by `wide_cast_cpp.rs`; this pins
//! that the emitter actually calls them.

use harc::codegen::{merge, tbir};
use harc::ir::{lower, verify};
use harc::parser::parse_source;

fn emit_tbir(src: &str) -> String {
    let parsed = parse_source(src).expect("parses");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let opts = harc::codegen::cpp_tb::EmitOpts::default();
    tbir::emit(&prog, &merged, &opts).expect("tbir emits")
}

const SRC: &str = r#"testbench T
    dut : Top
end testbench T

impl I for T
    run
        let a : sint<256> = -8
        let b : sint<256> = 3
        assert a < b else fail("lt")
        assert a <= b else fail("le")
        assert b > a else fail("gt")
        assert b >= a else fail("ge")
        let q : sint<256> = a / b
        let r : sint<256> = a % b
        assert q == r else fail("qr")
        let m : sint<100> = -8
        let n : sint<100> = 3
        assert m < n else fail("u128 lt")
        let md : sint<100> = m / n
        assert md == md else fail("u128 div")
        let q65 : sint<65> = -8 / 3
        let zero100 : sint<100> = 0
        assert q65 + zero100 == -2 else fail("mixed signed add")
        let q129 : sint<129> = -8 / 3
        let zero160 : sint<160> = 0
        assert q129 + zero160 == -2 else fail("mixed HarcWide signed add")
        wait 1 cycle
    end run
end impl I
"#;

#[test]
fn signed_wide_operators_route_to_twos_complement_helpers() {
    let cpp = emit_tbir(SRC);

    // >128 tier → `harc_wide_*`.
    assert!(
        cpp.contains("harc_wide_slt("),
        "signed wide `<`/`<=`/`>`/`>=` must route to harc_wide_slt:\n{cpp}"
    );
    assert!(
        cpp.contains("harc_wide_sdiv_checked("),
        "signed wide `/` must route to harc_wide_sdiv_checked (the zero/MIN-guarded harc_wide_sdiv):\n{cpp}"
    );
    assert!(
        cpp.contains("harc_wide_smod_checked("),
        "signed wide `%` must route to harc_wide_smod_checked (the zero/MIN-guarded harc_wide_smod):\n{cpp}"
    );

    // 65..=128 tier → the `_u128` twins.
    assert!(
        cpp.contains("harc_slt_u128("),
        "signed 65..128-bit `<` must route to harc_slt_u128:\n{cpp}"
    );
    assert!(
        cpp.contains("harc_sdiv_u128_checked("),
        "signed 65..128-bit `/` must route to harc_sdiv_u128_checked (the zero/MIN-guarded harc_sdiv_u128):\n{cpp}"
    );

    // A SIGNED wide `==` is width-masked, not the raw carrier operator
    // (which would compare inconsistent padding above the width).
    assert!(
        cpp.contains("harc_wide_mask_bits") || cpp.contains("harc_mask_u128"),
        "signed wide `==` must be width-aware:\n{cpp}"
    );
    assert!(
        cpp.contains("harc_sext_u128"),
        "mixed signed scalar arithmetic must sign-extend to its common width:\n{cpp}"
    );
    // `q129 + zero160`: both operands are sign-extended to the 161-bit
    // ARCH result width (max + 1) before the HarcWide<6> operator.
    assert!(
        cpp.contains("harc_wide_sext<6>(q129, 129, 161)")
            && cpp.contains("harc_wide_sext<6>(zero160, 160, 161)"),
        "mixed signed HarcWide arithmetic must sign-extend to its common width:\n{cpp}"
    );
}
