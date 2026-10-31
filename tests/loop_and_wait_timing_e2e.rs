//! Native regressions for loop-bound scoping, negative `repeat` counts,
//! `domain` frequency diagnostics, per-cycle checker passes inside
//! wall-clock / clock-qualified waits, and the at-least-one-cycle rule for
//! every `wait until` form (spec §7.4, §7.9, §7.10).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DUT: &str = "tests/dut/top_counter.sv";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn verilator_available(test: &str) -> bool {
    let ok = Command::new("verilator")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        if std::env::var_os("HARC_REQUIRE_VERILATOR").is_some() {
            panic!("verilator required");
        }
        eprintln!("SKIP {test}: verilator not found");
    }
    ok
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("harc_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test directory");
    dir
}

fn sim(dir: &Path, name: &str, source: &str, extra: &[&str]) -> Output {
    sim_on(dir, name, source, &root().join(DUT), "Top", extra)
}

fn sim_on(dir: &Path, name: &str, source: &str, dut: &Path, top: &str, extra: &[&str]) -> Output {
    let fixture = dir.join(format!("{name}.harc"));
    std::fs::write(&fixture, source).expect("write HARC fixture");
    Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(dut)
        .arg(&fixture)
        .args(["--top", top, "--outdir"])
        .arg(dir.join(format!("{name}-out")))
        .args(["--rebuild", "--jobs", "2"])
        .args(extra)
        .env("HARC_NO_LEARN", "1")
        .env("ARCH_NO_LEARN", "1")
        .output()
        .expect("spawn harc sim")
}

fn dump_ir(dir: &Path, name: &str, source: &str) -> Output {
    let fixture = dir.join(format!("{name}.harc"));
    std::fs::write(&fixture, source).expect("write HARC fixture");
    Command::new(env!("CARGO_BIN_EXE_harc"))
        .arg("dump-ir")
        .arg(&fixture)
        .output()
        .expect("spawn harc dump-ir")
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The user `log(info, …)` lines of a passing run, in order, without the
/// `seed=` banner.
fn info_lines(output: &Output) -> Vec<String> {
    let log = text(output);
    assert!(
        output.status.success() && log.contains("ALL TESTS PASSED"),
        "simulation failed:\n{log}"
    );
    any_info_lines(output)
}

/// The user `log(info, …)` lines of a run, passing or not.
fn any_info_lines(output: &Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| l.starts_with("[cycle:") && l.contains(" INFO] ") && !l.contains("seed="))
        .map(|l| l.split(" INFO] ").nth(1).unwrap_or("").to_string())
        .collect()
}

const LOOP_TB: &str = r#"domain D
    freq_mhz: 100
end domain D
test LoopBounds
    let dut : Top
    clock clk = D
    run
        let n : uint<8> = 3
        let a : uint<8> = 0
        for n in 0 .. n
            a = a + 1
        end for
        assert a == 4 else fail("for-hi resolved to the loop variable")
        let neg : sint<8> = -2
        let s : uint<16> = 0
        repeat neg
            s = s + 1
            if s == 1000
                break
            end if
        end repeat
        assert s == 0 else fail("runtime-negative repeat ran its body")
        let three : sint<8> = 3
        let t : uint<16> = 0
        repeat three
            t = t + 1
        end repeat
        assert t == 3 else fail("signed positive repeat count mismatch")
        log(info, "LOOPS a=${a} s=${s} t=${t}")
    end run
end test LoopBounds
"#;

#[test]
fn for_upper_bound_uses_enclosing_scope_and_negative_repeat_runs_zero_times() {
    if !verilator_available("for_upper_bound_uses_enclosing_scope") {
        return;
    }
    let dir = scratch("loop_bounds");
    let out = sim(&dir, "loops", LOOP_TB, &[]);
    assert_eq!(info_lines(&out), vec!["LOOPS a=4 s=0 t=3".to_string()]);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn constant_negative_repeat_count_is_a_compile_error() {
    let dir = scratch("repeat_negative_const");
    let source = r#"const N : sint<8> = -3
domain D
    freq_mhz: 100
end domain D
test NegRepeat
    let dut : Top
    clock clk = D
    run
        let s : uint<16> = 0
        repeat N
            s = s + 1
        end repeat
    end run
end test NegRepeat
"#;
    let out = dump_ir(&dir, "neg_repeat", source);
    let log = text(&out);
    assert!(
        !out.status.success(),
        "negative constant repeat lowered:\n{log}"
    );
    assert!(
        log.contains("`repeat` count is the negative constant -3"),
        "missing negative repeat diagnostic:\n{log}"
    );
    let literal = source
        .replace("const N : sint<8> = -3\n", "")
        .replace("repeat N", "repeat -1");
    let out = dump_ir(&dir, "neg_repeat_literal", &literal);
    let log = text(&out);
    assert!(
        !out.status.success(),
        "negative literal repeat lowered:\n{log}"
    );
    assert!(
        log.contains("`repeat` count is the negative constant -1"),
        "missing negative literal repeat diagnostic:\n{log}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn bad_domain_frequency_names_the_value() {
    let dir = scratch("domain_freq");
    for (name, value) in [("zero", "0"), ("frac", "12.5"), ("huge", "2000000")] {
        let source = format!(
            r#"domain Slow
    freq_mhz: {value}
end domain Slow
test K3
    let dut : Top
    clock clk = Slow
    run
        wait 1 cycle
    end run
end test K3
"#
        );
        let out = dump_ir(&dir, name, &source);
        let log = text(&out);
        assert!(
            !out.status.success(),
            "bad freq_mhz {value} lowered:\n{log}"
        );
        assert!(
            log.contains(&format!("has `freq_mhz: {value}`"))
                && log.contains("must be a")
                && !log.contains("declaration was found"),
            "freq_mhz {value} diagnostic does not name the bad value:\n{log}"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

fn checker_tb(wait: &str) -> String {
    format!(
        r#"domain D
    freq_mhz: 100
end domain D
testbench Tb
    dut : Top
    on 1 cycles
        log(info, "TICK=${{cycle_count}}")
    end on
end testbench Tb
impl T for Tb
    clock clk = D
    run
        dut.rst = 0
        wait 1 cycle
        log(info, "BEFORE=${{cycle_count}}")
        wait {wait}
        log(info, "AFTER=${{cycle_count}}")
        wait 1 cycle
    end run
end impl T
"#
    )
}

/// With a single 100 MHz clock, `wait 4 cycles on clk` and `wait 40ns`
/// started on a cycle boundary are observationally identical to
/// `wait 4 cycles`: the `on 1 cycles` handler fires once for every
/// cycle, in the same order relative to the run body.
#[test]
fn periodic_handlers_fire_every_cycle_inside_wall_and_named_clock_waits() {
    if !verilator_available("periodic_handlers_fire_every_cycle") {
        return;
    }
    let dir = scratch("wait_checker_passes");
    let plain = info_lines(&sim(&dir, "plain", &checker_tb("4 cycles"), &[]));
    let expected: Vec<String> = [
        "BEFORE=1", "TICK=1", "TICK=2", "TICK=3", "TICK=4", "AFTER=5", "TICK=5", "TICK=6",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(plain, expected, "plain wait baseline changed");
    let named = info_lines(&sim(&dir, "named", &checker_tb("4 cycles on clk"), &[]));
    assert_eq!(
        named, expected,
        "`wait 4 cycles on clk` skipped checker passes"
    );
    let wall = info_lines(&sim(&dir, "wall", &checker_tb("40ns"), &[]));
    assert_eq!(wall, expected, "`wait 40ns` skipped checker passes");
    let common = ["--cpp-split", "tests", "--cpp-split-layout", "common"];
    let named_common = info_lines(&sim(
        &dir,
        "named_common",
        &checker_tb("4 cycles on clk"),
        &common,
    ));
    assert_eq!(
        named_common, expected,
        "common layout `wait 4 cycles on clk` skipped checker passes"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Inside a testbench function (synchronous context) every wait form —
/// plain `wait N cycles`, `wait N cycles on clk`, and `wait <time>` —
/// runs the per-cycle checker pass exactly once per cycle: no skipped
/// call cycle and no duplicated end cycle. The level-mode `on <expr>`
/// handler fires on every pass, so a duplicated pass would log twice.
/// Checked in both emission layouts.
#[test]
fn helper_waits_run_the_checker_pass_exactly_once_per_cycle() {
    if !verilator_available("helper_waits_run_the_checker_pass_exactly_once") {
        return;
    }
    let dir = scratch("helper_wait_checker_passes");
    let helper = r#"domain D
    freq_mhz: 100
end domain D
testbench Tb
    dut : Top
    on 1 cycles
        log(info, "TICK=${cycle_count}")
    end on
    on cycle_count >= 1 level
        log(info, "LVL=${cycle_count}")
    end on
    function f_plain()
        wait 2 cycles
    end function f_plain
    function f_named()
        wait 2 cycles on clk
    end function f_named
    function f_wall()
        wait 20ns
    end function f_wall
end testbench Tb
impl T for Tb
    clock clk = D
    run
        dut.rst = 0
        wait 1 cycle
        log(info, "P0=${cycle_count}")
        f_plain()
        log(info, "P1=${cycle_count}")
        wait 1 cycle
        log(info, "N0=${cycle_count}")
        f_named()
        log(info, "N1=${cycle_count}")
        wait 1 cycle
        log(info, "W0=${cycle_count}")
        f_wall()
        log(info, "W1=${cycle_count}")
        wait 1 cycle
    end run
end impl T
"#;
    // Each cycle: one TICK and one LVL; each helper call spans two cycles
    // and its caller logs before that cycle's pass, as a run-body wait does.
    let mut expected: Vec<String> = Vec::new();
    let marks = [
        (1, "P0"),
        (3, "P1"),
        (4, "N0"),
        (6, "N1"),
        (7, "W0"),
        (9, "W1"),
    ];
    for cycle in 1..=10 {
        for (at, mark) in marks {
            if at == cycle {
                expected.push(format!("{mark}={cycle}"));
            }
        }
        expected.push(format!("TICK={cycle}"));
        expected.push(format!("LVL={cycle}"));
    }
    let lines = info_lines(&sim(&dir, "helper", helper, &[]));
    assert_eq!(
        lines, expected,
        "helper-context waits skipped or repeated a checker pass"
    );
    let common = ["--cpp-split", "tests", "--cpp-split-layout", "common"];
    let lines = info_lines(&sim(&dir, "helper_common", helper, &common));
    assert_eq!(
        lines, expected,
        "common layout helper waits skipped or repeated a checker pass"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Every `wait until` form takes at least one cycle when its predicate
/// already holds — in the run body, with or without `timeout`, `all of`
/// and `any of`, and inside testbench functions.
#[test]
fn wait_until_on_true_predicate_takes_one_cycle_in_every_form() {
    if !verilator_available("wait_until_takes_one_cycle") {
        return;
    }
    let dir = scratch("wait_until_one_cycle");
    let run_forms = r#"domain D
    freq_mhz: 100
end domain D
testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    clock clk = D
    run
        dut.rst = 0
        dut.en = 1
        wait 2 cycles
        log(info, "START=${cycle_count}")
        wait until dut.en == 1
        log(info, "UNTIMED=${cycle_count}")
        wait until dut.en == 1 timeout 5 cycles
        log(info, "TIMED=${cycle_count}")
        wait until all of dut.en == 1, dut.rst == 0
        log(info, "ALLOF=${cycle_count}")
        wait until any of dut.en == 1, dut.rst == 1 timeout 3 cycles
        log(info, "ANYOF_TIMED=${cycle_count}")
    end run
end impl T
"#;
    let expected: Vec<String> = [
        "START=2",
        "UNTIMED=3",
        "TIMED=4",
        "ALLOF=5",
        "ANYOF_TIMED=6",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(info_lines(&sim(&dir, "run", run_forms, &[])), expected);
    let common = ["--cpp-split", "tests", "--cpp-split-layout", "common"];
    assert_eq!(
        info_lines(&sim(&dir, "run_common", run_forms, &common)),
        expected,
        "common layout"
    );

    let helper_forms = r#"domain D
    freq_mhz: 100
end domain D
testbench Tb
    dut : Top
    function until_untimed()
        wait until dut.en == 1
    end function until_untimed
    function until_timed()
        wait until dut.en == 1 timeout 5 cycles
    end function until_timed
end testbench Tb
impl T for Tb
    clock clk = D
    run
        dut.rst = 0
        dut.en = 1
        wait 2 cycles
        log(info, "START=${cycle_count}")
        until_untimed()
        log(info, "FN_UNTIMED=${cycle_count}")
        until_timed()
        log(info, "FN_TIMED=${cycle_count}")
    end run
end impl T
"#;
    let expected: Vec<String> = ["START=2", "FN_UNTIMED=3", "FN_TIMED=4"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(
        info_lines(&sim(&dir, "helper", helper_forms, &[])),
        expected
    );

    // Predicates with side effects lower to an explicit polling loop; it
    // too waits one cycle before the first poll, and a runtime budget of
    // zero is clamped to a one-cycle wait in every lowering.
    let effectful = r#"domain D
    freq_mhz: 100
end domain D
sequencer Counter
    hookable next() -> uint<8>
        return 1
    end next
end sequencer Counter
testbench Tb
    dut : Top
    counter : Counter
    function fn_eff()
        wait until counter.next() == 1
    end function fn_eff
    function fn_zero(z: uint<8>)
        wait until dut.en == 1 timeout z cycles
    end function fn_zero
end testbench Tb
impl T for Tb
    clock clk = D
    run
        dut.rst = 0
        dut.en = 1
        let z : uint<8> = 0
        wait 2 cycles
        log(info, "START=${cycle_count}")
        wait until counter.next() == 1
        log(info, "EFF_UNTIMED=${cycle_count}")
        wait until counter.next() == 1 timeout 3 cycles
        log(info, "EFF_TIMED=${cycle_count}")
        fn_eff()
        log(info, "EFF_FN=${cycle_count}")
        wait until dut.en == 1 timeout z cycles
        log(info, "ZERO=${cycle_count}")
        wait until counter.next() == 1 timeout z cycles
        log(info, "ZERO_EFF=${cycle_count}")
        fn_zero(z)
        log(info, "ZERO_FN=${cycle_count}")
    end run
end impl T
"#;
    let expected: Vec<String> = [
        "START=2",
        "EFF_UNTIMED=3",
        "EFF_TIMED=4",
        "EFF_FN=5",
        "ZERO=6",
        "ZERO_EFF=7",
        "ZERO_FN=8",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(
        info_lines(&sim(&dir, "effectful", effectful, &[])),
        expected
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// A constant `timeout` budget below one cycle can never be honoured
/// (every `wait until` takes at least one cycle), so it is a compile error.
#[test]
fn constant_zero_wait_until_timeout_is_a_compile_error() {
    let dir = scratch("wait_until_timeout_zero");
    for (name, budget) in [("zero", "0"), ("negative", "-2")] {
        let source = format!(
            r#"domain D
    freq_mhz: 100
end domain D
test ZeroTimeout
    let dut : Top
    clock clk = D
    run
        wait until dut.en == 1 timeout {budget} cycles
    end run
end test ZeroTimeout
"#
        );
        let out = dump_ir(&dir, name, &source);
        let log = text(&out);
        assert!(!out.status.success(), "timeout {budget} lowered:\n{log}");
        assert!(
            log.contains(&format!(
                "timeout {budget} cycles`: the timeout must be at least 1"
            )),
            "missing timeout {budget} diagnostic:\n{log}"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// A negative runtime `timeout` budget is clamped like zero — one cycle,
/// then a timeout — in every lowering: the coroutine awaiter, the polling
/// loop of a side-effecting predicate, and the synchronous helper loop
/// (both predicate shapes). The `on 1 cycles` guard turns the former
/// ~2^32-cycle hang into a visible fatal.
#[test]
fn negative_runtime_wait_until_budget_is_a_one_cycle_wait() {
    if !verilator_available("negative_runtime_wait_until_budget") {
        return;
    }
    let dir = scratch("wait_until_negative_budget");
    let source = r#"domain D
    freq_mhz: 100
end domain D
sequencer Counter
    hookable next() -> uint<8>
        return 1
    end next
end sequencer Counter
testbench Tb
    dut : Top
    counter : Counter
    on 1 cycles
        if cycle_count == 40
            log(fatal, "HUNG")
        end if
    end on
    function fn_neg(z: sint<8>)
        wait until dut.rst == 1 timeout z cycles
    end function fn_neg
    function fn_neg_eff(z: sint<8>)
        wait until counter.next() == 2 timeout z cycles
    end function fn_neg_eff
end testbench Tb
impl T for Tb
    clock clk = D
    run
        dut.rst = 0
        let z : sint<8> = -1
        wait 2 cycles
        log(info, "START=${cycle_count}")
        wait until dut.rst == 1 timeout z cycles
        log(info, "PLAIN=${cycle_count}")
        wait until counter.next() == 2 timeout z cycles
        log(info, "EFF=${cycle_count}")
        fn_neg(z)
        log(info, "FN=${cycle_count}")
        fn_neg_eff(z)
        log(info, "FN_EFF=${cycle_count}")
    end run
end impl T
"#;
    let out = sim(&dir, "negative", source, &[]);
    let log = text(&out);
    assert!(
        !out.status.success(),
        "timed-out waits did not fail:\n{log}"
    );
    assert!(!log.contains("HUNG"), "negative budget hung:\n{log}");
    assert_eq!(
        log.matches("wait until timed out after -1 cycles").count(),
        4,
        "every negative-budget wait must time out once:\n{log}"
    );
    let expected: Vec<String> = ["START=2", "PLAIN=3", "EFF=4", "FN=5", "FN_EFF=6"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(any_info_lines(&out), expected, "{log}");
    let _ = std::fs::remove_dir_all(dir);
}

const COMB_DUT: &str = r#"module Top (
  input logic clk,
  input logic rst,
  input logic en,
  output logic [8-1:0] count_out
);
  assign count_out = {7'b0, en};
endmodule
"#;

fn clockless_comb_tb(wait: &str) -> String {
    format!(
        r#"testbench Tb
    dut : Top
    on 1 cycles
        log(info, "TICK=${{cycle_count}} out=${{dut.count_out}}")
    end on
    function f()
        wait 1 cycles
    end function f
end testbench Tb
impl T for Tb
    run
        dut.rst = 0
        dut.en = 0
        wait 1 cycle
        dut.en = 1
        wait 1 cycle
        dut.en = 0
        {wait}
        dut.en = 1
        {wait}
        wait 1 cycle
    end run
end impl T
"#
    )
}

/// In a clockless test, a helper's wait runs the checker pass at the same
/// point as the main loop — after the falling edge has settled
/// combinational outputs — so a checker reading a combinational output sees
/// the same value as during a run-body wait. Both layouts.
#[test]
fn clockless_helper_wait_checkers_see_settled_combinational_outputs() {
    if !verilator_available("clockless_helper_wait_checkers") {
        return;
    }
    let dir = scratch("clockless_comb_checkers");
    let dut = dir.join("comb.sv");
    std::fs::write(&dut, COMB_DUT).expect("write combinational DUT");
    let expected: Vec<String> = [
        "TICK=1 out=1",
        "TICK=2 out=0",
        "TICK=3 out=1",
        "TICK=4 out=1",
        "TICK=5 out=1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let common = ["--cpp-split", "tests", "--cpp-split-layout", "common"];
    for (name, wait, extra) in [
        ("plain", "wait 1 cycle", &[][..]),
        ("helper", "f()", &[][..]),
        ("helper_common", "f()", &common[..]),
    ] {
        let out = sim_on(&dir, name, &clockless_comb_tb(wait), &dut, "Top", extra);
        assert_eq!(info_lines(&out), expected, "{name}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Pinned per-cycle checker behaviour beyond the single-clock run body: a
/// clockless helper (plain wait and `wait until`), a two-clock test mixing
/// wall-clock and slow-clock waits in the run body and a helper, and the
/// `--mt` actor model.
#[test]
fn checker_passes_are_pinned_for_clockless_multiclock_and_mt() {
    if !verilator_available("checker_passes_are_pinned") {
        return;
    }
    let dir = scratch("checker_pass_pins");
    let clockless = r#"testbench Tb
    dut : Top
    on 1 cycles
        log(info, "TICK=${cycle_count}")
    end on
    on cycle_count >= 1 level
        log(info, "LVL=${cycle_count}")
    end on
    function f()
        wait 2 cycles
    end function f
    function g()
        wait until dut.rst == 0
    end function g
end testbench Tb
impl T for Tb
    run
        dut.rst = 0
        wait 1 cycle
        log(info, "A=${cycle_count}")
        f()
        log(info, "B=${cycle_count}")
        g()
        log(info, "C=${cycle_count}")
        wait 1 cycle
    end run
end impl T
"#;
    let expected: Vec<String> = [
        "A=1", "TICK=1", "LVL=1", "TICK=2", "LVL=2", "B=3", "TICK=3", "LVL=3", "C=4", "TICK=4",
        "LVL=4", "TICK=5", "LVL=5",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(
        info_lines(&sim(&dir, "clockless", clockless, &[])),
        expected
    );
    let common = ["--cpp-split", "tests", "--cpp-split-layout", "common"];
    assert_eq!(
        info_lines(&sim(&dir, "clockless_common", clockless, &common)),
        expected,
        "common layout"
    );

    let multiclock = r#"domain FastDomain
    freq_mhz: 200
end domain FastDomain
domain SlowDomain
    freq_mhz: 50
end domain SlowDomain
testbench Tb
    dut : MultiClockSync
    on 1 cycles
        log(info, "TICK=${cycle_count}")
    end on
    function f()
        wait 3ns
        wait 1 cycles on slow_clk
    end function f
end testbench Tb
impl T for Tb
    clock fast_clk = FastDomain
    clock slow_clk = SlowDomain
    run
        dut.rst = 0
        wait 1 cycle
        log(info, "A=${cycle_count}")
        wait 2ns
        log(info, "B=${cycle_count}")
        wait 1 cycles on slow_clk
        log(info, "C=${cycle_count}")
        wait 7ns
        log(info, "D=${cycle_count}")
        f()
        log(info, "E=${cycle_count}")
        wait 1 cycle
        log(info, "F=${cycle_count}")
        wait 2 cycles
    end run
end impl T
"#;
    let expected: Vec<String> = [
        "A=1", "B=1", "TICK=1", "C=2", "TICK=2", "D=3", "TICK=3", "TICK=4", "TICK=5", "E=6",
        "TICK=6", "F=7", "TICK=7", "TICK=8", "TICK=9",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let out = sim_on(
        &dir,
        "multiclock",
        multiclock,
        &root().join("tests/dut/multi_clock.sv"),
        "MultiClockSync",
        &[],
    );
    assert_eq!(info_lines(&out), expected, "multi-clock");

    let expected: Vec<String> = [
        "BEFORE=1", "TICK=1", "TICK=2", "TICK=3", "TICK=4", "AFTER=5", "TICK=5", "TICK=6",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for (name, wait) in [("mt_named", "4 cycles on clk"), ("mt_wall", "40ns")] {
        let out = sim(&dir, name, &checker_tb(wait), &["--mt"]);
        assert_eq!(info_lines(&out), expected, "{name}");
    }
    let _ = std::fs::remove_dir_all(dir);
}
