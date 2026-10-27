//! Native regression for fatal post-eval services during wall-clock waits.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DUT: &str = "tests/dut/top_counter.sv";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run_case(root_dir: &Path, name: &str, source: &str) -> Output {
    run_case_with(root_dir, name, source, &[])
}

fn run_case_with(root_dir: &Path, name: &str, source: &str, extra: &[&str]) -> Output {
    let fixture = root_dir.join(format!("{name}.harc"));
    let outdir = root_dir.join(format!("{name}-out"));
    std::fs::write(&fixture, source).expect("write HARC fixture");
    Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(root().join(DUT))
        .arg(&fixture)
        .args(["--top", "Top", "--outdir"])
        .arg(outdir)
        .args(["--rebuild", "--jobs", "2"])
        .args(extra)
        .env("HARC_NO_LEARN", "1")
        .env("ARCH_NO_LEARN", "1")
        .env("HARC_REQUIRE_VERILATOR", "1")
        .output()
        .expect("spawn harc sim")
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn fixture(wait: &str, fatal: bool) -> String {
    let severity = if fatal { "fatal" } else { "info" };
    format!(
        r#"domain D
    freq_mhz: 100
end domain D
testbench Tb
    dut : Top
    on 1 cycles phase post_eval
        if cycle_count == 1
            log({severity}, "MIDWAIT_FATAL")
        end if
        log(info, "SERVICE=${{cycle_count}}")
    end on
    function wall_helper()
        wait 100ns
        log(info, "HELPER_AFTER")
    end function wall_helper
end testbench Tb
impl T for Tb
    clock clk = D
    run
        dut.rst = 0
        dut.en = 1
        wait {wait}
        log(info, "SUCCESSOR=${{cycle_count}}")
    end run
end impl T
"#
    )
}

fn zero_fatal_fixture() -> &'static str {
    r#"domain D
    freq_mhz: 100
end domain D
testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    clock clk = D
    run
        log(fatal, "BEFORE_ZERO")
        wait 0ns
        log(info, "ZERO_SUCCESSOR")
    end run
end impl T
"#
}

fn helper_fixture() -> &'static str {
    r#"domain D
    freq_mhz: 100
end domain D
testbench Tb
    dut : Top
    on 1 cycles phase post_eval
        if cycle_count == 1
            log(fatal, "HELPER_FATAL")
        end if
        log(info, "SERVICE=${cycle_count}")
    end on
    function wall_helper()
        wait 100ns
        log(info, "HELPER_AFTER")
    end function wall_helper
end testbench Tb
impl T for Tb
    clock clk = D
    run
        wall_helper()
        log(info, "CALLER_AFTER")
    end run
end impl T
"#
}

#[test]
fn fatal_wall_wait_stops_current_run_and_preserves_zero_wait_boundary() {
    if Command::new("verilator")
        .arg("--version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        if std::env::var_os("HARC_REQUIRE_VERILATOR").is_some() {
            panic!("verilator required");
        }
        eprintln!("SKIP fatal_wall_wait_e2e: verilator not found");
        return;
    }
    let dir = std::env::temp_dir().join(format!("harc_fatal_wall_wait_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test directory");

    let fatal = run_case(&dir, "fatal", &fixture("100ns", true));
    let fatal_log = text(&fatal);
    assert!(
        !fatal.status.success(),
        "fatal case unexpectedly passed:\n{fatal_log}"
    );
    assert!(
        fatal_log.contains("SERVICE=1"),
        "missing current-edge service:\n{fatal_log}"
    );
    assert!(
        !fatal_log.contains("SERVICE=2") && !fatal_log.contains("SUCCESSOR"),
        "future edge or successor ran:\n{fatal_log}"
    );

    let cycles = run_case(&dir, "cycles", &fixture("10 cycles", true));
    let cycles_log = text(&cycles);
    assert!(!cycles.status.success());
    assert!(cycles_log.contains("SERVICE=1") && !cycles_log.contains("SERVICE=2"));

    let info = run_case(&dir, "info", &fixture("100ns", false));
    let info_log = text(&info);
    assert!(info.status.success(), "INFO case failed:\n{info_log}");
    assert!(
        info_log.contains("SERVICE=10") && info_log.contains("SUCCESSOR=10"),
        "wall wait did not reach target:\n{info_log}"
    );

    let zero = run_case(&dir, "zero", zero_fatal_fixture());
    let zero_log = text(&zero);
    assert!(
        !zero.status.success(),
        "fatal zero wait unexpectedly passed:\n{zero_log}"
    );
    assert!(
        zero_log.contains("ZERO_SUCCESSOR"),
        "zero wait did not preserve same-cycle successor:\n{zero_log}"
    );

    let helper = run_case(&dir, "helper", helper_fixture());
    let helper_log = text(&helper);
    assert!(
        !helper.status.success(),
        "fatal helper wait unexpectedly passed:\n{helper_log}"
    );
    assert!(
        helper_log.contains("SERVICE=1"),
        "missing helper current-edge service:\n{helper_log}"
    );
    assert!(
        !helper_log.contains("HELPER_AFTER") && !helper_log.contains("CALLER_AFTER"),
        "helper continued after fatal:\n{helper_log}"
    );

    let typed_source = helper_fixture()
        .replace(
            "function wall_helper()",
            "function wall_helper() -> uint<8>",
        )
        .replace(
            "        log(info, \"HELPER_AFTER\")",
            "        log(info, \"HELPER_AFTER\")\n        return 7",
        )
        .replace(
            "        wall_helper()\n",
            "        let result = wall_helper()\n",
        );
    let typed = run_case(&dir, "typed_helper", &typed_source);
    let typed_log = text(&typed);
    assert!(!typed.status.success(), "{typed_log}");
    assert!(typed_log.contains("SERVICE=1"), "{typed_log}");
    assert!(
        !typed_log.contains("HELPER_AFTER") && !typed_log.contains("CALLER_AFTER"),
        "value-returning helper continued after fatal:\n{typed_log}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// `wait N cycles on <clock>` interrupted by a fatal from a post-eval
/// service. `run_body` is the run block after reset/enable; the helper
/// waits on the named clock too, so helper calls cover the callable path.
fn qualified_fixture(severity: &str, run_body: &str) -> String {
    format!(
        r#"domain D
    freq_mhz: 100
end domain D
testbench Tb
    dut : Top
    on 1 cycles phase post_eval
        if cycle_count == 1
            log({severity}, "MIDWAIT_FATAL")
        end if
        log(info, "SERVICE=${{cycle_count}}")
    end on
    function clock_helper()
        wait 10 cycles on clk
        log(info, "HELPER_AFTER")
    end function clock_helper
    function typed_clock_helper() -> uint<8>
        wait 10 cycles on clk
        log(info, "HELPER_AFTER")
        return 7
    end function typed_clock_helper
end testbench Tb
impl T for Tb
    clock clk = D
    run
        dut.rst = 0
        dut.en = 1
{run_body}
    end run
end impl T
"#
    )
}

#[test]
fn fatal_named_clock_wait_stops_current_run_in_both_layouts() {
    if Command::new("verilator")
        .arg("--version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        if std::env::var_os("HARC_REQUIRE_VERILATOR").is_some() {
            panic!("verilator required");
        }
        eprintln!("SKIP fatal_named_clock_wait: verilator not found");
        return;
    }
    let dir = std::env::temp_dir().join(format!(
        "harc_fatal_named_clock_wait_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test directory");

    let positive = "        wait 10 cycles on clk\n        log(info, \"SUCCESSOR=${cycle_count}\")";
    let helper = "        clock_helper()\n        log(info, \"CALLER_AFTER\")";
    let typed =
        "        let result = typed_clock_helper()\n        log(info, \"CALLER_AFTER=${result}\")";
    let same_cycle =
        "        wait 1 cycles on clk\n        log(info, \"SUCCESSOR=${cycle_count}\")";
    let zero = "        log(fatal, \"BEFORE_ZERO\")\n        wait 0 cycles on clk\n        log(info, \"ZERO_SUCCESSOR\")";
    let layouts: [(&str, &[&str]); 2] = [
        ("default", &[]),
        (
            "common",
            &["--cpp-split", "tests", "--cpp-split-layout", "common"],
        ),
    ];
    for (layout, extra) in layouts {
        let run = |name: &str, severity: &str, body: &str| {
            let output = run_case_with(
                &dir,
                &format!("{layout}_{name}"),
                &qualified_fixture(severity, body),
                extra,
            );
            (output.status.success(), text(&output))
        };

        for (name, body) in [("positive", positive), ("helper", helper), ("typed", typed)] {
            let (ok, log) = run(name, "fatal", body);
            assert!(!ok, "{layout} {name}: fatal run passed:\n{log}");
            assert!(
                log.contains("SERVICE=1") && !log.contains("SERVICE=2"),
                "{layout} {name}: missing current edge or ran a future edge:\n{log}"
            );
            assert!(
                !log.contains("SUCCESSOR")
                    && !log.contains("HELPER_AFTER")
                    && !log.contains("CALLER_AFTER"),
                "{layout} {name}: code after the interrupted wait ran:\n{log}"
            );
        }

        // A wait whose last owed edge is the fatal edge completes, like
        // the unqualified `wait 1 cycles`; so does a zero wait.
        let (ok, log) = run("same_cycle", "fatal", same_cycle);
        assert!(!ok, "{layout} same_cycle: fatal run passed:\n{log}");
        assert!(
            log.contains("SUCCESSOR=1"),
            "{layout} same_cycle: completed wait lost its successor:\n{log}"
        );
        let (ok, log) = run("zero", "info", zero);
        assert!(!ok, "{layout} zero: fatal run passed:\n{log}");
        assert!(
            log.contains("ZERO_SUCCESSOR"),
            "{layout} zero: zero wait lost its successor:\n{log}"
        );

        let (ok, log) = run("info", "info", positive);
        assert!(ok, "{layout} info: nonfatal run failed:\n{log}");
        assert!(
            log.contains("SERVICE=10") && log.contains("SUCCESSOR=10"),
            "{layout} info: wait did not reach its target:\n{log}"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}
