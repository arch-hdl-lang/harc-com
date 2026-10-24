//! Native SAT/UNSAT regression for signed `%` in keep constraints.

use std::{fs, path::PathBuf, process::Command};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run(source: &PathBuf, outdir: &PathBuf) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(repo_root().join("tests/dut/top_counter.sv"))
        .arg(source)
        .args(["--top", "Top", "--jobs", "2", "--seed", "1", "--outdir"])
        .arg(outdir)
        .env("HARC_NO_LEARN", "1")
        .output()
        .expect("run signed remainder native fixture")
}

#[test]
fn signed_remainder_native_accepts_sat_and_rejects_wrong_sat_keep() {
    let dir = std::env::temp_dir().join(format!("harc_signed_remainder_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create output directory");
    let source = dir.join("signed_remainder_constraint_test.harc");
    let valid = include_str!("fixtures/signed_remainder_constraint_test.harc");
    fs::write(&source, valid).expect("write valid fixture");

    let ok = run(&source, &dir.join("sat"));
    assert!(
        ok.status.success(),
        "signed remainder SAT fixture failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&ok.stdout),
        String::from_utf8_lossy(&ok.stderr)
    );
    assert!(
        String::from_utf8_lossy(&ok.stdout).contains("ALL TESTS PASSED"),
        "SAT fixture did not report success:\n{}",
        String::from_utf8_lossy(&ok.stdout)
    );

    // This keep is satisfiable under bvsmod (+1) but impossible under HARC's
    // native signed remainder (-5 % 3 == -2). It must fail randomization.
    let wrong = valid.replace("keep a % b == NEG_TWO", "keep a % b == POS_ONE");
    fs::write(&source, wrong).expect("write deliberately wrong fixture");
    let unsat = run(&source, &dir.join("unsat"));
    assert!(
        !unsat.status.success(),
        "wrong signed remainder keep unexpectedly passed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&unsat.stdout),
        String::from_utf8_lossy(&unsat.stderr)
    );
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&unsat.stdout),
        String::from_utf8_lossy(&unsat.stderr)
    );
    assert!(log.contains("UNSAT"), "wrong keep did not report UNSAT:\n{log}");
    let _ = fs::remove_dir_all(dir);
}
