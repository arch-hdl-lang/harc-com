//! Whole expression trees and exact small-domain solver model sets.
use harc::parser::parse_source;
use harc::solver::problem_table::{build_typed_solver_problem_table, TypedSolverProblemBuild};
use std::{fs, path::Path, process::Command};

fn python(root: &Path, out: &Path, phase: &str) {
    python_seed(root, out, phase, None);
}

fn python_seed(root: &Path, out: &Path, phase: &str, seed: Option<&str>) {
    let mut command = Command::new("python3");
    command
        .arg(root.join("tests/conformance/expression_models.py"))
        .args(["--phase", phase, "--out"])
        .arg(out)
        .arg("--harc")
        .arg(env!("CARGO_BIN_EXE_harc"));
    if let Some(seed) = seed {
        command.arg("--seed").arg(seed);
    } else if let Some(seed) = std::env::var_os("HARC_EXPRESSION_SEED") {
        command.arg("--seed").arg(seed);
    }
    let output = command
        .output()
        .expect("Python 3 required for expression properties");
    assert!(
        output.status.success(),
        "{phase} failed; artifacts {}\n{}\n{}",
        out.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!("{}", String::from_utf8_lossy(&output.stdout));
}

#[test]
fn expression_trees_and_solver_models_match_independent_interpretation() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let base = std::env::var_os("HARC_CONFORMANCE_OUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    fs::create_dir_all(&base).unwrap();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let out = base.join(format!(
        "harc_expression_models_{}_{nonce}",
        std::process::id()
    ));
    fs::create_dir(&out).unwrap();
    python(root, &out, "generate");
    let mut paths: Vec<_> = fs::read_dir(out.join("models"))
        .unwrap()
        .map(|p| p.unwrap().path())
        .collect();
    paths.sort();
    assert_eq!(paths.len(), 38);
    for path in paths {
        let parsed = parse_source(&fs::read_to_string(&path).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let table = build_typed_solver_problem_table(&parsed);
        assert_eq!(table.entries.len(), 1, "{}", path.display());
        match table.entries.into_iter().next().unwrap().build {
            TypedSolverProblemBuild::Z3 { z3, .. } => {
                fs::write(path.with_extension("smt2"), z3.smt).unwrap()
            }
            other => panic!("{}: {other:?}", path.display()),
        }
    }
    python(root, &out, "typed");
    python(root, &out, "expressions");
    let alternate = out.join("seed1");
    python_seed(root, &alternate, "generate", Some("1"));
    python_seed(root, &alternate, "expressions", Some("1"));
    python(root, &out, "models");
    fs::remove_dir_all(out).unwrap();
}
