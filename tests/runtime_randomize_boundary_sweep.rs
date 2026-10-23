//! TBIR runtime-randomization boundary guardrail.
//!
//! Every lowerable fixture that emits a runtime randomization problem table
//! must route generated randomize sites through the runtime call-site setup,
//! solve, and status-handling boundary.

use std::fs;
use std::path::{Path, PathBuf};

use harc::codegen::{cpp_tb, merge, tbir};
use harc::ir::{lower, verify};
use harc::parser::parse_source;

#[test]
fn tbir_randomize_fixtures_use_runtime_boundary() {
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut paths: Vec<_> = fs::read_dir(&fixtures_dir)
        .expect("read fixtures dir")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("harc"))
        .collect();
    paths.sort();

    let mut emitted = 0usize;
    let mut runtime_randomize = 0usize;
    let mut runtime_call_sites = 0usize;
    let mut failures = Vec::new();

    for path in &paths {
        let name = path
            .file_name()
            .and_then(|part| part.to_str())
            .unwrap_or("?");
        if should_skip_companion(name) {
            continue;
        }

        let Some(cpp) = emit_fixture(path, name, &mut failures) else {
            continue;
        };
        emitted += 1;

        for forbidden in [
            "thread_local",
            "static harc_rt::random::HarcRng",
            "static harc_rt::random::HarcRuntimeCallSite",
            "static harc_rt::random::HarcUniqueHistory",
            "static harc_rt::random::HarcAutoCovState",
            "static uint64_t _cov_",
            "Verilated::threadContextp()",
        ] {
            if cpp.contains(forbidden) {
                failures.push(format!(
                    "[run-state] {name}: generated mutable or ambient state `{forbidden}`"
                ));
            }
        }

        let solve_sites = cpp
            .matches("harc_rt::random::harc_solve_constrained(")
            .count()
            + cpp.matches("harc_rt::random::harc_solve_queued(").count();
        if solve_sites == 0 {
            continue;
        }
        runtime_randomize += 1;
        runtime_call_sites += solve_sites;

        let mut missing = Vec::new();
        if !cpp.contains("harc_rt::random::HarcRuntimeCallSite") {
            missing.push("per-run call-site state");
        }
        if !cpp.contains("harc_prepare_randomize_call(")
            && !cpp.contains("harc_rt::random::harc_call_site_next_seed(")
        {
            missing.push("runtime call-site seed boundary");
        }
        if !cpp.contains("harc_rt::random::harc_handle_solve_status(") {
            missing.push("runtime solve-status handler");
        }
        if !missing.is_empty() {
            failures.push(format!("[boundary] {name}: {}", missing.join(", ")));
        }
    }

    eprintln!(
        "[TBIR runtime-randomize boundary sweep] emitted={emitted} \
         runtime_randomize={runtime_randomize} runtime_call_sites={runtime_call_sites}"
    );
    assert!(
        failures.is_empty(),
        "TBIR runtime-randomize boundary sweep failed:\n{}",
        failures.join("\n")
    );
    assert!(emitted >= 20, "sweep only emitted {emitted} fixtures");
    assert!(
        runtime_randomize >= 5,
        "sweep only found {runtime_randomize} runtime-randomize fixtures"
    );
    assert!(
        runtime_call_sites >= 5,
        "sweep only found {runtime_call_sites} runtime call-site fixtures"
    );
}

fn should_skip_companion(name: &str) -> bool {
    name.ends_with("_sim.harc") || name.ends_with("_domains.harc")
}

fn emit_fixture(path: &PathBuf, name: &str, failures: &mut Vec<String>) -> Option<String> {
    let source = fs::read_to_string(path).expect("read fixture");
    let parsed = match parse_source(&source) {
        Ok(parsed) => parsed,
        Err(_) => return None,
    };

    let sim_sibling = path.with_file_name(format!("{}_sim.harc", name.trim_end_matches(".harc")));
    let parsed_units = if sim_sibling.exists() {
        let sim_source = fs::read_to_string(&sim_sibling).expect("read sim sibling");
        match parse_source(&sim_source) {
            Ok(sim) => vec![parsed.clone(), sim],
            Err(_) => vec![parsed.clone()],
        }
    } else {
        vec![parsed.clone()]
    };
    let merged = match merge::merge_for_sim(parsed_units, None) {
        Ok(merged) => merged,
        Err(_) => return None,
    };
    let program = match lower::lower_program(&merged) {
        Ok(program) => program,
        Err(_) => return None,
    };
    if let Err(error) = verify::verify_program(&program) {
        failures.push(format!("[verify] {name}: {error:?}"));
        return None;
    }
    match tbir::emit(&program, &merged, &cpp_tb::EmitOpts::default()) {
        Ok(cpp) => Some(cpp),
        Err(error) => {
            failures.push(format!("[emit] {name}: {}", error.0));
            None
        }
    }
}
