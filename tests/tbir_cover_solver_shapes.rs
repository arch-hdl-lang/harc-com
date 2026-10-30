//! Emitted-C++ shape pins for the R16 hunt-1 coverpoint sampler and solver
//! value fixes. CI has no Verilator, so these lock the generated C++ (and
//! that the covergroup programs typecheck against the runtime headers);
//! `tests/cover_solver_value_e2e.rs` runs the same programs end to end where
//! Verilator is present.

use harc::codegen::{cpp_tb, merge, tbir};
use harc::ir::{lower, verify};
use harc::parser::parse_source;
use std::path::Path;

fn merged_src(src: &str) -> harc::ast::SourceFile {
    let parsed = parse_source(src).expect("source parses");
    merge::merge_for_sim(vec![parsed], None).expect("merge")
}

fn emit_cpp_src(src: &str) -> String {
    let merged = merged_src(src);
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits")
}

fn assert_cpp_typechecks(name: &str, cpp: &str) {
    let dir = std::env::temp_dir().join(format!(
        "harc_tbir_cover_solver_shapes_{name}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create C++ compile directory");
    let path = dir.join("emitted.cpp");
    std::fs::write(&path, cpp).expect("write emitted C++");
    let output = std::process::Command::new("c++")
        .args(["-std=gnu++20", "-fcoroutines", "-fsyntax-only"])
        .arg("-I")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime"))
        .arg("-I")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vstub"))
        .arg(&path)
        .output()
        .expect("spawn C++ compiler");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "emitted C++ must typecheck\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn hook_cov_src(params: &str, args: &str, points: &str, run: &str) -> String {
    format!(
        r#"scoreboard Sink
    calls : uint<32> default 0
    hookable accept({params})
        calls = (calls + 1).resize<32>()
    end accept
end scoreboard Sink

covergroup Cov @(sink.accept({args}) post)
{points}
end covergroup Cov

testbench Tb
    dut : Top
    sink : Sink
    cov : Cov
end testbench Tb

impl Test for Tb
    run
{run}
        wait 1 cycle
    end run
end impl Test
"#
    )
}

/// `.zext<N>()` / `.sext<N>()` on a hook parameter extend from the bound
/// source width; `.resize<N>()` on a `sint` hook parameter keeps its sign;
/// `as uint<N>` stays an unsigned mask.
#[test]
fn cover_hook_param_width_methods_use_the_source_width() {
    let src = hook_cov_src(
        "v: sint<8>, u: uint<8>",
        "v, u",
        "    cp_z : cover v.zext<16>()
        bins
            ff = {0xFF}
        end bins
    cp_s : cover u.sext<16>()
        bins
            neg = {-1}
        end bins
    cp_r : cover v.resize<16>()
        bins
            neg = {-1}
        end bins
    cp_as : cover v as uint<16>
        bins
            raw = {0xFFFF}
        end bins",
        "        let a : sint<8> = -1
        let b : uint<8> = 0xFF
        sink.accept(a, b)",
    );
    let cpp = emit_cpp_src(&src);
    assert!(
        cpp.contains("harc_rt::harc_trunc_u128((_harc_u128)(v), 8)"),
        "zext must mask to the 8-bit source width:\n{cpp}"
    );
    assert!(
        cpp.contains(
            "int64_t _v = (int64_t)(((int64_t)(((int64_t)((uint64_t)(u) << 56)) >> 56)));"
        ),
        "sext of an unsigned hook param must sign-fill from bit 7:\n{cpp}"
    );
    assert!(
        cpp.contains(
            "int64_t _v = (int64_t)(((int64_t)(((int64_t)((uint64_t)(v) << 56)) >> 56)));"
        ),
        "resize of a sint hook param must be a signed sample:\n{cpp}"
    );
    assert!(
        cpp.contains("& 0xFFFFULL"),
        "`as uint<16>` must stay the unsigned 16-bit mask:\n{cpp}"
    );
    assert_cpp_typechecks("hook_width_methods", &cpp);
}

/// A narrowing `.zext<N>()` / `.sext<N>()` over a hook parameter is the same
/// direction error as in procedural code, not a silent truncation.
#[test]
fn cover_hook_param_narrowing_extension_is_rejected() {
    for method in ["zext", "sext"] {
        let src = hook_cov_src(
            "u: uint<16>",
            "u",
            &format!(
                "    cp : cover u.{method}<8>()
        bins
            low = {{0x34}}
        end bins"
            ),
            "        let u : uint<16> = 0x1234
        sink.accept(u)",
        );
        let merged = merged_src(&src);
        let prog = lower::lower_program(&merged).expect("lowers");
        let err = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default())
            .expect_err("narrowing extension must be rejected");
        assert!(
            err.to_string().contains(&format!(
                "`.{method}<8>()` on a 16-bit value: width must be ≥ the source"
            )),
            "{err}"
        );
    }
}

/// `~` complements within the operand width: unsigned masks, signed
/// sign-fills from the top bit.
#[test]
fn cover_bitnot_complements_within_the_operand_width() {
    let src = hook_cov_src(
        "u: uint<8>, s: sint<8>",
        "u, s",
        "    cp_u : cover ~u
        bins
            ff = {0xFF}
        end bins
    cp_s : cover ~s
        bins
            m6 = {-6}
        end bins",
        "        let b : uint<8> = 0
        let c : sint<8> = 5
        sink.accept(b, c)",
    );
    let cpp = emit_cpp_src(&src);
    assert!(
        cpp.contains("((uint64_t)(((uint64_t)(~(u))) & 0xFFULL))"),
        "unsigned ~ must mask to 8 bits:\n{cpp}"
    );
    assert!(
        cpp.contains("((int64_t)(((int64_t)((uint64_t)(~(s)) << 56)) >> 56))"),
        "signed ~ must sign-fill from bit 7:\n{cpp}"
    );
    assert_cpp_typechecks("bitnot_width", &cpp);
}

/// Coverpoints wider than 128 bits sample a masked `HarcWide<N>` and compare
/// signed bins with `harc_wide_slt`.
#[test]
fn cover_wide_samples_keep_full_width() {
    let src = hook_cov_src(
        "u: uint<200>, s: sint<200>, n: sint<8>",
        "u, s, n",
        "    cp_u : cover u
        bins
            one = {1}
            big = [0x100..u]
        end bins
    cp_s : cover s
        bins
            neg = [-10..-1]
            same_n = {n}
        end bins",
        "        let u : uint<200> = 1
        let s : sint<200> = 0
        let n : sint<8> = -1
        sink.accept(u, s, n)",
    );
    let cpp = emit_cpp_src(&src);
    assert!(
        cpp.contains("harc_rt::HarcWide<7> _v = harc_rt::harc_wide_mask_bits(harc_rt::harc_wide_zext<7>(u, 200), 200);")
            && cpp.contains("harc_rt::HarcWide<7> _v = harc_rt::harc_wide_mask_bits(harc_rt::harc_wide_zext<7>(s, 200), 200);"),
        "wide samples must be full-width masked HarcWide values:\n{cpp}"
    );
    assert!(
        cpp.contains("harc_rt::harc_wide_slt(_v, harc_rt::harc_wide_mask_bits(harc_rt::HarcWide<7>((int64_t)(-10LL)), 200), 200)"),
        "signed wide range bounds must compare with harc_wide_slt:\n{cpp}"
    );
    assert!(
        cpp.contains("harc_rt::harc_wide_sext<7>(n, 8, 200)"),
        "a narrow signed runtime bound must sign-extend to the sample width:\n{cpp}"
    );
    assert!(
        !cpp.contains("uint64_t _v = (uint64_t)(u)") && !cpp.contains("int64_t _v = (int64_t)(s)"),
        "no wide sample may be cast to a 64-bit scalar:\n{cpp}"
    );
    assert_cpp_typechecks("wide_samples", &cpp);
}

/// `dist` on a signed field (directive and `[dist]` attribute) keeps its
/// preference in an `int64_t`.
#[test]
fn signed_dist_preference_uses_a_signed_carrier() {
    let cpp = emit_cpp_src(
        r#"transaction T
    s : sint<4>
end transaction T
transaction A
    s : sint<4> with [dist {-3 :/ 100}]
end transaction A
test SD
    let dut : Top
    run
        let r : T
        randomize(r) with
            r.s dist { -2 :/ 100 }
        end randomize
        let a : A
        randomize(a)
    end run
end test SD
"#,
    );
    assert_eq!(
        cpp.matches("= (int64_t)harc_rt::random::harc_prefer_dist(")
            .count(),
        2,
        "both dist forms on a sint field must use an int64_t preference:\n{cpp}"
    );
    assert!(
        !cpp.contains("= (uint64_t)harc_rt::random::harc_prefer_dist("),
        "no signed dist preference may be relabelled uint64_t:\n{cpp}"
    );
}

/// spec §4.4 (D13): a queued randomize captures scalar locals at the call,
/// including into a solver vector wider than 64 bits (a `uint<100>` field):
/// unsigned and bool snapshots through the `uint64_t` overload, signed ones
/// sign-extended from their declared width.
#[test]
fn queued_randomize_captures_scalar_snapshots() {
    let cpp = emit_cpp_src(
        r#"transaction T
    b : bool
    u : uint<8>
    s : sint<16>
    w : uint<100>
end transaction T
test Q
    let dut : Top
    run
        let flag : bool = true
        let lo : uint<8> = 10
        let neg : sint<16> = -300
        let r : T
        randomize(r) with
            r.b == flag
            r.u == lo
            r.s == neg
        end randomize
    end run
end test Q
"#,
    );
    assert!(
        cpp.contains("harc_rt::random::HarcSolveMode::Queued"),
        "the call stays queued:\n{cpp}"
    );
    assert!(
        cpp.matches("harc_z3_bv_value(_ctx, (uint64_t)(").count() >= 2,
        "bool and unsigned snapshots must use the uint64_t overload:\n{cpp}"
    );
    assert!(
        cpp.contains("harc_z3_bv_signed_value(_ctx, ") && cpp.contains(", 16, 100)"),
        "a sint<16> snapshot must sign-extend from 16 bits:\n{cpp}"
    );
}

/// spec §4.4: scalar fields of a local record `let` are snapshots too, so a
/// queued randomize captures them at the call (here into a solver vector
/// wider than 64 bits). DUT state is still live and stays rejected.
#[test]
fn queued_randomize_captures_record_let_fields_but_not_dut_state() {
    let cpp = emit_cpp_src(
        r#"transaction T
    b : bool
    u : uint<8>
    s : sint<16>
    w : uint<100>
end transaction T
test Q
    let dut : Top
    run
        let other : T
        other.b = true
        other.u = 9
        other.s = -300
        let r : T
        randomize(r) with
            r.b == other.b
            r.u == other.u
            r.s == other.s
        end randomize
        randomize(r) with
            other.b
        end randomize
    end run
end test Q
"#,
    );
    assert!(
        cpp.contains("harc_rt::random::HarcSolveMode::Queued")
            && !cpp.contains("HarcSolveMode::Blocking"),
        "the calls stay queued:\n{cpp}"
    );
    assert!(
        cpp.contains("_z_b == harc_z3_bv_value(_ctx, (uint64_t)(other.b), 100)")
            && cpp.contains("_z_u == harc_z3_bv_value(_ctx, (uint64_t)(other.u), 100)")
            && cpp.contains("_z_s == harc_z3_bv_signed_value(_ctx, other.s, 16, 100)")
            && cpp.contains("_ctx.bool_val((bool)(other.b))"),
        "record-let fields must be captured as snapshots:\n{cpp}"
    );

    let dut_src = r#"transaction T
    u : uint<8>
end transaction T
test Q
    let dut : Top
    run
        let r : T
        randomize(r) with
            r.u == dut.count_out
        end randomize
    end run
end test Q
"#;
    let merged = merged_src(dut_src);
    let prog = lower::lower_program(&merged).expect("lowers");
    let err = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default())
        .expect_err("queued randomize over live DUT state must be rejected");
    assert!(
        err.to_string()
            .contains("queued randomize(T) constraint references runtime state `dut.count_out`"),
        "{err}"
    );
}
