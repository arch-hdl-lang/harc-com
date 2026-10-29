//! `--test` selection on the ARCH backend. Set ARCH_BIN to run the compiler pair.
use std::{fs, path::Path, process::Command};

const SUITE: &str = r#"
testbench SelectTb
    dut : Top
end testbench SelectTb
impl A_first for SelectTb
    run
        log(info, "RAN_A")
        ${A_BODY}
    end run
end impl A_first
impl B_second for SelectTb
    run
        log(info, "RAN_B")
        ${B_BODY}
    end run
end impl B_second
"#;

/// Run `harc sim --dut ... --test B_second` in the default suite compile
/// scope. The selected test sorts second, so a launcher that drops the
/// selection falls back to `A_first` instead.
fn run_selected(arch: &str, name: &str, a_body: &str, b_body: &str) -> (bool, String) {
    let dir = std::env::temp_dir().join(format!("harc_arch_select_{name}_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let tb = dir.join("select.harc");
    fs::write(
        &tb,
        SUITE
            .replace("${A_BODY}", a_body)
            .replace("${B_BODY}", b_body),
    )
    .unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--dut"])
        .arg(root.join("tests/dut/top_counter.arch"))
        .args(["--top", "Top", "--arch-bin", arch, "--test", "B_second"])
        .arg("--outdir")
        .arg(dir.join("build"))
        .arg(&tb)
        .env("HARC_NO_LEARN", "1")
        .env("ARCH_NO_LEARN", "1")
        .env_remove("HARC_TEST")
        .output()
        .unwrap();
    let log = fs::read_to_string(dir.join("build/sim.log")).unwrap_or_else(|_| {
        format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    let _ = fs::remove_dir_all(&dir);
    (output.status.success(), log)
}

#[test]
fn arch_suite_runs_the_selected_test() {
    let Ok(arch) = std::env::var("ARCH_BIN") else {
        eprintln!("set ARCH_BIN to exercise ARCH test selection");
        return;
    };
    let pass = "wait 1 cycle";
    let fail = "assert false else fail(\"selected test failed\")";

    let (ok, log) = run_selected(&arch, "fail", pass, fail);
    assert!(!ok, "selected failing test must fail the run:\n{log}");
    assert!(
        log.contains("RAN_B") && !log.contains("RAN_A"),
        "only the selected test may run:\n{log}"
    );

    let (ok, log) = run_selected(&arch, "pass", fail, pass);
    assert!(ok, "selected passing test must pass the run:\n{log}");
    assert!(
        log.contains("RAN_B") && !log.contains("RAN_A"),
        "only the selected test may run:\n{log}"
    );
}

/// `--sim-arg` cannot reach the simulation binary on the ARCH backend
/// (`arch sim` owns its argv), so it is refused up front instead of being
/// silently dropped. ARCH-free: `--arch-bin` names a path that does not
/// exist, so reaching the launcher would fail with a different error.
#[test]
fn arch_backend_rejects_sim_args_before_launching() {
    let dir = std::env::temp_dir().join(format!("harc_arch_sim_arg_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let tb = dir.join("select.harc");
    fs::write(
        &tb,
        SUITE
            .replace("${A_BODY}", "wait 1 cycle")
            .replace("${B_BODY}", "wait 1 cycle"),
    )
    .unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--dut"])
        .arg(root.join("tests/dut/top_counter.arch"))
        .args(["--top", "Top", "--arch-bin"])
        .arg(dir.join("no-such-arch"))
        .args(["--sim-arg", "+TEST=B_second"])
        .arg("--outdir")
        .arg(dir.join("build"))
        .arg(&tb)
        .env("HARC_NO_LEARN", "1")
        .env("ARCH_NO_LEARN", "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let emitted = dir.join("build").exists();
    let _ = fs::remove_dir_all(&dir);
    assert!(!output.status.success(), "--sim-arg on --dut must fail");
    assert!(
        stderr.contains("--sim-arg is not supported on the ARCH backend")
            && stderr.contains("--test <name>"),
        "unexpected diagnostic:\n{stderr}"
    );
    assert!(!emitted, "nothing may be emitted before the refusal");
}
