use std::{fs, process::Command};

#[test]
fn predicate_equality_solver_executes_in_tbir_and_v1() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/fixtures/predicate_equality_solver.harc");
    let dut = root.join("tests/fixtures/predicate_equality_solver.sv");
    let base = std::env::temp_dir().join(format!(
        "harc_predicate_equality_solver_{}",
        std::process::id()
    ));
    fs::create_dir_all(&base).unwrap();

    for codegen in ["tbir", "v1"] {
        let sat_out = base.join(format!("{codegen}-sat"));
        let sat = Command::new(env!("CARGO_BIN_EXE_harc"))
            .args([
                "sim", fixture.to_str().unwrap(), "--sv", dut.to_str().unwrap(), "--top", "Top",
                "--codegen", codegen, "--test", "Sat", "--jobs", "2",
            ])
            .arg("--outdir")
            .arg(&sat_out)
            .output()
            .expect("harc sim Sat");
        let sat_log = format!(
            "{}{}",
            String::from_utf8_lossy(&sat.stdout),
            String::from_utf8_lossy(&sat.stderr)
        );
        assert!(sat.status.success(), "{codegen} SAT failed:\n{sat_log}");
        assert!(sat_log.contains("ALL TESTS PASSED"), "{codegen} SAT did not pass:\n{sat_log}");

        let unsat_out = base.join(format!("{codegen}-unsat"));
        let unsat = Command::new(env!("CARGO_BIN_EXE_harc"))
            .args([
                "sim", fixture.to_str().unwrap(), "--sv", dut.to_str().unwrap(), "--top", "Top",
                "--codegen", codegen, "--test", "Unsat", "--jobs", "2",
            ])
            .arg("--outdir")
            .arg(&unsat_out)
            .output()
            .expect("harc sim Unsat");
        let unsat_log = format!(
            "{}{}",
            String::from_utf8_lossy(&unsat.stdout),
            String::from_utf8_lossy(&unsat.stderr)
        );
        assert!(!unsat.status.success(), "{codegen} UNSAT unexpectedly passed:\n{unsat_log}");
        assert!(unsat_log.contains("UNSAT"), "{codegen} query did not report UNSAT:\n{unsat_log}");
    }

    fs::remove_dir_all(base).unwrap();
}
