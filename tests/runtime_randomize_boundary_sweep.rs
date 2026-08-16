//! Runtime randomize boundary guardrail.
//!
//! This is a migration tripwire for Phase 5: every fixture that emits a
//! runtime randomization problem table must route generated randomize sites
//! through the runtime call-site setup, solve, and status-handling boundary.

use std::fs;
use std::path::{Path, PathBuf};

use harc::codegen::{cpp_tb, merge};
use harc::parser::parse_source;

#[test]
fn randomize_fixtures_use_runtime_boundary() {
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut paths: Vec<_> = fs::read_dir(&fixtures_dir)
        .expect("read fixtures dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("harc"))
        .collect();
    paths.sort();

    let mut emitted = 0usize;
    let mut runtime_randomize = 0usize;
    let mut runtime_call_sites = 0usize;
    let mut failures = Vec::new();

    for path in &paths {
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
        if should_skip_companion(name) {
            continue;
        }

        let Some(cpp) = emit_fixture(path, name, &mut failures) else {
            continue;
        };
        emitted += 1;

        if !cpp.contains("_harc_runtime_random_problem_table_entries[]") {
            continue;
        }
        runtime_randomize += 1;

        let mut missing = Vec::new();
        if !cpp.contains("_harc_runtime_random_problem_table_prepare_call(") {
            missing.push("generated prepare-call wrapper");
        }
        if !cpp.contains("harc_rt::random::harc_prepare_randomize_call(") {
            missing.push("runtime prepare-call helper");
        }

        let has_generated_call =
            cpp.contains("auto _harc_rt_call = _harc_runtime_random_problem_table_prepare_call(");
        if has_generated_call {
            runtime_call_sites += 1;
            if !cpp.contains("harc_rt::random::harc_solve_constrained(")
                && !cpp.contains("harc_rt::random::harc_solve_queued(")
            {
                missing.push("runtime solve entry");
            }
            if !cpp.contains("harc_rt::random::harc_handle_solve_status(") {
                missing.push("runtime solve-status handler");
            }
        }
        if cpp.contains("harc_find_problem(_harc_runtime_random_problem_table")
            || cpp.contains("harc_find_call_site(_harc_runtime_random_problem_table")
            || cpp.contains("harc_call_site_next_seed(*_harc_rt_site")
        {
            missing.push("no open-coded runtime lookup/seed boilerplate");
        }
        if !missing.is_empty() {
            failures.push(format!("[boundary] {name}: {}", missing.join(", ")));
        }
    }

    eprintln!(
        "[runtime_randomize_boundary sweep] emitted={emitted} runtime_randomize={runtime_randomize} runtime_call_sites={runtime_call_sites}"
    );

    assert!(
        failures.is_empty(),
        "runtime randomize boundary sweep failed:\n{}",
        failures.join("\n")
    );
    assert!(
        emitted >= 20,
        "runtime randomize boundary sweep only emitted {emitted} files"
    );
    assert!(
        runtime_randomize >= 5,
        "runtime randomize boundary sweep only found {runtime_randomize} runtime-randomize fixtures"
    );
    assert!(
        runtime_call_sites >= 5,
        "runtime randomize boundary sweep only found {runtime_call_sites} emitted runtime call-site fixtures"
    );
}

fn should_skip_companion(name: &str) -> bool {
    name.ends_with("_sim.harc") || name.ends_with("_domains.harc")
}

fn emit_fixture(path: &PathBuf, name: &str, failures: &mut Vec<String>) -> Option<String> {
    let src = fs::read_to_string(path).expect("read fixture");
    let parsed = match parse_source(&src) {
        Ok(parsed) => parsed,
        Err(err) => {
            failures.push(format!("[parse] {name}: {err:?}"));
            return None;
        }
    };

    let sim_sibling = path.with_file_name(format!("{}_sim.harc", name.trim_end_matches(".harc")));
    let parsed_units = if sim_sibling.exists() {
        let sim_src = fs::read_to_string(&sim_sibling).expect("read sim sibling");
        match parse_source(&sim_src) {
            Ok(sim) => vec![parsed.clone(), sim],
            Err(_) => vec![parsed.clone()],
        }
    } else {
        vec![parsed.clone()]
    };
    let to_emit = match merge::merge_for_sim(parsed_units, None) {
        Ok(merged) => merged,
        Err(_) => parsed,
    };

    match cpp_tb::emit(&to_emit) {
        Ok(cpp) => Some(cpp),
        Err(err) if benign_emit_error(&err.0) => None,
        Err(err) => {
            failures.push(format!("[emit] {name}: {}", err.0));
            None
        }
    }
}

fn benign_emit_error(msg: &str) -> bool {
    msg.contains("no `test` declaration")
        || msg.contains("let dut")
        || msg.contains("only non-sim impls")
        || msg.contains("no `impl sim`")
        || msg.contains("multiple tests")
        || msg.contains("is not a known bus binding")
        || msg.contains("no `domain") && msg.contains("declaration was found")
        || msg.contains("randomize(") && msg.contains("no `transaction")
        || msg.contains("constraint references unknown name")
        // A suspending bus/TLM method call inside a `log`/`fail` message
        // interpolation is a TB-IR-only capability (#494 P2d follow-up):
        // the legacy v1 emitter cannot resolve a bus-method call in a
        // message and fails with "bus ... has no signal or channel named
        // <method>". Such fixtures are exercised via the default (tbir)
        // backend in tests/run_fixtures.sh, not this v1 emit sweep.
        || msg.contains("has no signal or channel named")
}
