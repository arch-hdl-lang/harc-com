//! Executable conformance: Python arbitrary-precision oracle vs both native
//! emitters and the typed SMT backend. Expectations never call compiler code.
use harc::parser::parse_source;
use harc::solver::problem_table::{build_typed_solver_problem_table, TypedSolverProblemBuild};
use std::{fs, path::Path, process::Command};

fn python(root: &Path, out: &Path, phase: &str) {
    let mut command = Command::new("python3");
    command
        .arg(root.join("tests/conformance/arithmetic_solver.py"))
        .args(["--phase", phase, "--out"])
        .arg(out)
        .arg("--harc")
        .arg(env!("CARGO_BIN_EXE_harc"));
    if let Some(seed) = std::env::var_os("HARC_CONFORMANCE_SEED") {
        command.arg("--seed").arg(seed);
    }
    if std::env::var_os("HARC_CONFORMANCE_EXTENDED").is_some() {
        command.arg("--extended");
    }
    let output = command
        .output()
        .expect("Python 3 is required for arithmetic/solver conformance");
    assert!(
        output.status.success(),
        "conformance {phase} failed; artifacts: {}\n{}\n{}",
        out.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!("{}", String::from_utf8_lossy(&output.stdout));
}

#[test]
fn arithmetic_and_solver_match_independent_integer_oracle() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let base = std::env::var_os("HARC_CONFORMANCE_OUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    fs::create_dir_all(&base).expect("create conformance output parent");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let out = base.join(format!("harc_conformance_{}_{}", std::process::id(), nonce));
    fs::create_dir(&out).expect("create fresh conformance artifact directory");
    // This target deliberately fails on missing dependencies; CI must never
    // turn a missing Verilator/Z3/Python installation into conformance success.
    python(root, &out, "generate");
    let mut sources: Vec<_> = fs::read_dir(out.join("typed"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "harc"))
        .collect();
    sources.sort();
    assert!(!sources.is_empty(), "no generated typed solver cases");
    for source in sources {
        let program = parse_source(&fs::read_to_string(&source).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", source.display()));
        let table = build_typed_solver_problem_table(&program);
        assert_eq!(
            table.entries.len(),
            1,
            "{}: missing/extra solver problems",
            source.display()
        );
        let entry = table.entries.into_iter().next().unwrap();
        match entry.build {
            TypedSolverProblemBuild::Z3 { z3, .. } => {
                fs::write(source.with_extension("smt2"), z3.smt).unwrap()
            }
            other => panic!(
                "{}: unexpected typed solver rejection: {other:?}",
                source.display()
            ),
        }
    }
    python(root, &out, "typed");
    python(root, &out, "native");
    // On failure the entire source, SMT and native logs remain for replay.
    fs::remove_dir_all(out).expect("remove successful conformance artifacts");
}
