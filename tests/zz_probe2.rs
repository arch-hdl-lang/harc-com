//! THROWAWAY probe test 2 — delete before finishing.

use harc::codegen::{cpp_tb, merge, tbir};
use harc::ir::{lower, verify};
use harc::parser::parse_source;

fn merged_src(src: &str) -> harc::ast::SourceFile {
    let parsed = parse_source(src).expect("fixture parses");
    merge::merge_for_sim(vec![parsed], None).expect("merge")
}

fn src_scalar(clause: &str) -> String {
    format!(
        "domain D\n  freq_mhz: 100\nend domain D\n\n\
         transaction P\n    n     : uint<8>\n    m     : uint<8>\n\
         end transaction P\n\n\
         test T\n    let dut : Top\n    let p : P\n    clock clk = D\n    run\n\
         \x20       randomize(p) with\n            {clause}\n        end randomize\n\
         \x20   end run\nend test T\n"
    )
}

#[test]
fn probe_emitted_cpp_for_scalar_sum() {
    for clause in [
        "sum(p.n) == 1",
        "p.n == 1",
        "sum(p.n) == 1\n            p.m == 7",
    ] {
        let src = src_scalar(clause);
        let merged = merged_src(&src);
        println!("======== clause: {clause:?}");
        match lower::lower_program(&merged) {
            Err(e) => println!("  tbir lower ERR: {e}"),
            Ok(prog) => {
                match verify::verify_program(&prog) {
                    Ok(()) => println!("  tbir verify OK"),
                    Err(e) => println!("  tbir verify ERR: {e:?}"),
                }
                match tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()) {
                    Err(e) => println!("  tbir emit ERR: {e}"),
                    Ok(cpp) => {
                        let lines: Vec<&str> = cpp
                            .lines()
                            .filter(|l| {
                                l.contains("_s.add") || l.contains("_z_n") || l.contains("bool_val")
                            })
                            .collect();
                        println!("  tbir emit OK; solver lines:");
                        for l in lines {
                            println!("      {}", l.trim());
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn probe_constraint_table_entries() {
    for clause in [
        "sum(p.n) == 1",
        "sum(p.n)",
        "sum(x: p.n) == 1",
        "sum(p.n, p.m) == 1",
    ] {
        let src = src_scalar(clause);
        if parse_source(&src).is_err() {
            println!("[{clause}] PARSE ERROR");
            continue;
        }
        let merged = merged_src(&src);
        let table = harc::solver::problem_table::build_typed_solver_problem_table(&merged);
        println!("[{clause}]");
        for e in &table.entries {
            match &e.build {
                harc::solver::problem_table::TypedSolverProblemBuild::LowerError(v) => {
                    for er in v {
                        println!("    err rel={} {:?}", er.is_relation_error(), er);
                    }
                }
                other => println!("    build = {other:?}"),
            }
        }
        println!(
            "    v1 = {:?}",
            cpp_tb::emit(&merged)
                .map(|_| "OK")
                .map_err(|e| format!("{e}"))
        );
        println!(
            "    tbir lower = {:?}",
            lower::lower_program(&merged)
                .map(|_| "LOWERS")
                .map_err(|e| format!("{e}"))
        );
    }
}
