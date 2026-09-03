//! Phase 2 parity sweep.
//!
//! For every `.harc` fixture, parse + elaborate + attempt
//! `lower_problem` on every transaction with no `randomize-with` body.
//! The gate is **no panics**: structured `Err(Vec<LowerError>)` for
//! constructs deferred to later phases (relation application,
//! field-method calls, foreach, when-subtypes) is acceptable and
//! expected.
//!
//! On failure (panic) the harness exits with the standard test
//! framework's traceback.  On success the test prints a one-line
//! summary so CI logs show how many fixtures lowered cleanly vs.
//! produced structured errors.

use std::fs;
use std::path::Path;

use harc::constraints::elaborate_constraints;
use harc::constraints::typed::ConstraintProblemId;
use harc::constraints::typed_lower::lower_problem;
use harc::constraints::typed_verify::verify_constraint_problem;
use harc::lexer::Span;
use harc::parser::parse_source;

#[test]
fn lower_problem_does_not_panic_on_any_fixture() {
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut total_fixtures = 0usize;
    let mut total_txns = 0usize;
    let mut clean_lowers = 0usize;
    let mut structured_errors = 0usize;

    let mut entries: Vec<_> = fs::read_dir(&fixtures_dir)
        .expect("read fixtures dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("harc"))
        .collect();
    entries.sort_by_key(|e| e.path());

    for entry in entries {
        let path = entry.path();
        let src = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let parsed = match parse_source(&src) {
            Ok(p) => p,
            Err(_) => continue, // not a parser-coverage test
        };
        total_fixtures += 1;
        let elab = elaborate_constraints(&parsed);
        // Snapshot the transactions to avoid borrowing `elab` mutably
        // while also iterating.
        let txns = elab.transactions.clone();
        for txn in &txns {
            total_txns += 1;
            // Bare randomize — no `with` body.  This exercises the keep
            // clauses on the transaction plus the FieldEnv construction.
            let result = lower_problem(
                &elab,
                txn,
                None,
                Span::default(),
                ConstraintProblemId(total_txns as u64),
            );
            match result {
                Ok(problem) => {
                    verify_constraint_problem(&problem).unwrap_or_else(|errors| {
                        panic!(
                            "typed verifier rejected cleanly lowered problem from {}: {errors:#?}",
                            path.display()
                        )
                    });
                    clean_lowers += 1;
                }
                Err(_) => structured_errors += 1,
            }
        }
    }

    eprintln!(
        "[typed_lower sweep] fixtures={total_fixtures} txns={total_txns} \
         clean={clean_lowers} structured_errors={structured_errors}"
    );

    // The gate is no panics; that's enforced by reaching this line at
    // all.  We additionally require the sweep to find at least one
    // fixture and at least one transaction — guards against the test
    // silently passing when the fixture directory has moved.
    assert!(
        total_fixtures > 0,
        "no fixtures found in {}",
        fixtures_dir.display()
    );
    assert!(total_txns > 0, "no transactions found in any fixture");
}
