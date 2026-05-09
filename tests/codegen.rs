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
