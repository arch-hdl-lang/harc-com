//! Phase 4 scaffold parity sweep.
//!
//! This does not execute Z3 and does not replace `cpp_tb.rs`. It proves that
//! every fixture can be walked, every clean typed lowering can be handed to
//! the solver backend boundary, and unsupported backend cases are reported as
//! structured `SolverBuildError::Unsupported` entries rather than panics.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use harc::ast::{
    Block, CallArg, Expr, ExprKind, Item, Stmt, StmtKind, TestDecl, TestItem, TypeExpr,
};
use harc::constraints::elaborate_constraints;
use harc::constraints::typed::ConstraintProblemId;
use harc::constraints::typed_lower::lower_problem;
use harc::lexer::Span;
use harc::parser::parse_source;
use harc::solver::z3::Z3Backend;
use harc::solver::{SolverBackend, SolverBuildError};

#[test]
fn typed_z3_backend_builds_for_clean_fixture_lowers() {
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut entries: Vec<_> = fs::read_dir(&fixtures_dir)
        .expect("read fixtures dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("harc"))
        .collect();
    entries.sort_by_key(|e| e.path());

    let backend = Z3Backend;
    let mut total_fixtures = 0usize;
    let mut total_problems = 0usize;
    let mut z3_built = 0usize;
    let mut lower_errors = 0usize;
    let mut unsupported = Vec::new();

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
        let elab = elaborate_constraints(&parsed);

        for txn in elab.transactions.clone() {
            total_problems += 1;
            let lower = lower_problem(
                &elab,
                &txn,
                None,
                Span::default(),
                ConstraintProblemId(total_problems as u32),
            );
            match lower {
                Ok(problem) => match backend.build(&problem) {
                    Ok(z3) => {
                        assert!(
                            z3.smt.contains("(check-sat)"),
                            "Z3 scaffold output for {} bare {} missed check-sat",
                            path.display(),
                            txn.name
                        );
                        z3_built += 1;
                    }
                    Err(SolverBuildError::Unsupported { feature, detail }) => {
                        unsupported.push(format!(
                            "{} bare {}: {feature}: {detail}",
                            path.display(),
                            txn.name
                        ));
                    }
                    Err(SolverBuildError::Verify(errors)) => {
                        panic!(
                            "backend verifier rejected typed-lowered bare problem from {} {}: {errors:#?}",
                            path.display(),
                            txn.name
                        );
                    }
                },
                Err(_) => lower_errors += 1,
            }
        }

        let randomize_sites = collect_randomize_sites(&parsed.items);
        for site in randomize_sites {
            let Some(txn) = elab.transaction(&site.txn_name).cloned() else {
                continue;
            };
            total_problems += 1;
            let lower = lower_problem(
                &elab,
                &txn,
                Some(&site.with_body),
                site.span,
                ConstraintProblemId(total_problems as u32),
            );
            match lower {
                Ok(problem) => match backend.build(&problem) {
                    Ok(z3) => {
                        assert!(
                            z3.assertions.len() == problem.constraints.len(),
                            "assertion origin map lost clauses for {} {}",
                            path.display(),
                            site.context
                        );
                        z3_built += 1;
                    }
                    Err(SolverBuildError::Unsupported { feature, detail }) => {
                        unsupported.push(format!(
                            "{} {}: {feature}: {detail}",
                            path.display(),
                            site.context
                        ));
                    }
                    Err(SolverBuildError::Verify(errors)) => {
                        panic!(
                            "backend verifier rejected typed-lowered randomize problem from {} {}: {errors:#?}",
                            path.display(),
                            site.context
                        );
                    }
                },
                Err(_) => lower_errors += 1,
            }
        }
    }

    eprintln!(
        "[typed_z3 sweep] fixtures={total_fixtures} problems={total_problems} \
         z3_built={z3_built} lower_errors={lower_errors} unsupported={}",
        unsupported.len()
    );
    for line in unsupported.iter().take(12) {
        eprintln!("[typed_z3 unsupported] {line}");
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
}

#[derive(Debug, Clone)]
struct RandomizeSite {
    txn_name: String,
    with_body: Vec<Expr>,
    span: Span,
    context: String,
}

fn collect_randomize_sites(items: &[Item]) -> Vec<RandomizeSite> {
    let mut out = Vec::new();
    for item in items {
        let Item::Test(test) = item else {
            continue;
        };
        collect_test_randomize_sites(test, &mut out);
    }
    out
}

fn collect_test_randomize_sites(test: &TestDecl, out: &mut Vec<RandomizeSite>) {
    let mut env = BTreeMap::new();
    for item in &test.items {
        if let TestItem::Let(l) = item {
            if let Some(ty) = simple_type_name(l.ty.as_ref()) {
                env.insert(l.name.name.clone(), ty);
            }
        }
    }

    for item in &test.items {
        match item {
            TestItem::Stmt(stmt) => collect_stmt(stmt, &mut env.clone(), &test.name.name, out),
            TestItem::Scope(scope) => {
                for block in [
                    scope.setup.as_ref(),
                    scope.run.as_ref(),
                    scope.check.as_ref(),
                    scope.teardown.as_ref(),
                ]
                .into_iter()
                .flatten()
                {
                    collect_block(block, &mut env.clone(), &test.name.name, out);
                }
            }
            TestItem::Phase(_, block) => {
                collect_block(block, &mut env.clone(), &test.name.name, out)
            }
            TestItem::Let(_) | TestItem::Apply(_) | TestItem::Use(_) | TestItem::Clock(_) => {}
        }
    }
}

fn collect_block(
    block: &Block,
    env: &mut BTreeMap<String, String>,
    context: &str,
    out: &mut Vec<RandomizeSite>,
) {
    for stmt in &block.stmts {
        collect_stmt(stmt, env, context, out);
    }
}

fn collect_stmt(
    stmt: &Stmt,
    env: &mut BTreeMap<String, String>,
    context: &str,
    out: &mut Vec<RandomizeSite>,
) {
    match &stmt.kind {
        StmtKind::Let(l) => {
            if let Some(value) = &l.value {
                collect_expr(value, env, context, out);
            }
            if let Some(ty) = simple_type_name(l.ty.as_ref()) {
                env.insert(l.name.name.clone(), ty);
            }
        }
        StmtKind::Randomize {
            target, with_body, ..
        } => {
            collect_randomize_expr(target, with_body, env, context, out);
            for expr in with_body {
                collect_expr(expr, env, context, out);
            }
        }
        StmtKind::Assign { target, value } | StmtKind::Send { target, value } => {
            collect_expr(target, env, context, out);
            collect_expr(value, env, context, out);
        }
        StmtKind::For(f) => {
            collect_expr(&f.iter, env, context, out);
            collect_block(&f.body, &mut env.clone(), context, out);
        }
        StmtKind::Repeat(r) => {
            collect_expr(&r.count, env, context, out);
            collect_block(&r.body, &mut env.clone(), context, out);
        }
        StmtKind::Loop(block) => collect_block(block, &mut env.clone(), context, out),
        StmtKind::While { cond, body, .. } => {
            collect_expr(cond, env, context, out);
            collect_block(body, &mut env.clone(), context, out);
        }
        StmtKind::If(ifs) => {
            collect_expr(&ifs.cond, env, context, out);
            collect_block(&ifs.then_block, &mut env.clone(), context, out);
            for (cond, block) in &ifs.elsifs {
                collect_expr(cond, env, context, out);
                collect_block(block, &mut env.clone(), context, out);
            }
            if let Some(block) = &ifs.else_block {
                collect_block(block, &mut env.clone(), context, out);
            }
        }
        StmtKind::Fork(fork) => {
            for block in &fork.branches {
                collect_block(block, &mut env.clone(), context, out);
            }
        }
        StmtKind::Parallel(blocks) | StmtKind::Schedule(blocks) => {
            for block in blocks {
                collect_block(block, &mut env.clone(), context, out);
            }
        }
        StmtKind::Select(arms) => {
            for arm in arms {
                collect_expr(&arm.event, env, context, out);
                collect_block(&arm.action, &mut env.clone(), context, out);
            }
        }
        StmtKind::On(handler) => {
            collect_expr(&handler.event, env, context, out);
            collect_block(&handler.body, &mut env.clone(), context, out);
        }
        StmtKind::Emit { args, .. } | StmtKind::Log { args, .. } | StmtKind::LogF { args, .. } => {
            for arg in args {
                match arg {
                    CallArg::Expr(expr) | CallArg::Named { value: expr, .. } => {
                        collect_expr(expr, env, context, out);
                    }
                }
            }
        }
        StmtKind::Yield(expr) | StmtKind::Release(expr) | StmtKind::Fail { msg: expr, .. } => {
            collect_expr(expr, env, context, out);
        }
        StmtKind::Return(expr) => {
            if let Some(expr) = expr {
                collect_expr(expr, env, context, out);
            }
        }
        StmtKind::Assert(v) | StmtKind::Assume(v) | StmtKind::Cover(v) => {
            if let Some(expr) = &v.expr {
                collect_expr(expr, env, context, out);
            }
            if let Some(expr) = &v.else_fail {
                collect_expr(expr, env, context, out);
            }
        }
        StmtKind::After { duration, body, .. } => {
            collect_expr(duration, env, context, out);
            collect_block(body, &mut env.clone(), context, out);
        }
        StmtKind::Wait { duration, .. } => collect_expr(duration, env, context, out),
        StmtKind::WaitUntil {
            conditions,
            timeout,
            ..
        } => {
            for cond in conditions {
                collect_expr(cond, env, context, out);
            }
            if let Some(timeout) = timeout {
                collect_expr(&timeout.cycles, env, context, out);
                if let Some(msg) = &timeout.message {
                    collect_expr(msg, env, context, out);
                }
            }
        }
        StmtKind::Expr(expr) => collect_expr(expr, env, context, out),
        StmtKind::Apply(_)
        | StmtKind::Break { .. }
        | StmtKind::Continue { .. }
        | StmtKind::JoinAll { .. } => {}
    }
}

fn collect_expr(
    expr: &Expr,
    env: &BTreeMap<String, String>,
    context: &str,
    out: &mut Vec<RandomizeSite>,
) {
    match expr.kind.as_ref() {
        ExprKind::Randomize {
            target, with_body, ..
        } => {
            collect_randomize_expr(target, with_body, env, context, out);
            for expr in with_body {
                collect_expr(expr, env, context, out);
            }
        }
        ExprKind::Call { callee, args } => {
            collect_expr(callee, env, context, out);
            for arg in args {
                match arg {
                    CallArg::Expr(expr) | CallArg::Named { value: expr, .. } => {
                        collect_expr(expr, env, context, out);
                    }
                }
            }
        }
        ExprKind::Field { target, .. }
        | ExprKind::Cast { expr: target, .. }
        | ExprKind::Unary { expr: target, .. }
        | ExprKind::HashHash { expr: target, .. }
        | ExprKind::SeqRepeat { expr: target, .. }
        | ExprKind::Paren(target) => collect_expr(target, env, context, out),
        ExprKind::Index { target, index } => {
            collect_expr(target, env, context, out);
            collect_expr(index, env, context, out);
        }
        ExprKind::BitSlice { target, hi, lo } => {
            collect_expr(target, env, context, out);
            collect_expr(hi, env, context, out);
            collect_expr(lo, env, context, out);
        }
        ExprKind::Send { target, value }
        | ExprKind::Binary {
            lhs: target,
            rhs: value,
            ..
        } => {
            collect_expr(target, env, context, out);
            collect_expr(value, env, context, out);
        }
        ExprKind::ForkCall { call } => collect_expr(call, env, context, out),
        ExprKind::Ternary {
            cond,
            then_branch,
            else_branch,
        } => {
            collect_expr(cond, env, context, out);
            collect_expr(then_branch, env, context, out);
            collect_expr(else_branch, env, context, out);
        }
        ExprKind::RangeLit { lo, hi } => {
            if let Some(lo) = lo {
                collect_expr(lo, env, context, out);
            }
            if let Some(hi) = hi {
                collect_expr(hi, env, context, out);
            }
        }
        ExprKind::SetLit(items) => {
            for item in items {
                collect_expr(item, env, context, out);
            }
        }
        ExprKind::SystemCall { args, .. } => {
            for arg in args {
                collect_expr(arg, env, context, out);
            }
        }
        ExprKind::ForEachConstraint { iter, body, .. } => {
            collect_expr(iter, env, context, out);
            for expr in body {
                collect_expr(expr, env, context, out);
            }
        }
        ExprKind::DistDirective { target, .. } => collect_expr(target, env, context, out),
        ExprKind::NamedArg { value, .. } => collect_expr(value, env, context, out),
        ExprKind::StructLit { fields, .. } => {
            for field in fields {
                collect_expr(&field.value, env, context, out);
            }
        }
        ExprKind::CoverArrow { lhs, rhs, .. }
        | ExprKind::Membership {
            expr: lhs,
            set: rhs,
        } => {
            collect_expr(lhs, env, context, out);
            collect_expr(rhs, env, context, out);
        }
        ExprKind::SolveOrder { args } => {
            for arg in args {
                collect_expr(arg, env, context, out);
            }
        }
        ExprKind::DistLit(_)
        | ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Time(_)
        | ExprKind::Bool(_)
        | ExprKind::String(_)
        | ExprKind::Ident(_)
        | ExprKind::ImplicitSelf => {}
    }
}

fn collect_randomize_expr(
    target: &Expr,
    with_body: &[Expr],
    env: &BTreeMap<String, String>,
    context: &str,
    out: &mut Vec<RandomizeSite>,
) {
    let ExprKind::Ident(id) = target.kind.as_ref() else {
        return;
    };
    let Some(txn_name) = env.get(&id.name) else {
        return;
    };
    out.push(RandomizeSite {
        txn_name: txn_name.clone(),
        with_body: with_body.to_vec(),
        span: target.span,
        context: format!("{context}: randomize({})", id.name),
    });
}

fn simple_type_name(ty: Option<&TypeExpr>) -> Option<String> {
    let Some(TypeExpr::Named { name, .. }) = ty else {
        return None;
    };
    name.segments.last().map(|seg| seg.name.clone())
}
