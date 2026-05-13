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
fn impl_with_no_base_test_errors_clearly() {
    let only_impl = parse_source(
        "impl sim for Missing\n    run\n    end run\nend impl Missing",
    ).unwrap();
    // `merge_for_sim` requires a base test; an `impl` referencing
    // an unknown test name surfaces at codegen time, not merge time.
    let err = merge::merge_for_sim(&[only_impl], None).unwrap_err();
    assert!(err.contains("no `test` declaration"),
        "expected 'no `test` declaration' error, got: {}", err);
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
end test T

impl sim for T
    run
    end run
end impl T"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(err.0.contains("let dut"));
}

/// `${expr:WWx}` and `${expr:WWX}` format specs with WW > 16 route
/// through the `HarcHexBuf128` runtime helper (printf `%s`) so the
/// full ≤128-bit value prints. The current-default narrow path
/// `(long long)(...)` would truncate to the lower 64 bits — fine
/// for register dumps that fit in a uint64, useless for AES blocks.
/// Specs with width ≤ 16 stay on the legacy `%llx` / `(long long)`
/// path.
#[test]
fn wide_hex_format_spec_routes_through_hexbuf128() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
end test T

impl sim for T
    run
        // Wide-hex spec — width 32 hex digits = 128 bits.
        log(info, "ct=0x${dut.text_out:032x}")
        // Narrow-hex spec — width 8 hex digits = stays on long long.
        log(info, "narrow=0x${dut.x:08x}")
        // Uppercase wide spec.
        log(info, "CT=0x${dut.text_out:032X}")
    end run
end impl T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // Wide-hex lowercase — printf `%s` + HarcHexBuf128 with upper=false.
    assert!(cpp.contains("\"ct=0x%s\""),
        "expected `%s` format token for wide-hex spec:\n{}", cpp);
    assert!(cpp.contains("(const char*)harc_rt::HarcHexBuf128(harc_rt::harc_read(dut->text_out), 32, false)"),
        "expected HarcHexBuf128 lowering for `:032x`:\n{}", cpp);

    // Wide-hex uppercase — same shape, upper=true.
    assert!(cpp.contains("(const char*)harc_rt::HarcHexBuf128(harc_rt::harc_read(dut->text_out), 32, true)"),
        "expected HarcHexBuf128 lowering for `:032X`:\n{}", cpp);

    // Narrow-hex stays on the legacy path.
    assert!(cpp.contains("\"narrow=0x%08llx\""),
        "expected `%08llx` for narrow `:08x` spec:\n{}", cpp);
    assert!(!cpp.contains("HarcHexBuf128(harc_rt::harc_read(dut->x)") &&
            !cpp.contains("HarcHexBuf128(dut->x"),
        "narrow-hex spec must NOT route through HarcHexBuf128:\n{}", cpp);
}

/// Hex literals wider than 128 bits (>32 hex digits) overflow
/// `_harc_u128` and route through the `harc_assign_words` /
/// `harc_eq_words` runtime helpers — taking an
/// `std::initializer_list<uint32_t>` of the literal split into
/// LSB-first 32-bit words. This is what makes wide DATA buses
/// (AXI 256/512/1024-bit, vector lanes, etc.) drivable as
/// whole-signal hex literals.
#[test]
fn wide_hex_literal_routes_assign_and_eq_through_word_helpers() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
end test T

impl sim for T
    run
        // 256-bit literal — 64 hex digits — must split into 8
        // words and route through harc_assign_words for the write
        // and harc_eq_words for the compare.
        dut.data = 0x0123456789abcdef_fedcba9876543210_aabbccddeeff0011_2233445566778899
        assert dut.data == 0xffffffffffffffff_0000000000000000_aabbccddeeff0011_2233445566778899
            else fail("nope")
    end run
end impl T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // Assignment: harc_assign_words with 8 LSB-first words.
    assert!(cpp.contains("harc_rt::harc_assign_words(dut->data, {0x66778899u, 0x22334455u, 0xeeff0011u, 0xaabbccddu, 0x76543210u, 0xfedcba98u, 0x89abcdefu, 0x01234567u})"),
        "expected harc_assign_words call with LSB-first words:\n{}", cpp);

    // Equality: harc_eq_words with 8 LSB-first words from the
    // compared literal.
    assert!(cpp.contains("harc_rt::harc_eq_words(dut->data, {0x66778899u, 0x22334455u, 0xeeff0011u, 0xaabbccddu, 0x00000000u, 0x00000000u, 0xffffffffu, 0xffffffffu})"),
        "expected harc_eq_words call with LSB-first words:\n{}", cpp);
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
end test T

impl sim for T
    run
        dut.x = 0x000102030405060708090a0b0c0d0e0f
        assert dut.y == 0x66e94bd4ef8a2c3b884cfa59ca342b2e
            else fail("nope")
    end run
end impl T"#,
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
end test T

impl sim for T
    run
        dut.x = 1
        wait 1 cycle
        dut.x = 2
        wait 1 cycle
    end run
end impl T"#,
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
end test T

impl sim for T
    run
        dut.wide_in = 305419896
        dut.narrow_in = 5
        assert dut.wide_out == 305419896
            else fail("wide read")
        let v = dut.wide_out + 1
    end run
end impl T"#,
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
end test T

impl sim for T
    run
        assert MSHR_SIZE == 32
            else fail("MSHR_SIZE wrong")
        assert HALF == 16
            else fail("HALF wrong")
    end run
end impl T"#,
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
end test T

impl sim for T
    run
        log(info,  "info: no effect")
        log(warn,  "warn: no effect")
        log(debug, "debug: no effect")
        log(error, "error: should bump counter")
        log(fatal, "fatal: should abort")
    end run
end impl T"#,
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

/// Custom `phase <name> ... end phase <name>` blocks inside an
/// `impl sim for X` lower as `[&]`-capturing void-returning lambdas
/// at main() scope, callable by name from `run` (or from each other).
/// Spec §7.2: phases are pure code-organization helpers — not
/// auto-fired by the runtime, only invoked by explicit user calls.
#[test]
fn impl_sim_custom_phase_lowers_as_named_lambda() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
end test T

impl sim for T
    phase warmup
        log(info, "warmup phase")
    end phase warmup

    run
        warmup()
        wait 1 cycle
    end run
end impl T"#,
    ).unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // Phase emits as `auto warmup = [&]() -> void { ... };`
    assert!(cpp.contains("auto warmup = [&]() -> void {"),
        "expected `auto warmup = [&]() -> void {{` in:\n{cpp}");
    // Body of the phase contains the log line.
    assert!(cpp.contains("warmup phase"),
        "phase body should contain its log message; got:\n{cpp}");
    // Run-coroutine body invokes the phase by name.
    assert!(cpp.contains("warmup();"),
        "run body should call `warmup();` to invoke the phase; got:\n{cpp}");

    // Phase lambda emits BEFORE the run-coroutine bootstrap so the
    // capture-by-reference closure is in scope when run calls it.
    let phase_pos = cpp.find("auto warmup = [&]()").unwrap();
    let bootstrap_pos = cpp.find("sched.bootstrap()").unwrap();
    assert!(phase_pos < bootstrap_pos,
        "custom phase lambda must be emitted before sched.bootstrap()");
}

/// v0 only emits codegen for `impl sim for ...`. A test with only
/// non-sim impls (e.g. `impl emu for ...`) errors clearly rather than
/// silently producing an empty binary — emu transport is post-v0.
#[test]
fn impl_emu_only_test_errors_clearly() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
end test T

impl emu for T
    run
        log(info, "emu run body")
    end run
end impl T"#,
    ).unwrap();

    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(err.0.contains("only non-sim impls") && err.0.contains("emu"),
        "expected clear error mentioning non-sim impls; got: {}", err.0);
}

/// `expr as Type` (postfix cast, same shape as arch-com's grammar)
/// emits a C++ cast `((<c_type>)(<inner>))` when the target is a
/// builtin numeric type. Critical for width-widening cases like
/// `1 as uint<32> << 31` — without the cast, C++'s `int` literal
/// shift-by-31 hits sign-bit UB; with the cast, the shift operates
/// on `uint64_t` and is well-defined.
#[test]
fn cast_to_builtin_emits_cpp_cast() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
end test T

impl sim for T
    run
        // Walk-1 pattern that needs cast-widened source: `(1 as
        // uint<32>) << 31` would otherwise be `1 << 31` against a
        // 32-bit int literal — UB in C++.
        let mask : uint<32> = (1 as uint<32>) << 31
        dut.X = mask
    end run
end impl T"#,
    ).unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // Cast emits as `((uint64_t)(1))` — HARC's c_type_for maps
    // uint<32> to uint64_t (the C++ widening covers all ≤64-bit
    // unsigned ints uniformly).
    assert!(cpp.contains("((uint64_t)(1))"),
        "expected `((uint64_t)(1))` from `1 as uint<32>`; got:\n{cpp}");
    // And the shift uses that cast result, not a bare `1 << 31`.
    assert!(cpp.contains("((uint64_t)(1))) << 31") ||
            cpp.contains("((uint64_t)(1)) << 31"),
        "expected shift to operate on the cast result; got:\n{cpp}");
}

/// Standalone `fail("...")` lowers to the same emission as the
/// failure arm of an `assert ... else fail(...)`: a `sim_log_line`
/// + `errors++;`. Without the surrounding `if (!cond)` guard, it
/// is an unconditional failure — useful when the failure trigger
/// is control-flow-structural rather than a single boolean
/// predicate.
#[test]
fn standalone_fail_emits_sim_log_and_errors_bump() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
end test T

impl sim for T
    run
        for i in 0 .. 4
            if i == 3
                fail("loop reached unreachable branch at i=${i}")
            end if
        end for
    end run
end impl T"#,
    ).unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // The standalone fail emits a sim_log_line + errors++ inline,
    // no surrounding `if (!...)` guard.
    assert!(cpp.contains("sim_log_line(\"FAIL\", \"loop reached unreachable branch at i=%lld\""),
        "expected sim_log_line(\"FAIL\", ...) for standalone fail; got:\n{cpp}");
    assert!(cpp.contains("errors++"),
        "expected `errors++;` after standalone fail; got:\n{cpp}");
}

/// Every component struct (transactor/agent/env/scoreboard/sequencer)
/// gets two auto-injected heartbeat fields — `_last_in_cycle` and
/// `_last_out_cycle` — used by the built-in `idle(N)` / `idle_in(N)` /
/// `idle_out(N)` predicates. These fields default to 0 and are bumped
/// at every site the framework knows an in/out has just happened:
/// `on <event>` handler body entry, `emit ev(arg)`, `bus.<ch>.send`,
/// `bus.<ch>.recv`. This pins the lowering shape (spec §7.x).
#[test]
fn component_heartbeat_fields_and_bump_sites() {
    let parsed = parse_source(
        r#"transaction T
    addr  : uint<8>
    value : uint<32>
end transaction T

agent Producer
    out : event<T>
    in_ev : event<T>

    on in_ev(t)
        emit out(t)
    end on
end agent Producer

test HeartbeatTest
    let dut : DummyDut
    let prod : Producer
end test HeartbeatTest

impl sim for HeartbeatTest
    run
        let stuck = prod.idle(50)
        let stuck_in = prod.idle_in(10)
        let stuck_out = prod.idle_out(20)
        wait 1 cycle
    end run
end impl HeartbeatTest"#,
    ).unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // 1. Heartbeat fields appear on the component struct.
    assert!(cpp.contains("struct Producer {"),
        "Producer struct should be emitted; got:\n{cpp}");
    assert!(cpp.contains("uint64_t _last_in_cycle = 0;"),
        "expected `_last_in_cycle` field on component; got:\n{cpp}");
    assert!(cpp.contains("uint64_t _last_out_cycle = 0;"),
        "expected `_last_out_cycle` field on component; got:\n{cpp}");

    // 2. The `on in_ev(t)` handler body bumps `_last_in_cycle` at entry.
    assert!(cpp.contains("prod._last_in_cycle = (uint64_t)cycle_count;"),
        "on-handler body should bump _last_in_cycle on entry; got:\n{cpp}");

    // 3. The `emit out(t)` inside the handler bumps `_last_out_cycle`.
    assert!(cpp.contains("prod._last_out_cycle = (uint64_t)cycle_count;"),
        "emit inside component body should bump _last_out_cycle; got:\n{cpp}");

    // 4. `prod.idle(N)` lowers to a conjunction over both cycle deltas.
    assert!(
        cpp.contains("((uint64_t)cycle_count - prod._last_in_cycle) >= (uint64_t)(50))")
            && cpp.contains("((uint64_t)cycle_count - prod._last_out_cycle) >= (uint64_t)(50))"),
        "idle(N) should lower to (in_delta >= N) && (out_delta >= N); got:\n{cpp}",
    );
    // 5. `prod.idle_in(N)` lowers to in-delta only.
    assert!(cpp.contains("((uint64_t)cycle_count - prod._last_in_cycle) >= (uint64_t)(10))"),
        "idle_in(N) should lower to (in_delta >= N); got:\n{cpp}");
    // 6. `prod.idle_out(N)` lowers to out-delta only.
    assert!(cpp.contains("((uint64_t)cycle_count - prod._last_out_cycle) >= (uint64_t)(20))"),
        "idle_out(N) should lower to (out_delta >= N); got:\n{cpp}");
}

/// `bus.<ch>.send` and `bus.<ch>.recv` inside a component body bump
/// `_last_out_cycle` / `_last_in_cycle` respectively. Bus calls in
/// free test-run code don't attribute to any component instance and
/// emit no bump.
#[test]
fn bus_send_recv_bump_component_heartbeat() {
    // Setup mirrors `axilite_seqdrv_test.harc` — a bound active
    // transactor whose on-handler uses bus.send. The handshake spin
    // loop ends with a bump to `_last_out_cycle` on the driver
    // instance.
    let parsed = parse_source(
        r#"transaction RegOp
    addr  : uint<8>
    value : uint<32>
end transaction RegOp

bus BusLite
    handshake_channel w: send kind: valid_ready
        addr : uint<8>
        data : uint<32>
    end handshake_channel w
end bus BusLite

transactor SeqXactor bound to BusLite
    dut : DummyDut

    when active
        req : in event<RegOp>
        on req(t)
            bus.w.send(t.addr, t.value)
        end on
    end when
end transactor SeqXactor

test BusHeartbeatTest
    let dut : DummyDut
    let axil : BusLite = bind dut
    let drv : SeqXactor active = bind axil
end test BusHeartbeatTest

impl sim for BusHeartbeatTest
    run
        wait 1 cycle
    end run
end impl BusHeartbeatTest"#,
    ).unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // Bound-driver actor pops a transaction → bumps _last_in_cycle.
    assert!(cpp.contains("drv._last_in_cycle = (uint64_t)cycle_count;"),
        "bound-driver actor pop should bump _last_in_cycle; got:\n{cpp}");
    // bus.w.send inside the actor body → bumps _last_out_cycle.
    assert!(cpp.contains("drv._last_out_cycle = (uint64_t)cycle_count;"),
        "bus.<ch>.send inside component body should bump _last_out_cycle; got:\n{cpp}");
}

/// A user-defined `hookable idle(n)` on a component or transactor
/// wins over the built-in `idle(N)` predicate. The call lowers to
/// `<Type>_idle(obj, n)` as a regular hookable dispatch, NOT to the
/// boolean heartbeat-delta predicate. This is what lets pre-existing
/// fixtures keep their custom `idle()` semantics (e.g.
/// `buf_mgr_test.harc`'s `hookable idle(n)` that holds bus valids
/// low for `n` cycles).
#[test]
fn user_hookable_idle_wins_over_builtin_predicate() {
    let parsed = parse_source(
        r#"transactor Xact
    dut : DummyDut

    hookable idle(n: uint<32>)
        for _ in 0 .. n
            wait 1 cycle
        end for
    end idle
end transactor Xact

test UserIdleTest
    let dut : DummyDut
    let xact : Xact passive
end test UserIdleTest

impl sim for UserIdleTest
    run
        xact.idle(4)
    end run
end impl UserIdleTest"#,
    ).unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // The call should dispatch to the user's hookable, NOT to the
    // built-in heartbeat predicate.
    assert!(cpp.contains("Xact_idle(xact, 4)"),
        "user `hookable idle(n)` should be called via the standard dispatcher; got:\n{cpp}");
    // Specifically, the call's lowering should NOT contain the
    // built-in delta-comparison shape.
    assert!(!cpp.contains("cycle_count - xact._last_in_cycle"),
        "built-in predicate should NOT shadow user's `hookable idle`; got:\n{cpp}");
}

/// `idle()` predicate on a nested sub-component path (e.g.
/// `env.drv.idle(N)`) walks the type chain to confirm the leaf is a
/// component-typed binding. Mirrors `resolve_component_method_call`'s
/// chain walk so the predicate works wherever method dispatch works.
#[test]
fn idle_predicate_resolves_through_nested_component_path() {
    let parsed = parse_source(
        r#"agent Worker
    in_ev : event<int>
end agent Worker

env TopEnv
    w : Worker
end env TopEnv

test NestedTest
    let dut : DummyDut
    let top : TopEnv
end test NestedTest

impl sim for NestedTest
    run
        let hung = top.w.idle(100)
        wait 1 cycle
    end run
end impl NestedTest"#,
    ).unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(cpp.contains("top.w._last_in_cycle") && cpp.contains("top.w._last_out_cycle"),
        "nested idle() should walk through the env's field to the agent's heartbeat fields; got:\n{cpp}");
}

/// `wait until <expr>` with no timeout lowers to a direct
/// `co_await harc_rt::wait_until(_slot, [&]{ return <expr>; });` —
/// the most efficient shape (the scheduler evaluates the predicate
/// once per cycle and only resumes when true). Pins the lowering
/// shape (spec §7.9).
#[test]
fn wait_until_no_timeout_lowers_to_coroutine_wait_until() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
end test T

impl sim for T
    run
        wait until dut.ready
    end run
end impl T"#,
    ).unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("co_await harc_rt::wait_until(_slot, [&]{ return")
            && cpp.contains("dut->ready"),
        "untimed wait until should lower to coroutine wait_until + predicate lambda; got:\n{cpp}",
    );
}

/// `wait until all of <e1>, <e2> timeout N cycles fail("...")` lowers
/// to a bounded polling loop with per-sub-predicate diagnostics on
/// timeout. The diagnostic identifies each sub-predicate by its
/// pretty-printed source text (so logs show `dut.ready` rather than
/// a synthetic index).
#[test]
fn wait_until_all_of_with_timeout_emits_per_predicate_diagnostic() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
end test T

impl sim for T
    run
        wait until all of dut.ready, dut.empty
            timeout 500 cycles fail("did not quiesce")
    end run
end impl T"#,
    ).unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // Loop bounded by `_wu_budget` against `cycle_count`.
    assert!(cpp.contains("int64_t _wu_budget = (int64_t)(500);"),
        "expected `_wu_budget` initialized from the timeout expr; got:\n{cpp}");
    assert!(cpp.contains("_wu_start = (int64_t)cycle_count;"),
        "expected `_wu_start` snapshot from cycle_count; got:\n{cpp}");
    // Per-sub-predicate breakdown: one line per condition still false.
    assert!(cpp.contains("not yet true: dut.ready"),
        "expected per-predicate diagnostic mentioning `dut.ready`; got:\n{cpp}");
    assert!(cpp.contains("not yet true: dut.empty"),
        "expected per-predicate diagnostic mentioning `dut.empty`; got:\n{cpp}");
    // User-supplied header line.
    assert!(cpp.contains("did not quiesce"),
        "expected the user-supplied fail() message in the timeout log; got:\n{cpp}");
    // Errors counter bumps.
    assert!(cpp.contains("errors++;"),
        "expected `errors++;` on timeout; got:\n{cpp}");
}

/// `wait until any of <e1>, <e2>` on timeout reports "none of" with
/// the joined source list (we can't say which one was supposed to
/// fire — by definition none did).
#[test]
fn wait_until_any_of_timeout_reports_none_of_list() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
end test T

impl sim for T
    run
        wait until any of dut.error, dut.done
            timeout 200 cycles fail("expected error or done")
    end run
end impl T"#,
    ).unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // The overall cond is `||`-joined — emit_expr wraps DUT signals
    // in `harc_rt::harc_read(...)` so we test the join-shape with a
    // tolerant match rather than exact lexical comparison.
    assert!(cpp.contains("dut->error)) || (harc_rt::harc_read(dut->done"),
        "any-of overall predicate should be ||-joined; got:\n{cpp}");
    // Diagnostic lists every sub-predicate (none fired).
    assert!(cpp.contains("none of: dut.error, dut.done"),
        "expected `none of:` listing every sub-predicate; got:\n{cpp}");
}

/// `wait until <expr>` with no timeout still works inside a sync
/// context — e.g. inside a hookable method body — and lowers to a
/// `while (!cond) tick();` synchronous polling loop instead of
/// `co_await`. (Coroutines aren't available inside hookable bodies
/// because they run between coroutine yields, not as their own
/// coroutines.)
#[test]
fn wait_until_in_sync_context_uses_tick_loop() {
    let parsed = parse_source(
        r#"transactor X
    dut : DummyDut
    hookable wait_for_ready()
        wait until dut.ready
    end wait_for_ready
end transactor X

test T
    let dut : DummyDut
    let xact : X passive
end test T

impl sim for T
    run
        xact.wait_for_ready()
    end run
end impl T"#,
    ).unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // Inside a hookable, wait until must lower synchronously.
    assert!(cpp.contains("while (!(")
            && cpp.contains(")) tick();"),
        "wait until in sync context should be `while (!cond) tick();`; got:\n{cpp}");
}

/// Smoke-sweep every fixture under `tests/fixtures/` through
/// `cpp_tb::emit`. Fixtures missing a sibling `_sim.harc` half are
/// auto-paired (e.g. `counter_test.harc` + `counter_test_sim.harc`);
/// the rest go through `emit` standalone. Anything that emits without
/// error must continue to emit without error after the heartbeat-
/// foundation changes — this catches any case where the new bump
/// sites accidentally reference an out-of-scope instance.
///
/// Failures are reported as a single aggregated panic at the end so
/// one bad fixture doesn't mask issues in others.
#[test]
fn all_fixtures_emit_cleanly() {
    let fixtures = std::path::Path::new("tests/fixtures");
    let mut paths: Vec<_> = std::fs::read_dir(fixtures).unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("harc"))
        .collect();
    paths.sort();

    let mut failures: Vec<String> = Vec::new();
    let mut emitted = 0usize;
    for path in &paths {
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
        // Skip the `_sim.harc` halves — they're picked up via their
        // sibling base file's merge.
        if name.ends_with("_sim.harc") || name.ends_with("_domains.harc") {
            continue;
        }
        let src = std::fs::read_to_string(path).unwrap();
        let parsed = match parse_source(&src) {
            Ok(p) => p,
            Err(e) => {
                failures.push(format!("[parse] {name}: {e:?}"));
                continue;
            }
        };
        // Try sibling sim half if present.
        let sim_sibling = path.with_file_name(
            format!("{}_sim.harc", name.trim_end_matches(".harc")),
        );
        let parsed_units = if sim_sibling.exists() {
            let sim_src = std::fs::read_to_string(&sim_sibling).unwrap();
            match parse_source(&sim_src) {
                Ok(sim) => vec![parsed.clone(), sim],
                Err(_) => vec![parsed.clone()],
            }
        } else {
            vec![parsed.clone()]
        };
        let to_emit = match merge::merge_for_sim(&parsed_units, None) {
            Ok(m) => m,
            Err(_) => parsed.clone(),
        };
        match cpp_tb::emit(&to_emit) {
            Ok(_) => emitted += 1,
            // Fixtures that legitimately error (no test / no sim impl /
            // missing DUT) are skipped silently — those error paths
            // aren't part of what this sweep is checking.
            Err(e) => {
                let msg = e.0;
                // Benign error classes — these fixtures depend on
                // external declarations (`use BusAxiLite` brings in a
                // sibling bus decl, multi-clock fixtures rely on a
                // separate `domain` file) that aren't in scope when
                // emitting the fixture standalone. They're not
                // heartbeat-foundation regressions.
                let benign = msg.contains("no `test` declaration")
                    || msg.contains("let dut")
                    || msg.contains("only non-sim impls")
                    || msg.contains("no `impl sim`")
                    || msg.contains("multiple tests")
                    || msg.contains("is not a known bus binding")
                    || msg.contains("no `domain") && msg.contains("declaration was found")
                    || msg.contains("randomize(") && msg.contains("no `transaction");
                if !benign {
                    failures.push(format!("[emit] {name}: {msg}"));
                }
            }
        }
    }
    assert!(failures.is_empty(),
        "fixture sweep: {} emitted, {} failed:\n{}",
        emitted, failures.len(), failures.join("\n"));
    // Sanity: at least a substantial fraction of fixtures should have
    // gone through emit; otherwise the skip filter is too aggressive.
    assert!(emitted >= 20,
        "fixture sweep only emitted {emitted} files — skip filter too aggressive?");
}

/// Casts to non-Builtin types (struct, named) drop to identity at
/// codegen time. The cast is purely a HARC-level type assertion;
/// the C++ representation doesn't change.
#[test]
fn cast_to_named_type_is_identity_in_cpp() {
    let parsed = parse_source(
        r#"struct Pkt
    addr : uint<32>
    data : uint<32>
end struct Pkt

test T
    let dut : DummyDut
end test T

impl sim for T
    run
        let raw : uint<64> = 0xDEAD_BEEF_CAFE_BABE
        let pkt = raw as Pkt
        dut.X = pkt.addr
    end run
end impl T"#,
    ).unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // No `(Pkt)` C++ cast should appear — the user-level cast to a
    // struct is identity at the HARC C++ TB layer (struct field
    // access still uses `.addr`, which works on the underlying value).
    assert!(!cpp.contains("(Pkt)("),
        "expected NO `(Pkt)(...)` C++ cast for struct-targeted `as`; got:\n{cpp}");
}

