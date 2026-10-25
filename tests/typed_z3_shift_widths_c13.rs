use std::io::Write;
use std::process::{Command, Stdio};

use harc::parser::parse_source;
use harc::solver::problem_table::{build_typed_solver_problem_table, TypedSolverProblemBuild};

fn rendered(width: u32, op: &str) -> String {
    rendered_with_lhs("uint<8>", width, op)
}

fn rendered_with_lhs(lhs_ty: &str, width: u32, op: &str) -> String {
    let source = format!(
        "transaction Packet\n x : {lhs_ty}\n n : uint<{width}>\n keep (x {op} n) == 0\nend transaction Packet\n"
    );
    let parsed = parse_source(&source).expect("shift source parses");
    let table = build_typed_solver_problem_table(&parsed);
    assert_eq!(table.entries.len(), 1);
    match table.entries.into_iter().next().unwrap().build {
        TypedSolverProblemBuild::Z3 { z3, .. } => z3.smt,
        other => panic!("expected a Z3 problem, got {other:?}"),
    }
}

fn z3_via_libz3(smt: &str) -> String {
    let root = env!("CARGO_MANIFEST_DIR");
    let script = r#"import importlib.util, pathlib, sys
path = pathlib.Path(sys.argv[1]) / "tests" / "conformance" / "arithmetic_solver.py"
spec = importlib.util.spec_from_file_location("arithmetic_solver", path)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
print(module.SmtRunner().check(sys.stdin.read()), end="")
"#;
    let mut child = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("python3 is required for libz3 regression");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(smt.as_bytes())
        .expect("write SMT to libz3 runner");
    let output = child.wait_with_output().expect("wait for libz3 runner");
    assert!(
        output.status.success(),
        "libz3 runner failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("libz3 output is UTF-8")
        .trim()
        .to_string()
}

fn pin_fields(smt: &str, rhs_width: u32, x: u32, n: u32) -> String {
    let assertions =
        format!("(assert (= f_x (_ bv{x} 8)))\n(assert (= f_n (_ bv{n} {rhs_width})))\n");
    smt.replacen("(check-sat)\n", &format!("{assertions}(check-sat)\n"), 1)
}

#[test]
fn mixed_width_shifts_render_same_sort_operands() {
    let narrow_left = rendered(4, "<<");
    assert!(narrow_left.contains("(bvshl f_x ((_ zero_extend 4) f_n))"));

    let narrow_right = rendered(4, ">>");
    assert!(narrow_right.contains("(bvlshr f_x ((_ zero_extend 4) f_n))"));

    let equal = rendered(8, "<<");
    assert!(equal.contains("(bvshl f_x f_n)"));

    let wide_left = rendered(16, "<<");
    assert!(wide_left.contains("((_ extract 7 0) (bvshl ((_ zero_extend 8) f_x) f_n))"));

    let wide_right = rendered(16, ">>");
    assert!(wide_right.contains("((_ extract 7 0) (bvlshr ((_ zero_extend 8) f_x) f_n))"));

    let signed_wide_right = rendered_with_lhs("sint<8>", 16, ">>");
    assert!(signed_wide_right.contains("((_ extract 7 0) (bvashr ((_ sign_extend 8) f_x) f_n))"));
}

#[test]
fn mixed_width_shifts_execute_with_libz3_and_preserve_wide_counts() {
    for (width, op) in [(4, "<<"), (4, ">>"), (8, "<<"), (16, "<<"), (16, ">>")] {
        let smt = rendered(width, op);
        assert_eq!(z3_via_libz3(&smt), "sat", "baseline {op} uint<{width}>");
        assert_eq!(
            z3_via_libz3(&pin_fields(&smt, width, 1, 0)),
            "unsat",
            "inverse case {op} uint<{width}>"
        );
    }

    for op in ["<<", ">>"] {
        let smt = rendered(16, op);
        assert_eq!(
            z3_via_libz3(&pin_fields(&smt, 16, 1, 256)),
            "sat",
            "wide count 256 must remain an oversized shift for {op}"
        );
    }
}
