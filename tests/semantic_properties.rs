//! Properties across parse -> lower -> emit -> native/SMT execution.
//! Python computes expectations without compiler helpers. Missing dependencies
//! fail closed; successful runs clean up, failures retain replayable evidence.
use harc::parser::parse_source;
use harc::solver::problem_table::{build_typed_solver_problem_table, TypedSolverProblemBuild};
use std::{fs, path::Path, process::Command};

fn python(root: &Path, out: &Path, phase: &str) {
    let output = Command::new("python3")
        .arg(root.join("tests/conformance/semantic_properties.py"))
        .args(["--phase", phase, "--out"])
        .arg(out)
        .arg("--harc")
        .arg(env!("CARGO_BIN_EXE_harc"))
        .output()
        .expect("Python 3 required for semantic properties");
    assert!(
        output.status.success(),
        "{phase} properties failed; {}\n{}\n{}",
        out.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!("{}", String::from_utf8_lossy(&output.stdout));
}

#[test]
fn compiler_compositions_match_independent_semantics() {
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
        "harc_semantic_properties_{}_{nonce}",
        std::process::id()
    ));
    fs::create_dir(&out).unwrap();
    python(root, &out, "generate");
    let mut paths: Vec<_> = fs::read_dir(out.join("typed"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    paths.sort();
    assert!(!paths.is_empty());
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
    python(root, &out, "native");
    fs::remove_dir_all(out).unwrap();
}
