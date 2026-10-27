//! Native regression for fatal post-eval services during wall-clock waits.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DUT: &str = "tests/dut/top_counter.sv";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run_case(root_dir: &Path, name: &str, source: &str) -> Output {
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
