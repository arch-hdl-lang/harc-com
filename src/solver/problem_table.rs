//! Typed solver problem table extraction.
//!
//! This is the runtime-facing bridge for the staged migration: it walks a
//! parsed source file, recovers transaction templates and randomize sites,
//! lowers them into typed constraint IR, and attempts to build the Z3 scaffold.
//! Current codegen still uses `cpp_tb.rs` inline solver emission.

use std::collections::BTreeMap;

use crate::ast::{
    Block, CallArg, ComponentItem, Expr, ExprKind, Item, SourceFile, SourceId, Stmt, StmtKind,
    TestDecl, TestItem, TseqDecl, TypeExpr,
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
        source_id: SourceId,
        span: Span,
    },
    RandomizeSite {
        context: String,
        target: String,
        transaction: String,
        blocking: bool,
        has_with_body: bool,
        source_id: SourceId,
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
    source_id: SourceId,
    span: Span,
    context: String,
}

pub fn build_typed_solver_problem_table(file: &SourceFile) -> TypedSolverProblemTable {
    let elab = elaborate_constraints(file);
    let backend = Z3Backend;
    let mut entries = Vec::new();
    let mut next_problem_id = 1u64;

    for txn in elab.transactions.clone() {
        let source_id = file
            .items
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                Item::Transaction(decl) if decl.name.name == txn.name => {
                    Some(file.item_source(index))
                }
                _ => None,
            })
            .unwrap_or_default();
        let source = TypedSolverProblemSource::TransactionTemplate {
            transaction: txn.name.clone(),
            source_id,
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

    push_site_entries(
        file,
        &elab,
        &backend,
        collect_randomize_sites(file),
        &mut entries,
    );

    TypedSolverProblemTable { entries }
}

/// A table over the randomize sites `build_typed_solver_problem_table`
/// does **not** collect: those in component method bodies, `on`
/// handlers, lifecycle phases, transactor bodies and free functions.
///
/// This is a **validation-only** table and deliberately a separate
/// function rather than more arms in `collect_randomize_sites`. The
/// main table assigns stable semantic `problem_id`s that both backends
/// bake into emitted symbol names. These entries are built purely to be
/// read for their `LowerError`s and never reach emission.
pub fn build_component_scope_problem_table(file: &SourceFile) -> TypedSolverProblemTable {
    let sites = collect_component_randomize_sites(file);
    // Collect first, and return before `elaborate_constraints` when
    // there is nothing to elaborate FOR. Its caller has already
    // elaborated the same file once for the main table, and a
    // component-scope randomize site is rare — one file in the 190 in
    // `tests/fixtures` has one — so this keeps the duplicate walk off
    // the path every other program takes.
    if sites.is_empty() {
        return TypedSolverProblemTable {
            entries: Vec::new(),
        };
    }
    let elab = elaborate_constraints(file);
    let backend = Z3Backend;
    let mut entries = Vec::new();
    push_site_entries(file, &elab, &backend, sites, &mut entries);
    TypedSolverProblemTable { entries }
}

fn push_site_entries(
    file: &SourceFile,
    elab: &crate::constraints::ConstraintElaboration,
    backend: &Z3Backend,
    sites: Vec<RandomizeSite>,
    entries: &mut Vec<TypedSolverProblemEntry>,
) {
    let mut occurrence_by_site = BTreeMap::<(String, String, String), u32>::new();
    let mut used_ids = entries
        .iter()
        .filter_map(|entry| match &entry.build {
            TypedSolverProblemBuild::Z3 { typed, .. } => Some(typed.problem_id.0),
            TypedSolverProblemBuild::LowerError(_) | TypedSolverProblemBuild::BackendError(_) => {
                None
            }
        })
        .collect::<std::collections::BTreeSet<_>>();
    for site in sites {
        let Some(txn) = elab
            .transaction(&site.txn_name)
            .or_else(|| elab.struct_schema(&site.txn_name))
            .cloned()
        else {
            continue;
        };
        let source = TypedSolverProblemSource::RandomizeSite {
            context: site.context.clone(),
            target: site.target.clone(),
            transaction: site.txn_name.clone(),
            blocking: site.blocking,
            has_with_body: !site.with_body.is_empty(),
            source_id: site.source_id,
            span: site.span,
        };
        let source_name = file
            .source_for_id(site.source_id)
            .map(|source| source.name.as_ref())
            .unwrap_or("<unknown>");
        let occurrence_key = (
            source_name.to_string(),
            site.context.clone(),
            site.txn_name.clone(),
        );
        let occurrence = occurrence_by_site.entry(occurrence_key).or_default();
        let mut collision = 0u32;
        let problem_id = loop {
            let id = stable_randomize_problem_id(source_name, &site, *occurrence, collision);
            if used_ids.insert(id) {
                break id;
            }
            collision += 1;
        };
        *occurrence += 1;
        let build = match lower_problem(
            elab,
            &txn,
            Some(&site.with_body),
            site.span,
            ConstraintProblemId(problem_id),
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
    }
}

fn stable_randomize_problem_id(
    source_name: &str,
    site: &RandomizeSite,
    occurrence: u32,
    collision: u32,
) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for part in [
        "harc-randomize-site-v1",
        source_name,
        site.context.as_str(),
        site.target.as_str(),
        site.txn_name.as_str(),
    ] {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    for byte in occurrence
        .to_le_bytes()
        .into_iter()
        .chain(collision.to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash | (1u64 << 63)
}

/// Randomize sites in the bodies `collect_randomize_sites` skips. Both
/// backends emit these through `cpp_tb::emit_randomize_for_site`, which
/// does its own constraint lowering, so a bad constraint here reaches
/// C++ under *both* codegens with no diagnostic from either.
fn collect_component_randomize_sites(file: &SourceFile) -> Vec<RandomizeSite> {
    let mut out = Vec::new();
    for (item_index, item) in file.items.iter().enumerate() {
        let source_id = file.item_source(item_index);
        match item {
            Item::Agent(c) | Item::Env(c) | Item::Scoreboard(c) | Item::Sequencer(c) => {
                let scope = component_field_scope([&c.items[..]]);
                collect_component_items(&c.items, &scope, &c.name.name, source_id, &mut out);
            }
            Item::Transactor(t) => {
                // ONE scope across both halves, not one per half.
                // `synth_component_from_transactor` concatenates the
                // always-present items and the `when active` block, so
                // a field declared in the shared half really is in
                // scope inside `when active` — walking the two halves
                // as independent scopes made those bodies collect zero
                // sites, the same empty-scope hole this seeding exists
                // to close.
                let active = t.when_active.as_deref().unwrap_or(&[]);
                let scope = component_field_scope([&t.items[..], active]);
                collect_component_items(&t.items, &scope, &t.name.name, source_id, &mut out);
                collect_component_items(active, &scope, &t.name.name, source_id, &mut out);
            }
            Item::Function(f) => {
                let mut env = BTreeMap::new();
                extend_env_with_params(&mut env, &f.params);
                collect_block(
                    &f.body,
                    &mut env,
                    &format!("function {}", f.name.name),
                    source_id,
                    &mut out,
                );
            }
            _ => {}
        }
    }
    out
}

/// The field-derived half of a component's name scope: every field's
/// declared type, plus the payload type of every `event<T>` field.
///
/// A randomize target is resolved by NAME through `env`, so a body
/// whose `env` is empty contributes nothing no matter how it is walked.
/// Statement-position `let`s are picked up by `collect_stmt` as it
/// descends, but a component body binds names three other ways that a
/// `test` body does not, and all three are ordinary shapes: a component
/// FIELD (`r : RegOp`), a method PARAMETER (`hookable go(r: RegOp)`),
/// and an `on` handler's event PAYLOAD (`req : event<RegOp>` +
/// `on req(t)` binds `t : RegOp`). Seeding only the `let`s collected
/// zero sites for all three.
fn component_field_scope<'a>(
    halves: impl IntoIterator<Item = &'a [ComponentItem]>,
) -> ComponentScope {
    let mut scope = ComponentScope::default();
    for item in halves.into_iter().flatten() {
        let ComponentItem::Field(f) = item else {
            continue;
        };
        if let Some(ty) = simple_type_name(Some(&f.ty)) {
            scope.fields.insert(f.name.name.clone(), ty);
        }
        if let Some(ty) = event_payload_type_name(&f.ty) {
            scope.payloads.insert(f.name.name.clone(), ty);
        }
    }
    scope
}

#[derive(Default)]
struct ComponentScope {
    fields: BTreeMap<String, String>,
    payloads: BTreeMap<String, String>,
}

fn collect_component_items(
    items: &[ComponentItem],
    scope: &ComponentScope,
    owner: &str,
    source_id: SourceId,
    out: &mut Vec<RandomizeSite>,
) {
    let (fields, payloads) = (&scope.fields, &scope.payloads);
    for item in items {
        let mut env = fields.clone();
        match item {
            ComponentItem::OnHandler(h) => {
                // `on <ev>(<binder>)` — the binder's type is the event
                // field's payload type. Any other trigger shape (a
                // cycle expression, a period) binds nothing.
                if let ExprKind::Call { callee, args } = h.event.kind.as_ref() {
                    if let ExprKind::Ident(ev) = callee.kind.as_ref() {
                        if let (Some(ty), [CallArg::Expr(a)]) = (payloads.get(&ev.name), &args[..])
                        {
                            if let ExprKind::Ident(binder) = a.kind.as_ref() {
                                env.insert(binder.name.clone(), ty.clone());
                            }
                        }
                    }
                }
                collect_block(&h.body, &mut env, owner, source_id, out);
            }
            ComponentItem::Hookable(h) => {
                extend_env_with_params(&mut env, &h.params);
                collect_block(&h.body, &mut env, owner, source_id, out);
            }
            ComponentItem::TargetTlmThread(t) => {
                extend_env_with_params(&mut env, &t.params);
                collect_block(&t.body, &mut env, owner, source_id, out);
            }
            // Reached only from a `testbench`, and a `testbench` is
            // either bound by `impl ... for` — in which case
            // `desugar_impl_for_test_in_file` has already folded these
            // blocks into the bound test, so the MAIN table collects
            // the same site (measured: one entry in each) — or unbound,
            // in which case `lower_program`'s item loop refuses the
            // component before this table is built. So the arm is
            // redundant today. It stays because its redundancy is a
            // property of the desugarer, not of this walk, and a
            // duplicate diagnostic costs a caller nothing: both tables
            // produce the same error and the first one wins.
            ComponentItem::Lifecycle(_, block) => {
                collect_block(block, &mut env, owner, source_id, out);
            }
            // `watchdog disabled` suppresses all codegen for the
            // block, body included, so nothing written there can reach
            // C++ and refusing it would reject a program neither
            // backend mis-lowers. The guard is belt-and-braces today:
            // the parser takes no body after `watchdog disabled` (see
            // the control in `every_component_body_that_can_host_a_
            // randomize_is_walked`), so `w.body` is empty there either
            // way. It stays so that allowing a body later cannot
            // silently start refusing a suppressed block.
            ComponentItem::Watchdog(w) if !w.disabled => {
                collect_block(&w.body, &mut env, owner, source_id, out);
            }
            ComponentItem::Field(_)
            | ComponentItem::Connect(_)
            | ComponentItem::Apply(_)
            | ComponentItem::Watchdog(_) => {}
        }
    }
}

fn extend_env_with_params(env: &mut BTreeMap<String, String>, params: &[crate::ast::Param]) {
    for p in params {
        bind(env, &p.name.name, p.ty.as_ref());
    }
}

/// Bind `name` to `ty`'s simple name — and UNBIND it when `ty` resolves
/// to nothing.
///
/// The unbind arm matters because these scopes now nest. A component
/// field seeds the scope before a method's parameters and `let`s are
/// walked, so `hookable go()` with `let r = 5` inside, in a component
/// that also declares `r : RegOp`, used to leave the field's binding
/// standing and record the site under the WRONG transaction. Nothing on
/// today's surfaced error set reads the target transaction, so the
/// verdict came out right anyway — but a wrong attribution that happens
/// not to matter is a wrong attribution, and it becomes a wrong refusal
/// the moment the surfaced set widens.
fn bind(env: &mut BTreeMap<String, String>, name: &str, ty: Option<&TypeExpr>) {
    match simple_type_name(ty) {
        Some(t) => {
            env.insert(name.to_string(), t);
        }
        None => {
            env.remove(name);
        }
    }
}

/// The `T` of an `event<T>` field type, when `T` is a plain named type.
fn event_payload_type_name(t: &TypeExpr) -> Option<String> {
    let TypeExpr::Builtin {
        name: crate::ast::BuiltinTy::Event,
        args,
        ..
    } = t
    else {
        return None;
    };
    // `event<RegOp>` parses its argument as an EXPRESSION, not a type
    // — `TypeArg::Expr(Ident("RegOp"))` — so reading only the
    // `TypeArg::Type` arm resolved nothing and collected zero sites.
    match args.first()? {
        crate::ast::TypeArg::Type(inner) => simple_type_name(Some(inner)),
        crate::ast::TypeArg::Expr(e) => match e.kind.as_ref() {
            ExprKind::Ident(id) => Some(id.name.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn collect_randomize_sites(file: &SourceFile) -> Vec<RandomizeSite> {
    let mut out = Vec::new();
    for (item_index, item) in file.items.iter().enumerate() {
        let source_id = file.item_source(item_index);
        match item {
            Item::Test(test) => collect_test_randomize_sites(test, source_id, &mut out),
            Item::Tseq(tseq) => collect_tseq_randomize_sites(tseq, source_id, &mut out),
            _ => {}
        }
    }
    out
}

fn collect_tseq_randomize_sites(
    tseq: &TseqDecl,
    source_id: SourceId,
    out: &mut Vec<RandomizeSite>,
) {
    let mut env = BTreeMap::new();
    collect_block(
        &tseq.body,
        &mut env,
        &format!("tseq {}", tseq.name.name),
        source_id,
        out,
    );
}

fn collect_test_randomize_sites(
    test: &TestDecl,
    source_id: SourceId,
    out: &mut Vec<RandomizeSite>,
) {
    let mut env = BTreeMap::new();
    for item in &test.items {
        if let TestItem::Let(l) = item {
            bind(&mut env, &l.name.name, l.ty.as_ref());
        }
    }

    for (item_index, item) in test.items.iter().enumerate() {
        let item_source = test.item_source(item_index);
        let item_source = if item_source.is_known() {
            item_source
        } else {
            source_id
        };
        match item {
            TestItem::Stmt(stmt) => {
                collect_stmt(stmt, &mut env.clone(), &test.name.name, item_source, out)
            }
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
                    collect_block(block, &mut env.clone(), &test.name.name, item_source, out);
                }
            }
            TestItem::Phase(_, block) => {
                collect_block(block, &mut env.clone(), &test.name.name, item_source, out)
            }
            TestItem::Let(_) | TestItem::Apply(_) | TestItem::Use(_) | TestItem::Clock(_) => {}
        }
    }
}

fn collect_block(
    block: &Block,
    env: &mut BTreeMap<String, String>,
    context: &str,
    fallback_source_id: SourceId,
    out: &mut Vec<RandomizeSite>,
) {
    for (index, stmt) in block.stmts.iter().enumerate() {
        let source_id = block.stmt_source(index);
        let source_id = if source_id.is_known() {
            source_id
        } else {
            fallback_source_id
        };
        collect_stmt(stmt, env, context, source_id, out);
    }
}

fn collect_stmt(
    stmt: &Stmt,
    env: &mut BTreeMap<String, String>,
    context: &str,
    source_id: SourceId,
    out: &mut Vec<RandomizeSite>,
) {
    match &stmt.kind {
        StmtKind::Let(l) => {
            if let Some(value) = &l.value {
                collect_expr(value, env, context, source_id, out);
            }
            bind(env, &l.name.name, l.ty.as_ref());
        }
        StmtKind::Randomize {
            blocking,
            target,
            with_body,
        } => {
            collect_randomize_expr(*blocking, target, with_body, env, context, source_id, out);
            for expr in with_body {
                collect_expr(expr, env, context, source_id, out);
            }
        }
        StmtKind::Assign { target, value } | StmtKind::Send { target, value } => {
            collect_expr(target, env, context, source_id, out);
            collect_expr(value, env, context, source_id, out);
        }
        StmtKind::For(f) => {
            collect_expr(&f.iter, env, context, source_id, out);
            collect_block(&f.body, &mut env.clone(), context, source_id, out);
        }
        StmtKind::Repeat(r) => {
            collect_expr(&r.count, env, context, source_id, out);
            collect_block(&r.body, &mut env.clone(), context, source_id, out);
        }
        StmtKind::Loop(block) => collect_block(block, &mut env.clone(), context, source_id, out),
        StmtKind::While { cond, body, .. } => {
            collect_expr(cond, env, context, source_id, out);
            collect_block(body, &mut env.clone(), context, source_id, out);
        }
        StmtKind::If(ifs) => {
            collect_expr(&ifs.cond, env, context, source_id, out);
            collect_block(&ifs.then_block, &mut env.clone(), context, source_id, out);
            for (cond, block) in &ifs.elsifs {
                collect_expr(cond, env, context, source_id, out);
                collect_block(block, &mut env.clone(), context, source_id, out);
            }
            if let Some(block) = &ifs.else_block {
                collect_block(block, &mut env.clone(), context, source_id, out);
            }
        }
        StmtKind::Fork(fork) => {
            for block in &fork.branches {
                collect_block(block, &mut env.clone(), context, source_id, out);
            }
        }
        StmtKind::Parallel(blocks) | StmtKind::Schedule(blocks) => {
            for block in blocks {
                collect_block(block, &mut env.clone(), context, source_id, out);
            }
        }
        StmtKind::Select(arms) => {
            for arm in arms {
                collect_expr(&arm.event, env, context, source_id, out);
                collect_block(&arm.action, &mut env.clone(), context, source_id, out);
            }
        }
        StmtKind::On(handler) => {
            collect_expr(&handler.event, env, context, source_id, out);
            collect_block(&handler.body, &mut env.clone(), context, source_id, out);
        }
        StmtKind::Emit { args, .. } | StmtKind::Log { args, .. } | StmtKind::LogF { args, .. } => {
            for arg in args {
                collect_call_arg(arg, env, context, source_id, out);
            }
        }
        StmtKind::Yield(expr) | StmtKind::Release(expr) | StmtKind::Fail { msg: expr, .. } => {
            collect_expr(expr, env, context, source_id, out);
        }
        StmtKind::Return(expr) => {
            if let Some(expr) = expr {
                collect_expr(expr, env, context, source_id, out);
            }
        }
        StmtKind::Assert(v) | StmtKind::Assume(v) | StmtKind::Cover(v) => {
            if let Some(expr) = &v.expr {
                collect_expr(expr, env, context, source_id, out);
            }
            if let Some(expr) = &v.else_fail {
                collect_expr(expr, env, context, source_id, out);
            }
        }
        StmtKind::After { duration, body, .. } => {
            collect_expr(duration, env, context, source_id, out);
            collect_block(body, &mut env.clone(), context, source_id, out);
        }
        StmtKind::Wait { duration, .. } => collect_expr(duration, env, context, source_id, out),
        StmtKind::WaitUntil {
            conditions,
            timeout,
            ..
        } => {
            for cond in conditions {
                collect_expr(cond, env, context, source_id, out);
            }
            if let Some(timeout) = timeout {
                collect_expr(&timeout.cycles, env, context, source_id, out);
                if let Some(msg) = &timeout.message {
                    collect_expr(msg, env, context, source_id, out);
                }
            }
        }
        StmtKind::Expr(expr) => collect_expr(expr, env, context, source_id, out),
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
    source_id: SourceId,
    out: &mut Vec<RandomizeSite>,
) {
    match arg {
        CallArg::Expr(expr) | CallArg::Named { value: expr, .. } => {
            collect_expr(expr, env, context, source_id, out);
        }
    }
}

fn collect_expr(
    expr: &Expr,
    env: &BTreeMap<String, String>,
    context: &str,
    source_id: SourceId,
    out: &mut Vec<RandomizeSite>,
) {
    match expr.kind.as_ref() {
        ExprKind::Randomize {
            blocking,
            target,
            with_body,
        } => {
            collect_randomize_expr(*blocking, target, with_body, env, context, source_id, out);
            for expr in with_body {
                collect_expr(expr, env, context, source_id, out);
            }
        }
        ExprKind::Call { callee, args } => {
            collect_expr(callee, env, context, source_id, out);
            for arg in args {
                collect_call_arg(arg, env, context, source_id, out);
            }
        }
        ExprKind::Field { target, .. }
        | ExprKind::Cast { expr: target, .. }
        | ExprKind::Unary { expr: target, .. }
        | ExprKind::HashHash { expr: target, .. }
        | ExprKind::SeqRepeat { expr: target, .. }
        | ExprKind::Paren(target) => collect_expr(target, env, context, source_id, out),
        ExprKind::Index { target, index } => {
            collect_expr(target, env, context, source_id, out);
            collect_expr(index, env, context, source_id, out);
        }
        ExprKind::BitSlice { target, hi, lo } => {
            collect_expr(target, env, context, source_id, out);
            collect_expr(hi, env, context, source_id, out);
            collect_expr(lo, env, context, source_id, out);
        }
        ExprKind::Send { target, value }
        | ExprKind::Binary {
            lhs: target,
            rhs: value,
            ..
        } => {
            collect_expr(target, env, context, source_id, out);
            collect_expr(value, env, context, source_id, out);
        }
        ExprKind::ForkCall { call } => collect_expr(call, env, context, source_id, out),
        ExprKind::Ternary {
            cond,
            then_branch,
            else_branch,
        } => {
            collect_expr(cond, env, context, source_id, out);
            collect_expr(then_branch, env, context, source_id, out);
            collect_expr(else_branch, env, context, source_id, out);
        }
        ExprKind::RangeLit { lo, hi } => {
            if let Some(lo) = lo {
                collect_expr(lo, env, context, source_id, out);
            }
            if let Some(hi) = hi {
                collect_expr(hi, env, context, source_id, out);
            }
        }
        ExprKind::SetLit(items) => {
            for item in items {
                collect_expr(item, env, context, source_id, out);
            }
        }
        ExprKind::SystemCall { args, .. } => {
            for arg in args {
                collect_expr(arg, env, context, source_id, out);
            }
        }
        ExprKind::ForEachConstraint { iter, body, .. } => {
            collect_expr(iter, env, context, source_id, out);
            for expr in body {
                collect_expr(expr, env, context, source_id, out);
            }
        }
        ExprKind::SoftConstraint(sc) => {
            collect_expr(&sc.expr, env, context, source_id, out);
            if let Some(weight) = &sc.weight {
                collect_expr(weight, env, context, source_id, out);
            }
        }
        ExprKind::DistDirective { target, entries } => {
            collect_expr(target, env, context, source_id, out);
            for entry in entries {
                collect_expr(&entry.value, env, context, source_id, out);
                collect_expr(&entry.weight, env, context, source_id, out);
            }
        }
        ExprKind::NamedArg { value, .. } => collect_expr(value, env, context, source_id, out),
        ExprKind::StructLit { fields, .. } => {
            for field in fields {
                collect_expr(&field.value, env, context, source_id, out);
            }
        }
        ExprKind::CoverArrow { lhs, rhs, .. }
        | ExprKind::Membership {
            expr: lhs,
            set: rhs,
        } => {
            collect_expr(lhs, env, context, source_id, out);
            collect_expr(rhs, env, context, source_id, out);
        }
        ExprKind::SolveOrder { args } => {
            for arg in args {
                collect_expr(arg, env, context, source_id, out);
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
    source_id: SourceId,
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
        source_id,
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

    fn randomize_problem_id(table: &TypedSolverProblemTable, context: &str) -> u64 {
        table
            .entries
            .iter()
            .find_map(|entry| match (&entry.source, &entry.build) {
                (
                    TypedSolverProblemSource::RandomizeSite {
                        context: candidate, ..
                    },
                    TypedSolverProblemBuild::Z3 { typed, .. },
                ) if candidate == context => Some(typed.problem_id.0),
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing randomize site {context}"))
    }

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

    #[test]
    fn randomize_problem_ids_are_owner_stable_and_site_unique() {
        let source = |include_alpha: bool| {
            format!(
                r#"
transaction Packet
    value : uint<8>
end transaction Packet

{}
test Bravo
    run
        let p : Packet
        randomize(p)
        randomize(p)
    end run
end test Bravo
"#,
                if include_alpha {
                    r#"test Alpha
    run
        let p : Packet
        randomize(p)
    end run
end test Alpha"#
                } else {
                    ""
                }
            )
        };

        let without_alpha = build_typed_solver_problem_table(
            &parse_source(&source(false)).expect("parse without Alpha"),
        );
        let with_alpha = build_typed_solver_problem_table(
            &parse_source(&source(true)).expect("parse with Alpha"),
        );

        let bravo_without = without_alpha
            .entries
            .iter()
            .filter_map(|entry| match (&entry.source, &entry.build) {
                (
                    TypedSolverProblemSource::RandomizeSite { context, .. },
                    TypedSolverProblemBuild::Z3 { typed, .. },
                ) if context == "Bravo: randomize(p)" => Some(typed.problem_id.0),
                _ => None,
            })
            .collect::<Vec<_>>();
        let bravo_with = with_alpha
            .entries
            .iter()
            .filter_map(|entry| match (&entry.source, &entry.build) {
                (
                    TypedSolverProblemSource::RandomizeSite { context, .. },
                    TypedSolverProblemBuild::Z3 { typed, .. },
                ) if context == "Bravo: randomize(p)" => Some(typed.problem_id.0),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(bravo_without, bravo_with);
        assert_eq!(bravo_with.len(), 2);
        assert_ne!(bravo_with[0], bravo_with[1]);
        assert_ne!(
            randomize_problem_id(&with_alpha, "Alpha: randomize(p)"),
            bravo_with[0]
        );
    }

    #[test]
    fn randomize_problem_id_survives_an_earlier_distinct_site_in_the_same_test() {
        let source = |include_earlier: bool| {
            format!(
                r#"
transaction Packet
    value : uint<8>
end transaction Packet

test Stable
    run
        let earlier : Packet
        let target : Packet
{}        randomize(target)
    end run
end test Stable
"#,
                if include_earlier {
                    "        randomize(earlier)\n"
                } else {
                    ""
                }
            )
        };

        let before = build_typed_solver_problem_table(
            &parse_source(&source(false)).expect("parse before insertion"),
        );
        let after = build_typed_solver_problem_table(
            &parse_source(&source(true)).expect("parse after insertion"),
        );
        assert_eq!(
            randomize_problem_id(&before, "Stable: randomize(target)"),
            randomize_problem_id(&after, "Stable: randomize(target)")
        );
    }
}
