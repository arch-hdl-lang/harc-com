//! Native regressions for loop-bound scoping, negative `repeat` counts and
//! `domain` frequency diagnostics (spec §7.4).

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
