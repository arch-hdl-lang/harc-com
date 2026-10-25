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
