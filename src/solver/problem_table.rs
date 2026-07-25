//! Typed solver problem table extraction.
//!
//! This is the runtime-facing bridge for the staged migration: it walks a
//! parsed source file, recovers transaction templates and randomize sites,
//! lowers them into typed constraint IR, and attempts to build the Z3 scaffold.
//! Current codegen still uses `cpp_tb.rs` inline solver emission.

use std::collections::BTreeMap;

use crate::ast::{
    Block, CallArg, Expr, ExprKind, Item, SourceFile, Stmt, StmtKind, TestDecl, TestItem, TseqDecl,
    TypeExpr,
};
use crate::constraints::elaborate_constraints;
use crate::constraints::typed::{CTypedProblem, ConstraintProblemId};
use crate::constraints::typed_lower::{lower_problem, LowerError};
use crate::lexer::Span;
use crate::solver::z3::{Z3Backend, Z3Problem};
use crate::solver::{SolverBackend, SolverBuildError};

#[derive(Debug, Clone)]
pub struct TypedSolverProblemTable {
    pub entries: Vec<TypedSolverProblemEntry>,
}

#[derive(Debug, Clone)]
pub struct TypedSolverProblemEntry {
    pub source: TypedSolverProblemSource,
    pub build: TypedSolverProblemBuild,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypedSolverProblemSource {
    TransactionTemplate {
        transaction: String,
        span: Span,
    },
    RandomizeSite {
        context: String,
        target: String,
        transaction: String,
        blocking: bool,
        has_with_body: bool,
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub enum TypedSolverProblemBuild {
    Z3 {
        typed: Box<CTypedProblem>,
        z3: Box<Z3Problem>,
    },
    LowerError(Vec<LowerError>),
    BackendError(SolverBuildError),
}

impl TypedSolverProblemTable {
    pub fn z3_ready_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.build, TypedSolverProblemBuild::Z3 { .. }))
            .count()
    }

    pub fn lower_error_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.build, TypedSolverProblemBuild::LowerError(_)))
            .count()
    }

    pub fn backend_error_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.build, TypedSolverProblemBuild::BackendError(_)))
            .count()
    }
}

#[derive(Debug, Clone)]
struct RandomizeSite {
    txn_name: String,
    target: String,
    with_body: Vec<Expr>,
    blocking: bool,
    span: Span,
    context: String,
}

pub fn build_typed_solver_problem_table(file: &SourceFile) -> TypedSolverProblemTable {
    let elab = elaborate_constraints(file);
    let backend = Z3Backend;
    let mut entries = Vec::new();
    let mut next_problem_id = 1u32;

    for txn in elab.transactions.clone() {
        let source = TypedSolverProblemSource::TransactionTemplate {
            transaction: txn.name.clone(),
            span: txn.span,
        };
        let build = match lower_problem(
            &elab,
            &txn,
            None,
            Span::default(),
            ConstraintProblemId(next_problem_id),
        ) {
            Ok(problem) => match backend.build(&problem) {
                Ok(z3) => TypedSolverProblemBuild::Z3 {
                    typed: Box::new(problem),
                    z3: Box::new(z3),
                },
                Err(err) => TypedSolverProblemBuild::BackendError(err),
            },
            Err(errors) => TypedSolverProblemBuild::LowerError(errors),
        };
        entries.push(TypedSolverProblemEntry { source, build });
        next_problem_id += 1;
    }

    for site in collect_randomize_sites(file) {
        let Some(txn) = elab.transaction(&site.txn_name).cloned() else {
            continue;
        };
        let source = TypedSolverProblemSource::RandomizeSite {
            context: site.context.clone(),
            target: site.target.clone(),
            transaction: site.txn_name.clone(),
            blocking: site.blocking,
            has_with_body: !site.with_body.is_empty(),
            span: site.span,
        };
        let build = match lower_problem(
            &elab,
            &txn,
            Some(&site.with_body),
            site.span,
            ConstraintProblemId(next_problem_id),
        ) {
            Ok(problem) => match backend.build(&problem) {
                Ok(z3) => TypedSolverProblemBuild::Z3 {
                    typed: Box::new(problem),
                    z3: Box::new(z3),
                },
                Err(err) => TypedSolverProblemBuild::BackendError(err),
            },
            Err(errors) => TypedSolverProblemBuild::LowerError(errors),
        };
        entries.push(TypedSolverProblemEntry { source, build });
        next_problem_id += 1;
    }

    TypedSolverProblemTable { entries }
}

fn collect_randomize_sites(file: &SourceFile) -> Vec<RandomizeSite> {
    let mut out = Vec::new();
    for item in &file.items {
        match item {
            Item::Test(test) => collect_test_randomize_sites(test, &mut out),
            Item::Tseq(tseq) => collect_tseq_randomize_sites(tseq, &mut out),
            _ => {}
        }
    }
    out
}

fn collect_tseq_randomize_sites(tseq: &TseqDecl, out: &mut Vec<RandomizeSite>) {
    let mut env = BTreeMap::new();
    collect_block(
        &tseq.body,
        &mut env,
        &format!("tseq {}", tseq.name.name),
        out,
    );
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
            blocking,
            target,
            with_body,
        } => {
            collect_randomize_expr(*blocking, target, with_body, env, context, out);
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
                collect_call_arg(arg, env, context, out);
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

fn collect_call_arg(
    arg: &CallArg,
    env: &BTreeMap<String, String>,
    context: &str,
    out: &mut Vec<RandomizeSite>,
) {
    match arg {
        CallArg::Expr(expr) | CallArg::Named { value: expr, .. } => {
            collect_expr(expr, env, context, out);
        }
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
            blocking,
            target,
            with_body,
        } => {
            collect_randomize_expr(*blocking, target, with_body, env, context, out);
            for expr in with_body {
                collect_expr(expr, env, context, out);
            }
        }
        ExprKind::Call { callee, args } => {
            collect_expr(callee, env, context, out);
            for arg in args {
                collect_call_arg(arg, env, context, out);
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
        ExprKind::SoftConstraint(sc) => {
            collect_expr(&sc.expr, env, context, out);
            if let Some(weight) = &sc.weight {
                collect_expr(weight, env, context, out);
            }
        }
        ExprKind::DistDirective { target, entries } => {
            collect_expr(target, env, context, out);
            for entry in entries {
                collect_expr(&entry.value, env, context, out);
                collect_expr(&entry.weight, env, context, out);
            }
        }
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
    blocking: bool,
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
        target: id.name.clone(),
        with_body: with_body.to_vec(),
        blocking,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_source;

    #[test]
    fn builds_templates_and_randomize_sites() {
        let src = r#"
transaction Packet
    addr : uint<8>
    keep addr in [4..12]
end transaction Packet

test Smoke
    run
        let p : Packet
        randomize(p) with
            p.addr != 8
        end randomize
    end run
end test Smoke
"#;
        let file = parse_source(src).expect("parse");
        let table = build_typed_solver_problem_table(&file);
        assert_eq!(table.entries.len(), 2);
        assert_eq!(table.z3_ready_count(), 2);
        assert_eq!(table.lower_error_count(), 0);
        assert!(matches!(
            table.entries[0].source,
            TypedSolverProblemSource::TransactionTemplate { ref transaction, .. }
                if transaction == "Packet"
        ));
        assert!(matches!(
            table.entries[1].source,
            TypedSolverProblemSource::RandomizeSite {
                ref context,
                ref target,
                ref transaction,
                has_with_body: true,
                ..
            } if context == "Smoke: randomize(p)" && target == "p" && transaction == "Packet"
        ));
    }

    #[test]
    fn captures_lower_errors_without_panicking() {
        let src = r#"
transaction Packet
    addr : uint<8>
    keep addr == missing
end transaction Packet
"#;
        let file = parse_source(src).expect("parse");
        let table = build_typed_solver_problem_table(&file);
        assert_eq!(table.entries.len(), 1);
        assert_eq!(table.lower_error_count(), 1);
        assert!(matches!(
            table.entries[0].build,
            TypedSolverProblemBuild::LowerError(_)
        ));
    }
}
