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

/// Run one case under a hard deadline. A fatal that stalls a synchronous
/// polling wait used to spin forever, so a regression must fail here
/// instead of wedging the suite. `harc sim` runs in its own process group
/// so the deadline also kills the simulator it spawned.
#[cfg(unix)]
fn run_case_bounded(root_dir: &Path, name: &str, source: &str, extra: &[&str]) -> (bool, String) {
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};

    let fixture = root_dir.join(format!("{name}.harc"));
    let outdir = root_dir.join(format!("{name}-out"));
    let log = root_dir.join(format!("{name}.log"));
    std::fs::write(&fixture, source).expect("write HARC fixture");
    let log_file = std::fs::File::create(&log).expect("create log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_harc"))
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
        .stdout(log_file.try_clone().expect("clone log"))
        .stderr(log_file)
        .process_group(0)
        .spawn()
        .expect("spawn harc sim");
    let deadline = Instant::now() + Duration::from_secs(300);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll harc sim") {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = Command::new("kill")
                .args(["-KILL", &format!("-{}", child.id())])
                .status();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let status = status.unwrap_or_else(|| panic!("{name}: simulation did not terminate:\n{text}"));
    (status.success(), text)
}

/// Testbench helpers called from `run`, waiting synchronously (cycle,
/// polling and timed polling waits, plus a wall-clock wait) while a
/// post-eval service logs fatal at cycle 1.
fn sync_helper_fixture(severity: &str, helper_wait: &str, typed: bool) -> String {
    let (signature, ret, call) = if typed {
        (" -> uint<8>", "\n        return 7", "let result = helper()")
    } else {
        ("", "", "helper()")
    };
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
    function helper(){signature}
        {helper_wait}
        log(info, "HELPER_AFTER"){ret}
    end function helper
end testbench Tb
impl T for Tb
    clock clk = D
    run
        dut.rst = 0
        dut.en = 1
        {call}
        log(info, "CALLER_AFTER")
        log(info, "SUCCESSOR=${{cycle_count}}")
    end run
end impl T
"#
    )
}

#[cfg(unix)]
#[test]
fn fatal_sync_helper_waits_stop_current_run_in_both_layouts() {
    if Command::new("verilator")
        .arg("--version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        if std::env::var_os("HARC_REQUIRE_VERILATOR").is_some() {
            panic!("verilator required");
        }
        eprintln!("SKIP fatal_sync_helper_waits: verilator not found");
        return;
    }
    let dir = std::env::temp_dir().join(format!(
        "harc_fatal_sync_helper_wait_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test directory");

    let layouts: [(&str, &[&str]); 2] = [
        ("default", &[]),
        (
            "common",
            &["--cpp-split", "tests", "--cpp-split-layout", "common"],
        ),
    ];
    for (layout, extra) in layouts {
        let run = |name: &str, severity: &str, wait: &str, typed: bool| {
            run_case_bounded(
                &dir,
                &format!("{layout}_{name}"),
                &sync_helper_fixture(severity, wait, typed),
                extra,
            )
        };

        for (name, wait, typed) in [
            ("cycles", "wait 10 cycles", false),
            ("typed_cycles", "wait 10 cycles", true),
            ("until", "wait until cycle_count >= 10", false),
            (
                "until_timeout",
                "wait until cycle_count >= 10 timeout 20 cycles",
                false,
            ),
            ("wall", "wait 100ns", false),
        ] {
            // The common layout rejects a timed predicate wait inside a
            // function at compile time, so only the default layout emits
            // its polling loop.
            if layout == "common" && name == "until_timeout" {
                continue;
            }
            let (ok, log) = run(name, "fatal", wait, typed);
            assert!(!ok, "{layout} {name}: fatal run passed:\n{log}");
            assert!(
                log.contains("SERVICE=1") && !log.contains("SERVICE=2"),
                "{layout} {name}: missing current edge or ran a future edge:\n{log}"
            );
            assert!(
                !log.contains("HELPER_AFTER")
                    && !log.contains("CALLER_AFTER")
                    && !log.contains("SUCCESSOR"),
                "{layout} {name}: code after the interrupted wait ran:\n{log}"
            );
        }

        // A helper wait whose last owed edge is the fatal edge completes,
        // like a run-level `wait 1 cycles`.
        let (ok, log) = run("same_cycle", "fatal", "wait 1 cycles", false);
        assert!(!ok, "{layout} same_cycle: fatal run passed:\n{log}");
        assert!(
            log.contains("HELPER_AFTER") && log.contains("SUCCESSOR=1"),
            "{layout} same_cycle: completed wait lost its successor:\n{log}"
        );

        let (ok, log) = run("info", "info", "wait 10 cycles", false);
        assert!(ok, "{layout} info: nonfatal run failed:\n{log}");
        assert!(
            log.contains("SERVICE=10") && log.contains("SUCCESSOR=10"),
            "{layout} info: wait did not reach its target:\n{log}"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// An event-subscription hook fires in synchronous dispatch, where a fatal
/// cannot unwind; its polling wait must still end once fatal stops time
/// rather than spinning (default layout) or simulating later edges
/// (common layout).
#[cfg(unix)]
#[test]
fn fatal_stops_synchronous_hook_polling_wait_in_both_layouts() {
    if Command::new("verilator")
        .arg("--version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        if std::env::var_os("HARC_REQUIRE_VERILATOR").is_some() {
            panic!("verilator required");
        }
        eprintln!("SKIP fatal_synchronous_hook_wait: verilator not found");
        return;
    }
    let dir =
        std::env::temp_dir().join(format!("harc_fatal_sync_hook_wait_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test directory");
    let source = r#"domain SysDomain
  freq_mhz: 100
end domain SysDomain
agent HookSource
    obs : out event<sint<8>>
    hookable fire(v: sint<8>)
        emit obs(v)
    end fire
end agent HookSource
env HookEnv
    source : HookSource
end env HookEnv
testbench HookTb
    dut : Top
    env : HookEnv
    on 1 cycles phase post_eval
        if cycle_count == 1
            log(fatal, "MIDWAIT_FATAL")
        end if
        log(info, "SERVICE=${cycle_count}")
    end on
end testbench HookTb
impl HookTest for HookTb
    clock clk = SysDomain
    run
        dut.rst = 0
        dut.en = 1
        on env.source.obs(v)
            wait until cycle_count >= 10
        end on
        env.source.fire(-3)
        wait 20 cycles
        log(info, "SUCCESSOR=${cycle_count}")
    end run
end impl HookTest
"#;
    for (layout, extra) in [
        ("default", &[][..]),
        (
            "common",
            &["--cpp-split", "tests", "--cpp-split-layout", "common"][..],
        ),
    ] {
        let (ok, log) = run_case_bounded(&dir, &format!("{layout}_hook_until"), source, extra);
        assert!(!ok, "{layout}: fatal run passed:\n{log}");
        assert!(
            log.contains("SERVICE=1") && !log.contains("SERVICE=2"),
            "{layout}: missing current edge or ran a future edge:\n{log}"
        );
        assert!(
            !log.contains("SUCCESSOR"),
            "{layout}: run continued past its own wait after fatal:\n{log}"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// A wall-clock wait whose full duration has elapsed when a fatal is
/// raised on that same instant COMPLETES (spec §7.7): `wait 5ns` lands on
/// the first rising edge (100 MHz), where the post-eval service logs
/// fatal, so the statements after the wait run in that cycle and the test
/// still fails at the end of it. `wait 7ns` is interrupted with 2 ns still
/// owed and aborts immediately. Covered at run level and through a
/// synchronous testbench helper, in both emission layouts. Before the fix
/// the completed wait threw on `_fatal` and lost its successor.
#[cfg(unix)]
#[test]
fn completed_wall_wait_on_fatal_edge_runs_its_successor_in_both_layouts() {
    if Command::new("verilator")
        .arg("--version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        if std::env::var_os("HARC_REQUIRE_VERILATOR").is_some() {
            panic!("verilator required");
        }
        eprintln!("SKIP completed_wall_wait_on_fatal_edge: verilator not found");
        return;
    }
    let dir = std::env::temp_dir().join(format!(
        "harc_fatal_wall_wait_completed_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test directory");

    let layouts: [(&str, &[&str]); 2] = [
        ("default", &[]),
        (
            "common",
            &["--cpp-split", "tests", "--cpp-split-layout", "common"],
        ),
    ];
    for (layout, extra) in layouts {
        let run = |name: &str, source: &str| {
            run_case_bounded(&dir, &format!("{layout}_{name}"), source, extra)
        };

        // Run-level wait landing exactly on the fatal edge. The common
        // layout rejects a run-level wall-clock wait at compile time, so
        // it is covered through the helper path only.
        if layout == "default" {
            let (ok, log) = run("run_final", &fixture("5ns", true));
            assert!(!ok, "{layout} run_final: fatal run passed:\n{log}");
            assert!(
                log.contains("SERVICE=1") && !log.contains("SERVICE=2"),
                "{layout} run_final: missing current edge or ran a future edge:\n{log}"
            );
            assert!(
                log.contains("SUCCESSOR=1"),
                "{layout} run_final: completed wall wait lost its successor:\n{log}"
            );
        }

        // Same instant through a synchronous helper.
        let (ok, log) = run(
            "helper_final",
            &sync_helper_fixture("fatal", "wait 5ns", false),
        );
        assert!(!ok, "{layout} helper_final: fatal run passed:\n{log}");
        assert!(
            log.contains("SERVICE=1") && !log.contains("SERVICE=2"),
            "{layout} helper_final: missing current edge or ran a future edge:\n{log}"
        );
        assert!(
            log.contains("HELPER_AFTER") && log.contains("SUCCESSOR=1"),
            "{layout} helper_final: completed helper wall wait lost its successor:\n{log}"
        );

        // Interrupted with time still owed: aborts immediately.
        for (name, source) in [
            ("run_owed", fixture("7ns", true)),
            (
                "helper_owed",
                sync_helper_fixture("fatal", "wait 7ns", false),
            ),
        ] {
            if layout == "common" && name == "run_owed" {
                continue;
            }
            let (ok, log) = run(name, &source);
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
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Clockless testbench: a wall-clock wait that BEGINS after a fatal owes
/// its whole duration, so it must abort rather than advance time and run
/// its successor (spec §7.7). The clockless scheduler advanced `now_ps`
/// unconditionally, so a "time still owed" check after the advance could
/// not see it.
fn clockless_fixture(severity: &str, body: &str) -> String {
    format!(
        r#"testbench Tb
    dut : Top
    function helper()
        wait 100ns
        log(info, "HELPER_AFTER")
    end function helper
end testbench Tb
impl W for Tb
    run
        dut.rst = 0
        dut.en = 1
        log({severity}, "RAISED")
{body}
    end run
end impl W
"#
    )
}

#[cfg(unix)]
#[test]
fn clockless_wall_wait_after_fatal_aborts_in_both_layouts() {
    if Command::new("verilator")
        .arg("--version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        if std::env::var_os("HARC_REQUIRE_VERILATOR").is_some() {
            panic!("verilator required");
        }
        eprintln!("SKIP clockless_wall_wait_after_fatal: verilator not found");
        return;
    }
    let dir = std::env::temp_dir().join(format!(
        "harc_fatal_wall_wait_clockless_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test directory");

    let run_level = "        wait 100ns\n        log(info, \"RUN_AFTER\")";
    let helper = "        helper()\n        log(info, \"CALLER_AFTER\")";
    let layouts: [(&str, &[&str]); 2] = [
        ("default", &[]),
        (
            "common",
            &["--cpp-split", "tests", "--cpp-split-layout", "common"],
        ),
    ];
    for (layout, extra) in layouts {
        let run = |name: &str, severity: &str, body: &str| {
            run_case_bounded(
                &dir,
                &format!("{layout}_{name}"),
                &clockless_fixture(severity, body),
                extra,
            )
        };
        // The common layout rejects a run-level wall-clock wait at compile
        // time; it is covered through the helper only.
        let cases: &[(&str, &str)] = if layout == "default" {
            &[("run", run_level), ("helper", helper)]
        } else {
            &[("helper", helper)]
        };
        for (name, body) in cases {
            let (ok, log) = run(name, "fatal", body);
            assert!(!ok, "{layout} {name}: fatal run passed:\n{log}");
            assert!(
                !log.contains("RUN_AFTER")
                    && !log.contains("HELPER_AFTER")
                    && !log.contains("CALLER_AFTER"),
                "{layout} {name}: a wait begun after fatal ran its successor:\n{log}"
            );

            // Control: without a fatal the same wait completes.
            let (ok, log) = run(&format!("{name}_info"), "info", body);
            assert!(ok, "{layout} {name}_info: nonfatal run failed:\n{log}");
            assert!(
                log.contains("RUN_AFTER") || log.contains("CALLER_AFTER"),
                "{layout} {name}_info: wait did not complete:\n{log}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}
