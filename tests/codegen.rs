//! Codegen tests. The full end-to-end harc-sim → arch-sim run isn't covered
//! here (it depends on the sibling arch-com checkout being buildable); the
//! `harc sim` invocation in `examples/`-driven scripts validates that.
//! Here we just snapshot the C++ that comes out of `cpp_tb::emit`.

use harc::codegen::{cpp_tb, merge};
use harc::parser::parse_source;

// Snapshot test for the all-in-one counter TB form lives in
// `split_test_via_extend_round_trips_to_same_cpp`, which exercises the
// split-file form and locks the same emitted C++ via insta.

#[test]
fn missing_test_is_a_clean_error() {
    let parsed = parse_source("transaction T\n  addr : uint<32>\nend transaction T").unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(err.0.contains("no `test` declaration"));
}

#[test]
fn split_test_via_extend_round_trips_to_same_cpp() {
    // The split-file form (base + extend test) should produce the same C++
    // as the all-in-one form. Snapshot equality is the discipline.
    let base = include_str!("fixtures/counter_test.harc");
    let sim = include_str!("fixtures/counter_test_sim.harc");
    let parsed = vec![
        parse_source(base).expect("base parse"),
        parse_source(sim).expect("sim extend parse"),
    ];
    let merged = merge::merge_for_sim(&parsed, None).expect("merge");
    let cpp = cpp_tb::emit(&merged).expect("emit");
    insta::assert_snapshot!("counter_tb_cpp", cpp);
}

#[test]
fn extend_test_with_no_base_errors_clearly() {
    let only_extend = parse_source(
        "extend Missing\n  scope sim\n    run\n    end run\n  end scope sim\nend extend Missing",
    ).unwrap();
    let err = merge::merge_for_sim(&[only_extend], None).unwrap_err();
    assert!(err.contains("no matching base") && err.contains("Missing"));
}

#[test]
fn multiple_tests_require_explicit_pick() {
    let f = parse_source(
        r#"test A
    let dut : X
end test A
test B
    let dut : Y
end test B
"#,
    ).unwrap();
    let err = merge::merge_for_sim(&[f], None).unwrap_err();
    assert!(err.contains("multiple tests"));
}

#[test]
fn missing_dut_let_is_a_clean_error() {
    let parsed = parse_source(
        r#"test T
    scope sim
        run
        end run
    end scope sim
end test T"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(err.0.contains("let dut"));
}

/// Hex literals wider than 64 bits (>16 hex digits) lower to a
/// composite `_harc_u128` shifted-OR expression so they fit C++'s
/// integer-literal grammar and flow through `harc_assign` /
/// `harc_read` at full 128-bit precision. Mirrors arch-com's
/// `_arch_u128` model (arch-com src/sim_codegen/mod.rs:767).
#[test]
fn wide_hex_literal_lowers_to_harc_u128_composite() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    scope sim
        run
            dut.x = 0x000102030405060708090a0b0c0d0e0f
            assert dut.y == 0x66e94bd4ef8a2c3b884cfa59ca342b2e
                else fail("nope")
        end run
    end scope sim
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // The 128-bit literal `0x000102030405060708090a0b0c0d0e0f` should
    // emit as `((_harc_u128)0x0001020304050607ULL << 64) |
    //         (_harc_u128)0x08090a0b0c0d0e0fULL`.
    assert!(
        cpp.contains("(_harc_u128)0x0001020304050607ULL << 64") &&
        cpp.contains("(_harc_u128)0x08090a0b0c0d0e0fULL"),
        "expected composite _harc_u128 lowering for the assigned literal:\n{}", cpp,
    );
    assert!(
        cpp.contains("(_harc_u128)0x66e94bd4ef8a2c3bULL << 64") &&
        cpp.contains("(_harc_u128)0x884cfa59ca342b2eULL"),
        "expected composite _harc_u128 lowering for the compared literal:\n{}", cpp,
    );

    // Narrow literals (<= 16 hex digits) stay as plain hex —
    // no composite, no _harc_u128 cast.
    assert!(!cpp.contains("(_harc_u128)0xDEADBEEF") &&
            !cpp.contains("(_harc_u128)0xdeadbeef"),
        "narrow hex shouldn't be wrapped:\n{}", cpp);
}

/// `wait N cycles` matches Verilog's `@(posedge clk)` semantic: values
/// set in the segment BEFORE the wait are sampled at the next posedge.
/// To honor this — including for the FIRST segment (set during
/// `bootstrap()` before the loop) — the emitted main loop must:
///
/// 1. Do an initial `dut->eval()` with `clk=0` before the loop, so
///    bootstrap's combinational outputs settle without advancing time.
/// 2. Per loop iteration, do the posedge FIRST (clk 0→1, eval), then
///    `sched.tick()` (advance run coroutine for next cycle's inputs),
///    then the falling edge (clk 1→0, eval) for comb resettle.
///
/// Otherwise — if `tick()` happened first as it did pre-fix — the first
/// iteration's tick would decrement the bootstrap slot's WaitCycles to
/// 0 and run the next segment immediately, overwriting the bootstrap
/// segment's outputs before any posedge could sample them.
#[test]
fn main_loop_settles_comb_before_first_posedge_then_posedge_before_tick() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    scope sim
        run
            dut.x = 1
            wait 1 cycle
            dut.x = 2
            wait 1 cycle
        end run
    end scope sim
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // Initial comb settle: `dut->clk = 0; dut->eval();` BEFORE the loop
    // opens. There must be NO `clk = 1; eval();` between bootstrap and
    // the loop (no posedge before loop).
    let bootstrap_pos = cpp.find("sched.bootstrap()")
        .expect("expected sched.bootstrap() call");
    let loop_pos = cpp.find("while (_run_slot.kind != harc_rt::WaitKind::Done")
        .expect("expected main run loop");
    assert!(bootstrap_pos < loop_pos);
    let between = &cpp[bootstrap_pos..loop_pos];
    assert!(between.contains("dut->clk = 0; dut->eval();"),
        "expected initial `dut->clk = 0; dut->eval();` between bootstrap and loop:\n{}", between);
    assert!(!between.contains("dut->clk = 1; dut->eval();"),
        "no posedge should appear between bootstrap and loop:\n{}", between);

    // Inside the loop body, the order must be: clk=1 eval (posedge)
    // FIRST, then sched.tick(), then clk=0 eval (falling).
    let loop_body_end = cpp[loop_pos..].find("\n    }\n").map(|p| loop_pos + p)
        .expect("expected loop close");
    let body = &cpp[loop_pos..loop_body_end];
    let posedge_pos = body.find("dut->clk = 1; dut->eval();")
        .expect("expected posedge inside loop");
    let tick_pos = body.find("sched.tick();")
        .expect("expected sched.tick() inside loop");
    let falling_pos = body.find("dut->clk = 0; dut->eval();")
        .expect("expected falling edge inside loop");
    assert!(posedge_pos < tick_pos && tick_pos < falling_pos,
        "expected loop order: posedge → tick → falling. \
         got posedge@{posedge_pos}, tick@{tick_pos}, falling@{falling_pos}\n\
         body:\n{}", body);
}

/// `dut.<signal> = <expr>` and `dut.<signal>` accesses lower through
/// `harc_rt::harc_assign(...)` and `harc_rt::harc_read(...)` so wide
/// signals (Verilator's `VlWide<N>` for >64-bit ports) work without
/// the test author having to think about word-level decomposition.
/// Narrow signals see the same wrapper, which `if constexpr`-folds
/// to a plain assignment / cast.
#[test]
fn pointer_rooted_signal_access_uses_wide_helpers() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    scope sim
        run
            dut.wide_in = 305419896
            dut.narrow_in = 5
            assert dut.wide_out == 305419896
                else fail("wide read")
            let v = dut.wide_out + 1
        end run
    end scope sim
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // Writes lower as harc_assign(...).
    assert!(cpp.contains("harc_rt::harc_assign(dut->wide_in,"),
        "expected `harc_rt::harc_assign(dut->wide_in, ...)` in:\n{}", cpp);
    assert!(cpp.contains("harc_rt::harc_assign(dut->narrow_in,"),
        "expected `harc_rt::harc_assign(dut->narrow_in, ...)` in:\n{}", cpp);

    // Reads lower as harc_read(...).
    assert!(cpp.contains("harc_rt::harc_read(dut->wide_out)"),
        "expected `harc_rt::harc_read(dut->wide_out)` in:\n{}", cpp);

    // L-value path must NOT wrap with harc_read — the assignment
    // target stays a plain L-value reference passed to harc_assign.
    // Spot-check: the assignment line should contain the field as
    // an L-value, not `harc_read(dut->wide_in)`.
    let assign_line = cpp.lines()
        .find(|l| l.contains("harc_assign(dut->wide_in,"))
        .expect("expected assign line");
    assert!(!assign_line.contains("harc_read(dut->wide_in"),
        "L-value position must not be wrapped with harc_read:\n{}", assign_line);
}

/// `const NAME : Ty = expr` lowers to a file-scope `static constexpr`
/// so it's available inside `main()`, hookable lambdas, tseq lambdas,
/// and on-handler closures.
#[test]
fn top_level_const_lowers_to_static_constexpr() {
    let parsed = parse_source(
        r#"const MSHR_SIZE : uint<32> = 32
const HALF      : uint<32> = MSHR_SIZE / 2
test T
    let dut : DummyDut
    scope sim
        run
            assert MSHR_SIZE == 32
                else fail("MSHR_SIZE wrong")
            assert HALF == 16
                else fail("HALF wrong")
        end run
    end scope sim
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // Both consts emitted at file scope, before main().
    assert!(cpp.contains("static constexpr uint64_t MSHR_SIZE = 32;"),
        "expected `static constexpr uint64_t MSHR_SIZE = 32;` in:\n{}", cpp);
    assert!(cpp.contains("static constexpr uint64_t HALF ="),
        "expected `static constexpr uint64_t HALF` in:\n{}", cpp);

    // Order matters — both should appear BEFORE `int main`.
    let main_pos = cpp.find("int main").expect("expected `int main` in output");
    let mshr_pos = cpp.find("static constexpr uint64_t MSHR_SIZE").unwrap();
    let half_pos = cpp.find("static constexpr uint64_t HALF").unwrap();
    assert!(mshr_pos < main_pos, "MSHR_SIZE should be emitted before main()");
    assert!(half_pos < main_pos, "HALF should be emitted before main()");
}

/// Spec §7.7: `log(error, ...)` increments the failure counter, and
/// `log(fatal, ...)` additionally sets a flag so the main simulation
/// loop aborts at end of the current cycle. `info` / `warn` / `debug`
/// have no test-result effect.
#[test]
fn log_severity_test_result_semantics() {
    // We need a `let dut : SomeModule` to satisfy the emit prelude;
    // the actual lowering doesn't depend on it.
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    scope sim
        run
            log(info,  "info: no effect")
            log(warn,  "warn: no effect")
            log(debug, "debug: no effect")
            log(error, "error: should bump counter")
            log(fatal, "fatal: should abort")
        end run
    end scope sim
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // Sanity: the test-result flag is declared.
    assert!(cpp.contains("bool _fatal = false;"),
        "expected `_fatal` flag declaration in main()");

    // The main simulation loop guard checks _fatal so the test instance
    // exits at end of current cycle when fatal is set.
    assert!(cpp.contains("&& !_fatal"),
        "expected main loop to check `!_fatal`");

    // `log(info|warn|debug, ...)` must not bump errors. We grep for
    // `sim_log_line(\"INFO\"`, ... and verify the line that follows is
    // NOT `errors++`. Easier: count `errors++;` occurrences and confirm
    // exactly the right number.
    //
    // Each line starts a printf-call. After the closing `);`, the next
    // statement is either nothing (info/warn/debug) or `errors++;`
    // (error) or `errors++; _fatal = true;` (fatal).
    let errors_inc_count = cpp.matches("errors++;").count();
    // From-source: 2 (one for log(error), one for log(fatal)).
    // Plus existing `errors++;` from assert/fail paths: 0 here (no
    // asserts in the fixture).
    assert_eq!(errors_inc_count, 2,
        "expected exactly 2 `errors++;` lines (one for ERROR, one for FATAL); \
         got {} in:\n{}", errors_inc_count, cpp);

    // `log(fatal, ...)` additionally sets `_fatal = true`.
    assert!(cpp.contains("_fatal = true;"),
        "expected `_fatal = true;` in FATAL lowering");
}
