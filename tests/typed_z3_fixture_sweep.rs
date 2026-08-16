//! Phase 4 scaffold parity sweep.
//!
//! This does not execute Z3 and does not replace `cpp_tb.rs`. It proves that
//! every fixture can be walked, every clean typed lowering can be handed to
//! the solver backend boundary, and unsupported backend cases are reported as
//! structured entries rather than panics.

use std::fs;
use std::path::Path;

use harc::constraints::typed_lower::LowerError;
use harc::parser::parse_source;
use harc::solver::problem_table::{
    build_typed_solver_problem_table, TypedSolverProblemBuild, TypedSolverProblemSource,
};

#[test]
fn typed_z3_backend_builds_for_clean_fixture_lowers() {
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut entries: Vec<_> = fs::read_dir(&fixtures_dir)
        .expect("read fixtures dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("harc"))
        .collect();
    entries.sort_by_key(|e| e.path());

    let mut total_fixtures = 0usize;
    let mut total_problems = 0usize;
    let mut z3_built = 0usize;
    let mut expected_lower_errors = Vec::new();
    let mut unexpected_lower_errors = Vec::new();
    let mut backend_errors = Vec::new();

    for entry in entries {
        let path = entry.path();
        let src = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let parsed = match parse_source(&src) {
            Ok(p) => p,
            Err(_) => continue,
        };
        total_fixtures += 1;

        let table = build_typed_solver_problem_table(&parsed);
        total_problems += table.entries.len();
        for entry in table.entries {
            match entry.build {
                TypedSolverProblemBuild::Z3 { typed, z3 } => {
                    assert!(
                        z3.smt.contains("(check-sat)"),
                        "Z3 scaffold output for {} {} missed check-sat",
                        path.display(),
                        source_label(&entry.source)
                    );
                    assert_eq!(
                        z3.assertions.len(),
                        typed.constraints.len() + typed.soft_constraints.len(),
                        "assertion origin map lost clauses for {} {}",
                        path.display(),
                        source_label(&entry.source)
                    );
                    z3_built += 1;
                }
                TypedSolverProblemBuild::LowerError(errors) => {
                    let label = source_label(&entry.source);
                    let rendered = format!("{} {label}: {errors:#?}", path.display());
                    if let Some(reason) = expected_lower_error_reason(&path, &entry.source, &errors)
                    {
                        expected_lower_errors.push(format!("{rendered}\n  classified: {reason}"));
                    } else {
                        unexpected_lower_errors.push(rendered);
                    }
                }
                TypedSolverProblemBuild::BackendError(err) => {
                    backend_errors.push(format!(
                        "{} {}: {err:#?}",
                        path.display(),
                        source_label(&entry.source)
                    ));
                }
            }
        }
    }

    eprintln!(
        "[typed_z3 sweep] fixtures={total_fixtures} problems={total_problems} \
         z3_built={z3_built} expected_lower_errors={expected_lower_errors} \
         unexpected_lower_errors={unexpected_lower_errors} backend_errors={backend_errors}",
        expected_lower_errors = expected_lower_errors.len(),
        unexpected_lower_errors = unexpected_lower_errors.len(),
        backend_errors = backend_errors.len()
    );
    for line in expected_lower_errors.iter().take(12) {
        eprintln!("[typed_z3 expected-lower-error] {line}");
    }
    for line in unexpected_lower_errors.iter().take(12) {
        eprintln!("[typed_z3 unexpected-lower-error] {line}");
    }
    for line in backend_errors.iter().take(12) {
        eprintln!("[typed_z3 backend-error] {line}");
    }

    assert!(
        total_fixtures > 0,
        "no fixtures found in {}",
        fixtures_dir.display()
    );
    assert!(
        total_problems > 0,
        "no typed constraint problems discovered"
    );
    assert!(
        z3_built > 0,
        "no fixture problem reached the Z3 backend scaffold"
    );
    assert!(
        unexpected_lower_errors.is_empty(),
        "unexpected typed-lowering errors reached the typed Z3 sweep:\n{}",
        unexpected_lower_errors.join("\n")
    );
    assert!(
        backend_errors.is_empty(),
        "typed Z3 backend errors reached the sweep:\n{}",
        backend_errors.join("\n")
    );
}

fn source_label(source: &TypedSolverProblemSource) -> String {
    match source {
        TypedSolverProblemSource::TransactionTemplate { transaction, .. } => {
            format!("bare {transaction}")
        }
        TypedSolverProblemSource::RandomizeSite {
            context, target, ..
        } => {
            if context.contains("randomize(") {
                context.clone()
            } else {
                format!("{context}: randomize({target})")
            }
        }
    }
}

fn expected_lower_error_reason(
    path: &Path,
    source: &TypedSolverProblemSource,
    errors: &[LowerError],
) -> Option<&'static str> {
    let file = path.file_name().and_then(|s| s.to_str())?;
    if file == "axi_agent.harc" {
        match source {
            TypedSolverProblemSource::TransactionTemplate { transaction, .. }
            | TypedSolverProblemSource::RandomizeSite { transaction, .. } => {
                if transaction == "AxiTxn" {
                    return Some(
                        "spec-sketch transaction uses unresolved imported enum-like types/variants \
                         (`AxiOp`, `BurstType`, `READ`, `WRITE`, `WRAP`) and illustrative AXI bounds; \
                         it is a parser/pretty fixture, not a fully typed constraint fixture",
                    );
                }
            }
        }
    }
    if file == "uint64_unique_randomize_test.harc" {
        let fixture_source = fs::read_to_string(path).ok()?;
        if let TypedSolverProblemSource::RandomizeSite {
            context,
            target,
            transaction,
            blocking: false,
            has_with_body: true,
            ..
        } = source
        {
            if transaction == "Uint64UniqueStim"
                && context == "Uint64UniqueRandomizeTest: randomize(s)"
                && target == "s"
                && errors.len() == 1
            {
                if let LowerError::DisallowedInConstraint {
                    what: "expression form",
                    span: error_span,
                } = &errors[0]
                {
                    if fixture_source.rfind("s.sample[63:32]") == Some(error_span.start_usize())
                        && fixture_source.get(error_span.start_usize()..error_span.end_usize())
                            == Some("s.sample[63:32]")
                    {
                        return Some(
                            "behavioral regression fixture uses a relational `randomize ... with` constraint; \
                             the typed Z3 scaffold currently rejects expression-form constraints, while the \
                             simulator path covers this behavior",
                        );
                    }
                }
            }
        }
    }
    None
}

#[test]
fn classifies_axi_agent_spec_sketch_lowering_gap() {
    let path = Path::new("tests/fixtures/axi_agent.harc");
    let source = TypedSolverProblemSource::TransactionTemplate {
        transaction: "AxiTxn".to_string(),
        span: Default::default(),
    };
    let reason = expected_lower_error_reason(path, &source, &[]).expect("classified");
    assert!(reason.contains("spec-sketch transaction"));
    assert!(reason.contains("unresolved imported enum-like types"));

    let site = TypedSolverProblemSource::RandomizeSite {
        context: "tseq RandomTxns".to_string(),
        target: "t".to_string(),
        transaction: "AxiTxn".to_string(),
        blocking: false,
        has_with_body: false,
        span: Default::default(),
    };
    assert!(expected_lower_error_reason(path, &site, &[]).is_some());

    let unrelated_site = TypedSolverProblemSource::RandomizeSite {
        context: "SmokeTest".to_string(),
        target: "t".to_string(),
        transaction: "OtherTxn".to_string(),
        blocking: false,
        has_with_body: false,
        span: Default::default(),
    };
    assert!(expected_lower_error_reason(path, &unrelated_site, &[]).is_none());
}

#[test]
fn classifies_uint64_randomize_with_lowering_gap() {
    let path = Path::new("tests/fixtures/uint64_unique_randomize_test.harc");
    let source = fs::read_to_string(path).expect("read uint64 randomize fixture");
    let parsed = parse_source(&source).expect("parse uint64 randomize fixture");
    let table = build_typed_solver_problem_table(&parsed);
    let entry = table
        .entries
        .iter()
        .find(|entry| match &entry.source {
            TypedSolverProblemSource::RandomizeSite {
                context,
                target,
                transaction,
                blocking: false,
                has_with_body: true,
                ..
            } if context == "Uint64UniqueRandomizeTest: randomize(s)"
                && target == "s"
                && transaction == "Uint64UniqueStim" =>
            {
                true
            }
            _ => false,
        })
        .expect("find uint64 randomize site");
    let errors = match &entry.build {
        TypedSolverProblemBuild::LowerError(errors) => errors,
        build => panic!("expected typed lowering error, got {build:?}"),
    };
    let reason = expected_lower_error_reason(path, &entry.source, errors).expect("classified");
    assert!(reason.contains("relational `randomize ... with` constraint"));
    assert!(reason.contains("typed Z3 scaffold"));

    let wrong_error = [LowerError::DisallowedInConstraint {
        what: "field access target",
        span: Default::default(),
    }];
    assert!(expected_lower_error_reason(path, &entry.source, &wrong_error).is_none());

    let wrong_span = [LowerError::DisallowedInConstraint {
        what: "expression form",
        span: Default::default(),
    }];
    assert!(expected_lower_error_reason(path, &entry.source, &wrong_span).is_none());
}
