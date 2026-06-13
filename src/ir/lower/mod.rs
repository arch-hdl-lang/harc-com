//! AST → TB-IR lowering (docs/tb-ir-design.md §"AST → IR lowering rules").
//!
//! MVP scope: classic-form and impl-for testbench-bound tests with a
//! single DUT, declared clocks, and the core statement subset (DUT
//! port read/write, `let`/assign, `log`/`logf`, inline `assert ...
//! else fail`, `fail`, `wait N cycles`, `wait until` in its single
//! and `all of` forms with optional `timeout`, `if`/`for`/`while`/
//! `repeat`/`loop`/`break`/`continue`). Everything else is rejected
//! with
//! `LowerError::Unsupported` naming the construct — lowering NEVER
//! silently mis-lowers. Re-run with `--codegen v1` to use the legacy
//! direct AST → C++ path for unsupported constructs.

mod bus;
mod components;
mod control;
mod covergroups;
mod exprs;
mod helpers;
mod records;
mod regblock;
mod scoreboards;
mod stmts;
mod tseqs;
mod transactors;

use crate::ast::{
    Block, BuiltinTy, BusDecl, ClockDecl, ComponentDecl, ComponentItem, ExprKind,
    HookableMethod, Item, ScopeDecl, SourceFile, Stmt as AstStmt, StmtKind, TestDecl, TestItem,
    TransactorMode, TypeExpr,
};
use crate::ir::{
    self, BasicBlock, BlockId, ClockSpec, ComponentSchema, ConstraintRef, ConstraintSite,
    CovgroupId, CovgroupSchema, FunctionId, FunctionKind, IrType, LocalId, RecordId, RecordSchema,
    RegblockId, ScoreboardId, ScoreboardSchema, TbFunction, TbProgram, TestSchema, TestbenchId,
    TestbenchSchema, Terminator, TransactorId, TransactorSchema, TypedLocal, TypedParam,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub enum LowerError {
    /// The construct is outside the TB-IR MVP subset.
    Unsupported { construct: String, detail: String },
    /// Structurally invalid input (would also fail v1 codegen).
    Invalid(String),
}

impl std::fmt::Display for LowerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LowerError::Unsupported { construct, detail } => {
                write!(
                    f,
                    "TB-IR lowering does not support {construct} yet"
                )?;
                if !detail.is_empty() {
                    write!(f, " ({detail})")?;
                }
                write!(f, "; re-run with `--codegen v1`")
            }
            LowerError::Invalid(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for LowerError {}

pub(crate) fn unsupported(construct: &str, detail: impl Into<String>) -> LowerError {
    LowerError::Unsupported {
        construct: construct.to_string(),
        detail: detail.into(),
    }
}

/// Lower a merged source file (post `merge_for_sim`) into a verified-
/// shape `TbProgram`. Callers should run `verify::verify_program` on
/// the result before emission.
pub fn lower_program(file: &SourceFile) -> Result<TbProgram, LowerError> {
    // Capture impl-form testbench bindings BEFORE desugaring clears
    // `for_testbench`.
    let mut tb_of_test: HashMap<String, String> = HashMap::new();
    for it in &file.items {
        if let Item::Test(t) = it {
            if let Some(tb) = &t.for_testbench {
                tb_of_test.insert(t.name.name.clone(), tb.name.clone());
            }
        }
    }

    // Reuse v1's impl-for desugaring so both codegens see the exact
    // same classic-form AST (synthesized `let dut` / `let _tb`, merged
    // lifecycle blocks, `_tb.<field>` rewrites).
    let file = crate::codegen::cpp_tb::desugar_impl_for_test_in_file(file);

    // Domain table: `domain D freq_mhz: N` → period_ps = 1_000_000 / N.
    let mut domains: HashMap<String, i64> = HashMap::new();
    for it in &file.items {
        if let Item::Domain(d) = it {
            for f in &d.fields {
                if f.name.name == "freq_mhz" {
                    if let ExprKind::Int(s) = &*f.value.kind {
                        if let Ok(n) = s.replace('_', "").parse::<i64>() {
                            if n > 0 {
                                domains.insert(d.name.name.clone(), 1_000_000 / n);
                            }
                        }
                    }
                }
            }
        }
    }

    // Testbench components referenced by impl-form tests.
    let mut components: HashMap<String, &ComponentDecl> = HashMap::new();
    for it in &file.items {
        // Scoreboards are NOT in this map — they lower to their own
        // schema table (`scoreboard_ids`) and bind as testbench fields,
        // not as `impl ... for`-bound composite components.
        if let Item::Env(c) | Item::Agent(c) | Item::Sequencer(c) = it {
            components.insert(c.name.name.clone(), c);
        }
    }

    // Bus declarations (inline or `use`-imported — the import resolver
    // appended the parsed stdlib file before merge_for_sim, so both
    // arrive here as plain items). Declarations are inert until a test
    // binds them; unsupported bus features are rejected at the bind /
    // access site, not here.
    let mut buses: HashMap<String, &BusDecl> = HashMap::new();
    for it in &file.items {
        if let Item::Bus(b) = it {
            buses.insert(b.name.name.clone(), b);
        }
    }
    let used_tbs: HashSet<&String> = tb_of_test.values().collect();

    // File-scope named integer constants: `const NAME : Ty = <lit>`
    // (v1: `static constexpr <cty> NAME = <expr>;`) and `enum Color {
    // RED, ... }` variant names (v1: variant index, first definition
    // wins). Both substitute as plain integer literals at use sites —
    // observably identical to v1's constexpr/index emission. `const`
    // initializers outside plain integer literals are rejected (v1
    // forwards arbitrary exprs to C++; the IR subset keeps literals).
    let mut consts: HashMap<String, u64> = HashMap::new();
    for it in &file.items {
        match it {
            Item::Const(c) => {
                let ExprKind::Int(s) = &*c.value.kind else {
                    return Err(unsupported(
                        &format!("`const {}` with a non-integer-literal initializer", c.name.name),
                        "",
                    ));
                };
                let Some(v) = exprs::parse_int_literal(s) else {
                    return Err(unsupported(
                        &format!("`const {}` initializer `{s}`", c.name.name),
                        "not a plain integer literal",
                    ));
                };
                consts.insert(c.name.name.clone(), v);
            }
            Item::Enum(e) => {
                for (i, v) in e.variants.iter().enumerate() {
                    // First definition wins across enums — v1's
                    // `enum_variants.entry(..).or_insert(i)`.
                    consts.entry(v.name.clone()).or_insert(i as u64);
                }
            }
            _ => {}
        }
    }
    // Enum names, so transaction fields of enum type lower as scalars
    // (v1 flattens them to `int64_t` members with index values).
    let enum_names: HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Enum(e) => Some(e.name.name.clone()),
            _ => None,
        })
        .collect();

    // Covergroup schemas, in file order. All declarations lower (even
    // unreferenced ones — v1 emits a struct for each), so unsupported
    // covergroup features are rejected here rather than dropped.
    let mut covgroup_ids: HashMap<String, CovgroupId> = HashMap::new();
    let mut covgroups: Vec<CovgroupSchema> = Vec::new();
    for it in &file.items {
        if let Item::Covergroup(g) = it {
            let schema = covergroups::lower_covergroup(g)?;
            covgroup_ids.insert(g.name.name.clone(), CovgroupId(covgroups.len() as u32));
            covgroups.push(schema);
        }
    }
    // Record schemas (`transaction` declarations), in file order. All
    // declarations lower (even unreferenced ones — v1 emits a struct
    // for each), so unsupported transaction shapes are rejected here
    // rather than dropped.
    let mut record_ids: HashMap<String, RecordId> = HashMap::new();
    let mut record_schemas: Vec<RecordSchema> = Vec::new();
    for it in &file.items {
        if let Item::Transaction(t) = it {
            let schema = records::lower_transaction(t, &enum_names)?;
            record_ids.insert(t.name.name.clone(), RecordId(record_schemas.len() as u32));
            record_schemas.push(schema);
        }
    }
    // `struct` declarations lower into the SAME records table — a struct is
    // the shared value-record shape (v1's `emit_struct_record` routes
    // through `emit_record_struct`, exactly as transactions do), so a
    // `let r : S` resolves `S` via `record_ids` and every record-local op
    // (`RecordInit` / `RecordFieldWrite` / `Expr::RecordField`) works for
    // free. A name shared with a transaction would resolve ambiguously, so
    // reject the collision rather than shadow.
    for it in &file.items {
        if let Item::Struct(s) = it {
            let name = &s.name.name;
            if record_ids.contains_key(name) {
                return Err(LowerError::Invalid(format!(
                    "struct `{name}` collides with a transaction or struct of the same name"
                )));
            }
            let schema = records::lower_struct(s, &enum_names)?;
            record_ids.insert(name.clone(), RecordId(record_schemas.len() as u32));
            record_schemas.push(schema);
        }
    }
    // Regblock schemas (`regblock` declarations), in file order. The
    // mirror is a synthetic value-record (one scalar field per
    // register), pushed into the records table right after the
    // transactions so its `RecordId` is stable; the `RegblockSchema`
    // carries the offset/width/access metadata access lowering needs.
    // The regblock name doubles as the mirror record's name, so a
    // `let regs : R` resolves `R` to the synthetic record via
    // `record_ids` exactly like a transaction local.
    let mut regblock_ids: HashMap<String, RegblockId> = HashMap::new();
    let mut regblock_schemas: Vec<ir::RegblockSchema> = Vec::new();
    for it in &file.items {
        if let Item::Regblock(r) = it {
            let name = &r.name.name;
            if record_ids.contains_key(name) {
                return Err(LowerError::Invalid(format!(
                    "regblock `{name}` collides with a transaction or struct of the same name"
                )));
            }
            let rec_id = RecordId(record_schemas.len() as u32);
            let (rec, schema) = regblock::lower_regblock(r, rec_id)?;
            record_ids.insert(name.clone(), rec_id);
            record_schemas.push(rec);
            regblock_ids.insert(name.clone(), RegblockId(regblock_schemas.len() as u32));
            regblock_schemas.push(schema);
        }
    }

    // `tseq` (transaction-sequence) declarations: name → element record
    // type. Validated up front (the element type must be a declared
    // record); the bodies lower to `FunctionKind::Tseq` functions after
    // the pure helpers (so FunctionIds stay sequential). Threaded into
    // every `LowerCtx` so a `let txns = Name(...)` resolves the call edge
    // and a `for t in txns` resolves the iteration.
    let tseq_records = tseqs::collect_tseq_records(&file, &record_ids)?;

    // Helper functions: categorize pure vs impure, reject recursion.
    // Pure helpers lower eagerly below; impure helpers are CFG-inlined
    // at each call site during body lowering.
    let helper_registry = helpers::HelperRegistry::build(&file)?;

    // Transactor names, for the file gate + testbench-field validation
    // (schemas lower after pure helpers so FunctionIds line up).
    let mut transactor_ids: HashMap<String, TransactorId> = HashMap::new();
    let mut n_transactors = 0u32;
    for it in &file.items {
        if let Item::Transactor(t) = it {
            // A pure analysis-source transactor (event port + no DUT
            // field) routes to the composite-component table instead of
            // the DUT-poking `TransactorSchema` (classified below).
            if components::transactor_is_component(t) {
                continue;
            }
            if transactor_ids
                .insert(t.name.name.clone(), TransactorId(n_transactors))
                .is_some()
            {
                return Err(LowerError::Invalid(format!(
                    "duplicate transactor declaration `{}`",
                    t.name.name
                )));
            }
            n_transactors += 1;
        }
    }

    // Scoreboard schemas (`scoreboard` declarations), in file order. All
    // declarations lower (even unreferenced ones — v1 emits a struct for
    // each), so unsupported scoreboard shapes are rejected here rather
    // than dropped. The ids feed the testbench-field validation below.
    let mut scoreboard_ids: HashMap<String, ScoreboardId> = HashMap::new();
    let mut scoreboard_schemas: Vec<ScoreboardSchema> = Vec::new();
    for it in &file.items {
        if let Item::Scoreboard(c) = it {
            // A method-bearing scoreboard needs per-instance state, so it
            // routes to the composite-component table instead of the
            // data-only `ScoreboardSchema` (classified below).
            if components::scoreboard_is_component(c) {
                continue;
            }
            let schema = scoreboards::lower_scoreboard(c)?;
            if scoreboard_ids
                .insert(c.name.name.clone(), ScoreboardId(scoreboard_schemas.len() as u32))
                .is_some()
            {
                return Err(LowerError::Invalid(format!(
                    "duplicate scoreboard declaration `{}`",
                    c.name.name
                )));
            }
            scoreboard_schemas.push(schema);
        }
    }

    // The set of every composite-component type that lowers to a
    // `ComponentSchema` (env, method-bearing scoreboard, analysis-source
    // transactor, agent). A field of one of these types is a
    // composite-component binding — valid both as a test-scope `let` and
    // (since the testbench-field-binding slice) as a `testbench` field.
    // Must stay in lockstep with the `comp_sources` classification below.
    let component_type_names: HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Scoreboard(c) if components::scoreboard_is_component(c) => {
                Some(c.name.name.clone())
            }
            Item::Transactor(t) if components::transactor_is_component(t) => {
                Some(t.name.name.clone())
            }
            Item::Env(c) if matches!(c.kind, crate::ast::ComponentKind::Env) => {
                Some(c.name.name.clone())
            }
            Item::Agent(c) if matches!(c.kind, crate::ast::ComponentKind::Agent) => {
                Some(c.name.name.clone())
            }
            Item::Sequencer(c) => Some(c.name.name.clone()),
            _ => None,
        })
        .collect();

    // The subset of composite-component types that are event-driven
    // *transactors* (`in event<T>` + `on <ev>` consumer BFM). They route
    // to a `ComponentSchema` but, being transactors, accept an
    // `active`/`passive` instance mode at a binding site (the mode just
    // selects whether the `when active` body is included — always, in
    // this subset, so `active` is required and `passive` rejected).
    let event_driven_transactor_names: HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Transactor(t) if components::transactor_is_event_driven(t) => {
                Some(t.name.name.clone())
            }
            _ => None,
        })
        .collect();

    // File-level construct gate: anything outside the MVP subset is an
    // explicit Unsupported, never silently dropped.
    for it in &file.items {
        match it {
            Item::Use(_)
            | Item::Domain(_)
            | Item::Test(_)
            | Item::Covergroup(_)
            | Item::Function(_)
            | Item::Transaction(_)
            | Item::Const(_)
            | Item::Enum(_)
            // Struct declarations already lowered to record schemas above
            // (with their own Unsupported rejections); inert here.
            | Item::Struct(_)
            | Item::Bus(_)
            | Item::Transactor(_)
            // Scoreboard declarations already lowered to schemas above
            // (with their own Unsupported rejections); they are inert
            // until a testbench binds one as a field.
            | Item::Scoreboard(_)
            | Item::Regblock(_) => {}
            // `extend` was already folded by merge_for_sim; a survivor
            // means the merge didn't apply (e.g. dump-ir on a lone
            // extension file).
            Item::Extend(_) => {
                return Err(unsupported(
                    "an unmerged `extend` block",
                    "pass the base test file alongside the extension",
                ));
            }
            // A `sequencer` is a composite component (analysis-source
            // shape: `out event<T>` + hookable `emit` methods). It is
            // lowered through the component-cluster path below and binds
            // only as a test-scope component or an env/agent sub-component
            // (a sequencer testbench field is rejected via
            // `component_type_names`); inert at this gate.
            Item::Sequencer(_) => {}
            // `tseq` declarations are validated by `collect_tseq_records`
            // (element type must be a declared record) and lowered to
            // `FunctionKind::Tseq` functions below — inert at this gate.
            Item::Tseq(_) => {}
            Item::Env(c) | Item::Agent(c) => {
                if used_tbs.contains(&c.name.name) {
                    validate_testbench_component(
                        c,
                        &components,
                        &covgroup_ids,
                        &record_ids,
                        &transactor_ids,
                        &scoreboard_ids,
                        &component_type_names,
                        &event_driven_transactor_names,
                    )?;
                } else if matches!(
                    c.kind,
                    crate::ast::ComponentKind::Env | crate::ast::ComponentKind::Agent
                ) {
                    // A composite `env`/`agent` used as a test-scope
                    // component (`let env : AnalysisEnv` / `let tagger :
                    // Tagger`) is lowered through the component-cluster
                    // path below — inert at this gate (its items are
                    // validated by the component schema builder).
                } else {
                    return Err(unsupported(
                        &format!("env/component `{}`", c.name.name),
                        "only testbench components bound via `impl ... for` are lowered",
                    ));
                }
            }
            other => {
                return Err(unsupported(
                    &format!("the `{}` construct", item_label(other)),
                    "",
                ));
            }
        }
    }

    let mut prog = TbProgram {
        covgroups,
        records: record_schemas,
        scoreboards: scoreboard_schemas,
        regblocks: regblock_schemas,
        ..TbProgram::default()
    };

    // Program-wide constraint-site accumulator. Shared by reference
    // across every function lowered below so a `Terminator::Randomize`'s
    // `ConstraintRef` is a globally-unique index into one table. Drained
    // into `prog.constraint_sites` once all functions are lowered.
    let constraint_sites: RefCell<Vec<ConstraintSite>> = RefCell::new(Vec::new());
    // Typed solver problem table (constraint-IR layer) — the source of
    // the per-site `problem_id` handle. Built from the SAME desugared
    // `file` v1 uses (`cpp_tb` desugars, then builds the table), so the
    // randomize-target spans this table is keyed by match the spans the
    // lowering sees. Drives `ConstraintSite::problem_id`.
    let solver_table = crate::solver::problem_table::build_typed_solver_problem_table(&file);

    // Randomize-target span → problem-id, keyed exactly like v1's
    // `runtime_randomize_problem_ids` (only Z3-ready sites populate).
    let mut randomize_problem_ids: HashMap<(usize, usize), u32> = HashMap::new();
    for entry in &solver_table.entries {
        let crate::solver::problem_table::TypedSolverProblemSource::RandomizeSite { span, .. } =
            entry.source
        else {
            continue;
        };
        if let crate::solver::problem_table::TypedSolverProblemBuild::Z3 { typed, .. } =
            &entry.build
        {
            randomize_problem_ids.insert((span.start, span.end), typed.problem_id.0);
        }
    }

    // Transaction-level `keep` clauses as AST exprs, by transaction
    // name. Spec §4: these are part of every `randomize(t)` of that
    // type, merged ahead of any call-site `with {...}` body (v1's
    // `txn_keeps` merge in `StmtKind::Randomize`).
    let mut txn_keeps: HashMap<String, Vec<crate::ast::Expr>> = HashMap::new();
    for it in &file.items {
        if let Item::Transaction(t) = it {
            let keeps: Vec<crate::ast::Expr> = t
                .body
                .iter()
                .filter_map(|item| match item {
                    crate::ast::TxnBodyItem::Keep(k) => Some(k.expr.clone()),
                    _ => None,
                })
                .collect();
            if !keeps.is_empty() {
                txn_keeps.insert(t.name.name.clone(), keeps);
            }
        }
    }

    // Eagerly lower pure helpers (declaration order) so call sites can
    // stay `ir::Expr::Call` and backends emit them as plain C++ functions.
    // Records are visible (for precise rejection messages), but pure
    // helpers cannot hold record locals — see `lower_let`.
    let helper_ctx = LowerCtx {
        dut_field: "dut".to_string(),
        tb_field: None,
        cov_fields: HashMap::new(),
        covgroups: Vec::new(),
        clock_names: Vec::new(),
        record_ids: record_ids.clone(),
        records: prog.records.clone(),
        // Deliberately empty: bus bindings and transactor fields are
        // test-scope, so a pure helper body can never resolve one —
        // which structurally enforces the design seam rule that
        // `TransactorMethod` call edges never appear in pure-helper
        // bodies.
        bus_bindings: HashMap::new(),
        transactor_fields: HashMap::new(),
        transactors: Vec::new(),
        scoreboard_fields: HashMap::new(),
        scoreboards: Vec::new(),
        consts: consts.clone(),
        tb_scalar_fields: HashSet::new(),
        tb_methods: HashMap::new(),
        test_scope_lets: HashSet::new(),
        regblock_bindings: HashMap::new(),
        regblock_init_order: Vec::new(),
        bare_transactor_fields: HashSet::new(),
        target_state: HashMap::new(),
        components: Vec::new(),
        component_fields: HashMap::new(),
        // Pure helpers cannot hold record locals, so `randomize` can
        // never fire in one — these maps stay inert here.
        txn_keeps: HashMap::new(),
        randomize_problem_ids: HashMap::new(),
        tseqs: HashMap::new(),
    };
    for it in &file.items {
        let Item::Function(fd) = it else { continue };
        let Some(entry) = helper_registry.get(&fd.name.name) else {
            continue;
        };
        // On duplicate names the registry keeps the last declaration;
        // only lower that one (single definition per name downstream).
        if !entry.pure || !std::ptr::eq(entry.decl, fd) {
            continue;
        }
        let id = FunctionId(prog.functions.len() as u32);
        let f = helpers::lower_pure_helper(id, fd, &helper_registry, &helper_ctx, &constraint_sites)?;
        prog.functions.push(f);
    }

    // `tseq` bodies, in file order: one `FunctionKind::Tseq` function
    // each, recorded as `name → FunctionId` so the test body can resolve
    // a `let txns = Name(...)` call edge. A tseq body holds record locals
    // and `randomize`, so its ctx carries the records / keep / problem-id
    // / tseq tables — but no test-scope bindings (a tseq cannot poke the
    // DUT, a bus, or a transactor field; those are test-scope only).
    let tseq_ctx = LowerCtx {
        dut_field: "dut".to_string(),
        tb_field: None,
        cov_fields: HashMap::new(),
        covgroups: Vec::new(),
        clock_names: Vec::new(),
        record_ids: record_ids.clone(),
        records: prog.records.clone(),
        bus_bindings: HashMap::new(),
        transactor_fields: HashMap::new(),
        transactors: Vec::new(),
        scoreboard_fields: HashMap::new(),
        scoreboards: Vec::new(),
        consts: consts.clone(),
        tb_scalar_fields: HashSet::new(),
        tb_methods: HashMap::new(),
        test_scope_lets: HashSet::new(),
        regblock_bindings: HashMap::new(),
        regblock_init_order: Vec::new(),
        bare_transactor_fields: HashSet::new(),
        target_state: HashMap::new(),
        components: Vec::new(),
        component_fields: HashMap::new(),
        txn_keeps: txn_keeps.clone(),
        randomize_problem_ids: randomize_problem_ids.clone(),
        tseqs: tseq_records.clone(),
    };
    for it in &file.items {
        let Item::Tseq(decl) = it else { continue };
        let record = tseq_records[&decl.name.name];
        let id = FunctionId(prog.functions.len() as u32);
        let f = tseqs::lower_tseq(id, decl, record, &tseq_ctx, &helper_registry, &constraint_sites)?;
        prog.functions.push(f);
    }

    // Transactor declarations, in file order: one schema each plus one
    // `TbFunction` (kind `TransactorBody`) per method. All declarations
    // lower (even unreferenced ones), so unsupported transactor shapes
    // are rejected here rather than dropped.
    for it in &file.items {
        let Item::Transactor(t) = it else { continue };
        if components::transactor_is_component(t) {
            continue;
        }
        let id = TransactorId(prog.transactors.len() as u32);
        debug_assert_eq!(Some(&id), transactor_ids.get(&t.name.name));
        let (schema, funcs) = transactors::lower_transactor(
            id,
            t,
            FunctionId(prog.functions.len() as u32),
            &helper_registry,
            &helper_ctx,
            &buses,
            &constraint_sites,
        )?;
        prog.transactors.push(schema);
        prog.functions.extend(funcs);
    }

    // Composite-component declarations (env/agent cluster, flat-struct
    // subset): method-bearing scoreboards, analysis-source transactors,
    // and the `env`s that compose them. Classified in file order; an
    // `env`'s sub-component fields and `connect` edges resolve against
    // the component ids assigned here.
    //
    // Two passes, like transactors: (1) build schemas (fields + method
    // signatures, reserving FunctionIds), (2) lower method bodies with a
    // ctx that knows the component table. `agent`/`sequencer` decls are
    // rejected precisely (they need the event-handler / sequencer slices,
    // out of this subset).
    let mut comp_sources: Vec<components::CompSource<'_>> = Vec::new();
    let mut component_ids: HashMap<String, ir::ComponentId> = HashMap::new();
    for it in &file.items {
        let (name, src) = match it {
            Item::Scoreboard(c) if components::scoreboard_is_component(c) => {
                (&c.name.name, components::CompSource::Scoreboard(c))
            }
            Item::Transactor(t) if components::transactor_is_component(t) => {
                (&t.name.name, components::CompSource::Transactor(t))
            }
            Item::Env(c) if matches!(c.kind, crate::ast::ComponentKind::Env) => {
                (&c.name.name, components::CompSource::Env(c))
            }
            Item::Agent(c) if matches!(c.kind, crate::ast::ComponentKind::Agent) => {
                (&c.name.name, components::CompSource::Agent(c))
            }
            Item::Sequencer(c) => (&c.name.name, components::CompSource::Sequencer(c)),
            _ => continue,
        };
        let cid = ir::ComponentId(comp_sources.len() as u32);
        if component_ids.insert(name.clone(), cid).is_some() {
            return Err(LowerError::Invalid(format!(
                "duplicate component declaration `{name}`"
            )));
        }
        comp_sources.push(src);
    }
    // Pass 1: schemas. FunctionIds count up from the current function
    // count (after pure helpers + transactor methods).
    let mut next_fn = prog.functions.len() as u32;
    for src in &comp_sources {
        let schema =
            components::lower_component_schema(
                src,
                &component_ids,
                &scoreboard_ids,
                &record_ids,
                &mut next_fn,
            )?;
        prog.components.push(schema);
    }
    // Pass 1b: resolve `connect` edges (env components only), now that
    // every component schema (fields + methods) exists.
    let comp_snapshot = prog.components.clone();
    for (i, src) in comp_sources.iter().enumerate() {
        if let components::CompSource::Env(env) = src {
            let connects = components::resolve_connects(env, &comp_snapshot[i], &comp_snapshot)?;
            prog.components[i].connects = connects;
        }
    }
    // Pass 2: method bodies. A method ctx knows the component table (for
    // self-relative field access + sub-component method resolution) but
    // no test-scope fields. Bodies are lowered into placeholder
    // FunctionIds reserved in pass 1, then sorted into the functions
    // table (their ids are already contiguous from `start_fn`).
    let start_fn = prog.functions.len();
    let method_ctx = LowerCtx {
        dut_field: "dut".to_string(),
        tb_field: None,
        cov_fields: HashMap::new(),
        covgroups: Vec::new(),
        clock_names: Vec::new(),
        record_ids: record_ids.clone(),
        records: prog.records.clone(),
        bus_bindings: HashMap::new(),
        transactor_fields: HashMap::new(),
        transactors: Vec::new(),
        scoreboard_fields: HashMap::new(),
        scoreboards: Vec::new(),
        consts: consts.clone(),
        tb_scalar_fields: HashSet::new(),
        tb_methods: HashMap::new(),
        test_scope_lets: HashSet::new(),
        regblock_bindings: HashMap::new(),
        regblock_init_order: Vec::new(),
        bare_transactor_fields: HashSet::new(),
        target_state: HashMap::new(),
        components: prog.components.clone(),
        component_fields: HashMap::new(),
        // Component method bodies are not cataloged in the constraint-IR
        // problem table; a `randomize` inside one lowers with no
        // problem-id (v1's nullptr-descriptor fallback).
        txn_keeps: HashMap::new(),
        randomize_problem_ids: HashMap::new(),
        // Component methods cannot call a tseq generator (test-scope only).
        tseqs: HashMap::new(),
    };
    let mut method_funcs: Vec<TbFunction> = Vec::new();
    for (i, src) in comp_sources.iter().enumerate() {
        let cid = ir::ComponentId(i as u32);
        let schema = prog.components[i].clone();
        let bodies = components::lower_component_bodies(
            src,
            cid,
            &schema,
            &method_ctx,
            &helper_registry,
            &constraint_sites,
        )?;
        // Patch the schema's pass-1 clause placeholders with the
        // resolved period/max_idle expressions (they could only lower
        // once a body context existed).
        debug_assert_eq!(
            bodies.periodic_periods.len(),
            prog.components[i].periodic_handlers.len()
        );
        for (ph, period) in prog.components[i]
            .periodic_handlers
            .iter_mut()
            .zip(bodies.periodic_periods)
        {
            ph.period = period;
        }
        if let (Some(ws), Some((period, max_idle))) =
            (prog.components[i].watchdog.as_mut(), bodies.watchdog_clauses)
        {
            ws.period = period;
            ws.max_idle = max_idle;
        }
        method_funcs.extend(bodies.funcs);
    }
    // The reserved FunctionIds must be contiguous from `start_fn`.
    debug_assert_eq!(
        method_funcs.first().map(|f| f.id.0),
        if method_funcs.is_empty() {
            None
        } else {
            Some(start_fn as u32)
        }
    );
    let _ = start_fn;
    prog.functions.extend(method_funcs);

    for it in &file.items {
        let Item::Test(t) = it else { continue };
        lower_test(
            t,
            &tb_of_test,
            &components,
            &component_ids,
            &domains,
            &covgroup_ids,
            &record_ids,
            &regblock_ids,
            &buses,
            &consts,
            &helper_registry,
            &txn_keeps,
            &randomize_problem_ids,
            &tseq_records,
            &constraint_sites,
            &mut prog,
        )?;
    }

    if prog.tests.is_empty() {
        return Err(LowerError::Invalid(
            "no `test` declaration found".to_string(),
        ));
    }
    prog.constraint_sites = constraint_sites.into_inner();
    Ok(prog)
}

/// MVP testbench-component validation: every field must be the DUT
/// field (a non-HARC type, i.e. a Verilator module type), a covergroup
/// instance, an `active` transactor instance, or a scalar
/// (uint/sint/bits/bool) member — run/check-shared host state on the
/// `_tb` struct. Scoreboard / agent / env fields are post-MVP.
fn validate_testbench_component(
    c: &ComponentDecl,
    components: &HashMap<String, &ComponentDecl>,
    covgroup_ids: &HashMap<String, CovgroupId>,
    record_ids: &HashMap<String, RecordId>,
    transactor_ids: &HashMap<String, TransactorId>,
    scoreboard_ids: &HashMap<String, ScoreboardId>,
    component_type_names: &HashSet<String>,
    event_driven_transactor_names: &HashSet<String>,
) -> Result<(), LowerError> {
    for ci in &c.items {
        match ci {
            ComponentItem::Field(f) => {
                if let TypeExpr::Named { name, mode, .. } = &f.ty {
                    let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                    if covgroup_ids.contains_key(simple) {
                        continue;
                    }
                    // An event-driven transactor field (`drv : SeqXactor
                    // active`) routes to a `ComponentSchema` but is still a
                    // transactor: it requires an explicit `active` mode (a
                    // `passive` instance has no `when active` body — its
                    // `on` handler never registers, so it can't consume).
                    if event_driven_transactor_names.contains(simple) {
                        match mode {
                            Some(TransactorMode::Active) => continue,
                            Some(TransactorMode::Passive) => {
                                return Err(unsupported(
                                    &format!(
                                        "a passive event-driven transactor field `{}.{} : \
                                         {simple} passive`",
                                        c.name.name, f.name.name
                                    ),
                                    "the consumer's `on` handler only registers on an \
                                     `active` instance",
                                ));
                            }
                            None => {
                                return Err(unsupported(
                                    &format!(
                                        "an event-driven transactor field `{}.{} : {simple}` \
                                         without an `active`/`passive` mode",
                                        c.name.name, f.name.name
                                    ),
                                    "annotate the instance `active`",
                                ));
                            }
                        }
                    }
                    // A composite-component type (method-bearing
                    // scoreboard, analysis-source transactor, env, or
                    // agent) bound as a testbench field. Accepted by the
                    // testbench-field-binding slice: the field routes to a
                    // `ComponentSchema` instance just like a test-scope
                    // `let env : <Env>` does. A `mode` (active/passive) is
                    // meaningless on a composite component (that keyword is
                    // a transactor concept), so reject it rather than
                    // silently drop it.
                    if component_type_names.contains(simple) {
                        if mode.is_some() {
                            return Err(unsupported(
                                &format!(
                                    "an `active`/`passive` mode on composite-component \
                                     testbench field `{}.{} : {simple}`",
                                    c.name.name, f.name.name
                                ),
                                "the mode keyword applies to transactor instances, not \
                                 envs/agents/scoreboards",
                            ));
                        }
                        continue;
                    }
                    if scoreboard_ids.contains_key(simple) {
                        // A scoreboard testbench field — data-only host
                        // state. Always accepted (no mode); the schema's
                        // own lowering already rejected unsupported field
                        // shapes.
                        continue;
                    }
                    if transactor_ids.contains_key(simple) {
                        // Mode rules mirror v1: a mode-less transactor
                        // field has nothing to inherit from at testbench
                        // scope, and a passive instance structurally
                        // lacks its `when active` methods — every method
                        // in this subset lives there.
                        match mode {
                            Some(TransactorMode::Active) => continue,
                            Some(TransactorMode::Passive) => {
                                return Err(unsupported(
                                    &format!(
                                        "passive transactor instance `{}.{} : {simple} passive`",
                                        c.name.name, f.name.name
                                    ),
                                    "methods inside `when active` do not exist on a passive \
                                     instance",
                                ));
                            }
                            None => {
                                return Err(unsupported(
                                    &format!(
                                        "transactor field `{}.{} : {simple}` without an \
                                         `active`/`passive` mode",
                                        c.name.name, f.name.name
                                    ),
                                    "",
                                ));
                            }
                        }
                    }
                    // Without this gate a transaction-typed field would
                    // fall through to the "assume DUT module type" arm
                    // and mis-lower.
                    if record_ids.contains_key(simple) {
                        return Err(unsupported(
                            &format!(
                                "testbench field `{}` of transaction type `{}`",
                                f.name.name, simple
                            ),
                            "",
                        ));
                    }
                    // env/agent component types are accepted by the
                    // `component_type_names` gate above; the only remaining
                    // entry in `components` here is a `sequencer`, which is
                    // out of the lowered subset entirely.
                    if components.contains_key(simple) {
                        return Err(unsupported(
                            &format!(
                                "testbench field `{}` of component type `{}`",
                                f.name.name, simple
                            ),
                            "the `sequencer` construct is not in this subset",
                        ));
                    }
                } else if tb_scalar_field_ir_type(&f.ty).is_none() {
                    return Err(unsupported(
                        &format!(
                            "testbench field `{}` with a non-scalar, non-named type",
                            f.name.name
                        ),
                        "only uint/sint/bits/bool fields up to 64 bits are lowered",
                    ));
                }
            }
            // Lifecycle blocks were folded into the test's scope by the
            // impl-for desugaring; the declaration itself is inert here.
            ComponentItem::Lifecycle(..) => {}
            // Helper methods are inert unless called; calls surface as
            // `Unsupported` at the call site during body lowering.
            ComponentItem::Hookable(_) => {}
            _ => {
                return Err(unsupported(
                    &format!("testbench item in `{}`", c.name.name),
                    "only fields and lifecycle phases are lowered",
                ));
            }
        }
    }
    Ok(())
}

fn item_label(it: &Item) -> &'static str {
    match it {
        Item::Use(_) => "use",
        Item::Package(_) => "package",
        Item::Const(_) => "const",
        Item::Domain(_) => "domain",
        Item::Struct(_) => "struct",
        Item::Enum(_) => "enum",
        Item::Transaction(_) => "transaction",
        Item::Relation(_) => "relation",
        Item::Tseq(_) => "tseq",
        Item::Agent(_) => "agent",
        Item::Env(_) => "env",
        Item::Scoreboard(_) => "scoreboard",
        Item::Sequencer(_) => "sequencer",
        Item::Test(_) => "test",
        Item::Extend(_) => "extend",
        Item::Covergroup(_) => "covergroup",
        Item::Property(_) => "property",
        Item::Pseq(_) => "pseq",
        Item::CoverSequence(_) => "cover sequence",
        Item::ExternalModule(_) => "external module",
        Item::Function(_) => "function",
        Item::ExternFn(_) => "extern fn",
        Item::Apply(_) => "apply",
        Item::Bus(_) => "bus",
        Item::Transactor(_) => "transactor",
        Item::Regblock(_) => "regblock",
        Item::Addrmap(_) => "addrmap",
    }
}

#[allow(clippy::too_many_arguments)]
fn lower_test(
    t: &TestDecl,
    tb_of_test: &HashMap<String, String>,
    components: &HashMap<String, &ComponentDecl>,
    component_ids: &HashMap<String, ir::ComponentId>,
    domains: &HashMap<String, i64>,
    covgroup_ids: &HashMap<String, CovgroupId>,
    record_ids: &HashMap<String, RecordId>,
    regblock_ids: &HashMap<String, RegblockId>,
    buses: &HashMap<String, &BusDecl>,
    consts: &HashMap<String, u64>,
    helpers: &helpers::HelperRegistry<'_>,
    txn_keeps: &HashMap<String, Vec<crate::ast::Expr>>,
    randomize_problem_ids: &HashMap<(usize, usize), u32>,
    tseq_records: &HashMap<String, RecordId>,
    constraint_sites: &RefCell<Vec<ConstraintSite>>,
    prog: &mut TbProgram,
) -> Result<(), LowerError> {
    if !t.params.is_empty() {
        return Err(unsupported("test parameters", ""));
    }

    let mut dut_type: Option<String> = None;
    let mut clocks: Vec<&ClockDecl> = Vec::new();
    let mut scope: Option<&ScopeDecl> = None;
    let mut bare_stmts: Vec<&AstStmt> = Vec::new();
    // Named `phase <name> ... end phase <name>` blocks (spec §7.2). Each
    // is callable by `<name>()` from the run/check body; the call site is
    // INLINED with the phase block's statements (v1 emits a `[&]() ->
    // void` lambda + a plain call — identical observable behavior, since
    // the body runs at the call site inside the run coroutine). Collected
    // in declaration order so a redeclaration is rejected.
    let mut phases: HashMap<String, &Block> = HashMap::new();
    let mut bus_bindings: Vec<ir::BusBindingSchema> = Vec::new();
    let mut bus_binding_decls: HashMap<String, BusDecl> = HashMap::new();
    // Regblock bindings (`let regs : R = bind <helper>`), collected here
    // and validated after the testbench's transactor fields are known
    // (the `<helper>` must be an active transactor field). Each tuple is
    // (binding name, regblock id, helper field name).
    let mut regblock_binds: Vec<(String, RegblockId, String)> = Vec::new();
    // Test-scope unbound-transactor instances (`let h : Xactor active`),
    // accessed by bare name. Merged into `transactor_fields` after the
    // testbench-field walk; collected here as (name, transactor id).
    let mut test_scope_xactors: Vec<(String, TransactorId)> = Vec::new();
    // Test-scope composite-component instances (`let env : AnalysisEnv`),
    // collected as (name, component id). Emitted as plain run-scope
    // locals + their `connect` push_backs.
    let mut test_scope_components: Vec<(String, ir::ComponentId)> = Vec::new();
    // Bound-to target-side TLM responder instances (`let target : X
    // passive = bind <busbinding>`), collected as (instance, transactor
    // id, bus-binding field). Validated after the bus bindings are known.
    let mut target_tlm_binds: Vec<(String, TransactorId, String)> = Vec::new();
    // Bound-to initiator-side BFM instances (`let helper : H active =
    // bind <busbinding>`), collected as (instance, transactor id, bus-
    // binding field). The helper's `hookable` methods drive the bound
    // bus's channels; it is registered as a transactor field so the
    // regblock `via` frontdoor and bare `helper.method(...)` calls
    // resolve. Validated after the bus bindings are known.
    let mut initiator_bfm_binds: Vec<(String, TransactorId, String)> = Vec::new();
    // Test-scope `let`s (beyond `dut`/`_tb`/bus binds), hoisted to the
    // head of the run body in item order — v1 hoists them to `main`
    // scope before the coroutine, initialized once, and the coroutine
    // captures by reference; the IR lowers them as run-function locals
    // initialized at entry. Names are recorded so a check-phase
    // reference gets a precise rejection (run and check are separate
    // IR functions, so the shared-state form is not representable).
    let mut test_let_stmts: Vec<AstStmt> = Vec::new();
    let mut test_let_names: HashSet<String> = HashSet::new();
    let tb_name = tb_of_test.get(&t.name.name).cloned();

    for it in &t.items {
        match it {
            TestItem::Let(l) if l.name.name == "dut" => {
                if !l.probes.is_empty() {
                    return Err(unsupported("probe declarations on `let dut`", ""));
                }
                if !l.bind_remap.is_empty() {
                    return Err(unsupported("bind remaps on `let dut`", ""));
                }
                let simple = match l.ty.as_ref() {
                    Some(TypeExpr::Named { name, .. }) => {
                        name.segments.last().map(|s| s.name.clone())
                    }
                    _ => None,
                };
                dut_type = Some(simple.ok_or_else(|| {
                    LowerError::Invalid(
                        "`let dut : <Type>` must use a simple named type".to_string(),
                    )
                })?);
            }
            // The desugared impl-form synthesizes `let _tb : <TbType>`;
            // the TB instance is scaffolding-owned in the IR backend.
            TestItem::Let(l) if l.name.name == "_tb" => {}
            // Bus binding: `let axil : BusAxiLite = bind dut`.
            TestItem::Let(l)
                if l.bind
                    && type_simple_name(l.ty.as_ref())
                        .is_some_and(|n| buses.contains_key(n)) =>
            {
                let bus_name = type_simple_name(l.ty.as_ref()).unwrap();
                let decl = buses[bus_name];
                let (schema, owned) = bus::lower_bus_binding(l, decl)?;
                if bus_binding_decls.contains_key(&l.name.name) {
                    // Two bindings with one name would resolve
                    // ambiguously (v1's map silently keeps the last;
                    // the IR schema would keep both) — reject.
                    return Err(LowerError::Invalid(format!(
                        "duplicate bus binding `{}` in test `{}`",
                        l.name.name, t.name.name
                    )));
                }
                bus_bindings.push(schema);
                bus_binding_decls.insert(l.name.name.clone(), owned);
            }
            // Regblock binding: `let regs : DmaRegs = bind <helper>`.
            TestItem::Let(l)
                if l.bind
                    && type_simple_name(l.ty.as_ref())
                        .is_some_and(|n| regblock_ids.contains_key(n)) =>
            {
                if !l.probes.is_empty() {
                    return Err(unsupported("probe declarations on a regblock binding", ""));
                }
                if !l.bind_remap.is_empty() {
                    return Err(unsupported("bind remaps on a regblock binding", ""));
                }
                let rb_name = type_simple_name(l.ty.as_ref()).unwrap();
                let rbid = regblock_ids[rb_name];
                // RHS must be a bare helper-instance identifier (the
                // transactor field the frontdoor routes through).
                let helper_field = match l.value.as_ref().map(|v| &*v.kind) {
                    Some(ExprKind::Ident(id)) => id.name.clone(),
                    _ => {
                        return Err(unsupported(
                            &format!(
                                "regblock binding `{}` to a non-identifier helper",
                                l.name.name
                            ),
                            "only `= bind <helper>` (a transactor instance) is lowered",
                        ));
                    }
                };
                regblock_binds.push((l.name.name.clone(), rbid, helper_field));
            }
            // Bound-to initiator-side BFM: `let helper : AxilHelper
            // active = bind <busbinding>`. The helper's `hookable`
            // methods drive the bound bus's handshake channels; it is
            // registered as an active transactor field so the regblock
            // `via <helper>` frontdoor (#369) and bare
            // `helper.method(...)` calls resolve through the same
            // `CallTarget::TransactorMethod` dispatch. The bind RHS must
            // be a bus binding declared earlier in the test, of the bus
            // the transactor is `bound to` (validated after the binding
            // walk). Distinguished from the target-responder form below
            // by carrying `methods` (initiator) rather than
            // `target_methods` (responder).
            TestItem::Let(l)
                if l.bind
                    && type_simple_name(l.ty.as_ref()).is_some_and(|n| {
                        prog.transactors.iter().any(|x| {
                            x.name == n && x.bound_bus.is_some() && !x.methods.is_empty()
                        })
                    }) =>
            {
                if !l.probes.is_empty() {
                    return Err(unsupported(
                        "probe declarations on an initiator-BFM instance",
                        "",
                    ));
                }
                if !l.bind_remap.is_empty() {
                    return Err(unsupported(
                        "bind remaps on an initiator-BFM instance",
                        "the default `<binding>_<ch>_<sig>` wire convention is lowered; \
                         custom signal remaps are a follow-up slice",
                    ));
                }
                let simple = type_simple_name(l.ty.as_ref()).unwrap();
                // The BFM host must be `active` — its methods are
                // test-called (via the regblock frontdoor or directly).
                match l.ty.as_ref() {
                    Some(TypeExpr::Named { mode: Some(TransactorMode::Active), .. }) => {}
                    _ => {
                        return Err(unsupported(
                            &format!(
                                "initiator-BFM instance `let {} : {simple}` must be declared \
                                 `active`",
                                l.name.name
                            ),
                            "its hookable methods are test-called, not request-served",
                        ));
                    }
                }
                // RHS must be a bare bus-binding identifier.
                let bus_field = match l.value.as_ref().map(|v| &*v.kind) {
                    Some(ExprKind::Ident(id)) => id.name.clone(),
                    _ => {
                        return Err(unsupported(
                            &format!(
                                "initiator-BFM instance `{}` bound to a non-identifier",
                                l.name.name
                            ),
                            "only `= bind <bus-binding>` is lowered",
                        ));
                    }
                };
                let xid = ir::TransactorId(
                    prog.transactors.iter().position(|x| x.name == simple).unwrap() as u32,
                );
                initiator_bfm_binds.push((l.name.name.clone(), xid, bus_field));
            }
            // Bound-to target-side TLM responder: `let target : MemTarget
            // passive = bind <busbinding>`. The instance is a passive
            // responder host for a `transactor X bound to <Bus>`; its
            // `thread bus.<m>(...)` bodies serve the bound bus binding's
            // req/rsp wires. The bind RHS must be a bus binding declared
            // earlier in the test (validated after the binding walk).
            TestItem::Let(l)
                if l.bind
                    && type_simple_name(l.ty.as_ref()).is_some_and(|n| {
                        prog.transactors
                            .iter()
                            .any(|x| x.name == n && x.bound_bus.is_some())
                    }) =>
            {
                if !l.probes.is_empty() {
                    return Err(unsupported(
                        "probe declarations on a target-TLM responder instance",
                        "",
                    ));
                }
                if !l.bind_remap.is_empty() {
                    return Err(unsupported(
                        "bind remaps on a target-TLM responder instance",
                        "the default `<binding>_<method>_<sig>` wire convention is lowered; \
                         custom signal remaps are a follow-up slice",
                    ));
                }
                let simple = type_simple_name(l.ty.as_ref()).unwrap();
                // The responder host must be `passive` — its methods are
                // request-served, never test-called.
                match l.ty.as_ref() {
                    Some(TypeExpr::Named { mode: Some(TransactorMode::Passive), .. }) => {}
                    _ => {
                        return Err(unsupported(
                            &format!(
                                "target-TLM responder instance `let {} : {simple}` must be \
                                 declared `passive`",
                                l.name.name
                            ),
                            "the responder serves bus requests; its methods are not test-called",
                        ));
                    }
                }
                // RHS must be a bare bus-binding identifier.
                let bus_field = match l.value.as_ref().map(|v| &*v.kind) {
                    Some(ExprKind::Ident(id)) => id.name.clone(),
                    _ => {
                        return Err(unsupported(
                            &format!(
                                "target-TLM responder `{}` bound to a non-identifier",
                                l.name.name
                            ),
                            "only `= bind <bus-binding>` is lowered",
                        ));
                    }
                };
                let xid = ir::TransactorId(
                    prog.transactors.iter().position(|x| x.name == simple).unwrap() as u32,
                );
                target_tlm_binds.push((l.name.name.clone(), xid, bus_field));
            }
            // Test-scope composite-component instance: `let env :
            // AnalysisEnv`. Emitted as a plain run-scope local (v1's
            // `AnalysisEnv env;`), default-constructed, with its env
            // `connect` push_backs wired right after. Accessed by bare
            // dotted path (`env.source.publish(..)`, `env.sb.count`).
            TestItem::Let(l)
                if !l.bind
                    && type_simple_name(l.ty.as_ref())
                        .is_some_and(|n| component_ids.contains_key(n)) =>
            {
                if !l.probes.is_empty() {
                    return Err(unsupported(
                        "probe declarations on a component instance",
                        "",
                    ));
                }
                if l.value.is_some() {
                    return Err(unsupported(
                        &format!("component instance `let {}` with an initializer", l.name.name),
                        "components default-construct",
                    ));
                }
                let simple = type_simple_name(l.ty.as_ref()).unwrap();
                let cid = component_ids[simple];
                test_scope_components.push((l.name.name.clone(), cid));
            }
            // Test-scope unbound-transactor instance: `let h : MemHelper
            // active` (no `= bind`). v1 routes regblock frontdoor calls
            // and bare `h.method(...)` through a test-scope-let helper
            // (it lives in v1's `let_types`), so the IR registers it as a
            // transactor instance accessed by its BARE name (not
            // `_tb.h` — test-scope lets aren't rewritten by the impl-for
            // desugaring). The DUT bind `h.dut = dut` and method calls
            // resolve through the same `transactor_fields` machinery as a
            // testbench-field instance.
            TestItem::Let(l)
                if !l.bind
                    && type_simple_name(l.ty.as_ref())
                        .is_some_and(|n| prog.transactors.iter().any(|x| x.name == n)) =>
            {
                if !l.probes.is_empty() {
                    return Err(unsupported(
                        "probe declarations on a transactor instance",
                        "",
                    ));
                }
                if l.value.is_some() {
                    return Err(unsupported(
                        &format!("transactor instance `let {}` with an initializer", l.name.name),
                        "transactor instances default-construct; bind the DUT with \
                         `{}.dut = dut` in the body",
                    ));
                }
                let simple = type_simple_name(l.ty.as_ref()).unwrap();
                // Require an explicit `active` mode (matching the
                // testbench-field rule: every method lives in `when
                // active`, so a passive instance has none).
                match l.ty.as_ref() {
                    Some(TypeExpr::Named { mode: Some(TransactorMode::Active), .. }) => {}
                    Some(TypeExpr::Named { mode: Some(TransactorMode::Passive), .. }) => {
                        return Err(unsupported(
                            &format!(
                                "passive transactor instance `let {} : {simple} passive`",
                                l.name.name
                            ),
                            "methods inside `when active` do not exist on a passive instance",
                        ));
                    }
                    _ => {
                        return Err(unsupported(
                            &format!(
                                "transactor instance `let {} : {simple}` without an \
                                 `active`/`passive` mode",
                                l.name.name
                            ),
                            "",
                        ));
                    }
                }
                let xid = ir::TransactorId(
                    prog.transactors.iter().position(|x| x.name == simple).unwrap() as u32,
                );
                let xdut = &prog.transactors[xid.index()].dut_type;
                if let Some(dt) = &dut_type {
                    if xdut != dt {
                        return Err(unsupported(
                            &format!(
                                "transactor instance `{}` whose DUT field type `{xdut}` \
                                 differs from the test DUT type `{dt}`",
                                l.name.name
                            ),
                            "",
                        ));
                    }
                }
                test_scope_xactors.push((l.name.name.clone(), xid));
            }
            TestItem::Let(l) => {
                if !l.probes.is_empty() || l.bind {
                    return Err(unsupported(
                        &format!("test-scope `let {}` with probes or a bind", l.name.name),
                        "only plain `let <name> [: <Ty>] = <expr>` test-scope lets are lowered",
                    ));
                }
                test_let_names.insert(l.name.name.clone());
                test_let_stmts.push(AstStmt {
                    kind: StmtKind::Let(l.clone()),
                    span: l.span,
                });
            }
            TestItem::Clock(c) => clocks.push(c),
            TestItem::Scope(s) => {
                if scope.is_some() {
                    return Err(unsupported("multiple `scope` blocks in one test", ""));
                }
                scope = Some(s);
            }
            TestItem::Stmt(s) => bare_stmts.push(s),
            TestItem::Phase(name, body) => {
                if phases.insert(name.name.clone(), body).is_some() {
                    return Err(LowerError::Invalid(format!(
                        "test `{}` declares phase `{}` more than once",
                        t.name.name, name.name
                    )));
                }
            }
            TestItem::Apply(_) => return Err(unsupported("apply", "")),
            TestItem::Use(_) => {}
        }
    }

    let dut_type = dut_type.ok_or_else(|| {
        LowerError::Invalid(format!(
            "test `{}` has no `let dut : <Type>` declaration",
            t.name.name
        ))
    })?;

    // Resolve clocks.
    let mut clock_specs = Vec::new();
    for c in &clocks {
        let (period_ps, domain) = match &*c.period.kind {
            ExprKind::Time(s) => (
                time_literal_to_ps(s).map_err(LowerError::Invalid)?,
                None,
            ),
            ExprKind::Ident(id) => {
                let p = domains.get(&id.name).ok_or_else(|| {
                    LowerError::Invalid(format!(
                        "clock {} references domain `{}` but no `domain {}` declaration was found",
                        c.name.name, id.name, id.name
                    ))
                })?;
                (*p, Some(id.name.clone()))
            }
            _ => {
                return Err(LowerError::Invalid(format!(
                    "clock {} period must be a time literal or a domain name",
                    c.name.name
                )));
            }
        };
        clock_specs.push(ClockSpec {
            name: c.name.name.clone(),
            period_ps,
            domain,
        });
    }

    // Testbench schema.
    let synthetic = tb_name.is_none();
    let tb_schema_name = tb_name.clone().unwrap_or_else(|| format!("{}_tb", t.name.name));
    if let Some(tbn) = &tb_name {
        // Validated at the file gate; double-check it resolved.
        if !components.contains_key(tbn) {
            return Err(LowerError::Invalid(format!(
                "test `{}` is bound to unknown testbench `{tbn}`",
                t.name.name
            )));
        }
    }
    // Covergroup- and transactor-typed testbench fields, in declaration
    // order — cov order is the sampler registration order (must match
    // v1); transactor order is the method-lambda emission order. Scalar
    // member fields and helper methods (`function`/`hookable`) are
    // collected in the same walk.
    let mut cov_fields: Vec<(String, CovgroupId)> = Vec::new();
    let mut transactor_fields: Vec<(String, TransactorId)> = Vec::new();
    let mut scoreboard_fields: Vec<(String, ScoreboardId)> = Vec::new();
    let mut scalar_fields: Vec<ir::TbScalarFieldSchema> = Vec::new();
    let mut tb_methods: HashMap<String, HookableMethod> = HashMap::new();
    if let Some(tbn) = &tb_name {
        if let Some(c) = components.get(tbn) {
            for ci in &c.items {
                match ci {
                    ComponentItem::Field(f) => {
                        if let TypeExpr::Named { name, .. } = &f.ty {
                            let simple =
                                name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                            if let Some(id) = covgroup_ids.get(simple) {
                                cov_fields.push((f.name.name.clone(), *id));
                            }
                            if let Some(idx) =
                                prog.scoreboards.iter().position(|s| s.name == simple)
                            {
                                scoreboard_fields
                                    .push((f.name.name.clone(), ScoreboardId(idx as u32)));
                            }
                            if let Some(idx) =
                                prog.transactors.iter().position(|t| t.name == simple)
                            {
                                let xid = TransactorId(idx as u32);
                                // The transactor's module-typed field must
                                // drive the same DUT type this test
                                // instantiates — the IR's single-DUT model
                                // makes the bind static.
                                let xdut = &prog.transactors[idx].dut_type;
                                if *xdut != dut_type {
                                    return Err(unsupported(
                                        &format!(
                                            "transactor field `{tbn}.{} : {simple}` whose \
                                             `{}` field type `{xdut}` differs from the test \
                                             DUT type `{dut_type}`",
                                            f.name.name, prog.transactors[idx].dut_field
                                        ),
                                        "",
                                    ));
                                }
                                transactor_fields.push((f.name.name.clone(), xid));
                            }
                            // Composite-component testbench field (`prod :
                            // Producer` / `top : HeartbeatEnv`). Routes to
                            // the SAME `test_scope_components` collector a
                            // test-scope `let env : <Env>` uses — the IR
                            // models both identically (a default-constructed
                            // run-scope instance plus its `connect`/`on`
                            // wiring). v1 instead holds the field on the
                            // `_tb` struct (`_tb.prod`); this is a recorded
                            // C++-shape divergence with identical trace
                            // behavior (docs/tbir-mvp.md). A method-bearing
                            // scoreboard / analysis-source transactor is in
                            // `component_ids` (NOT `prog.scoreboards` /
                            // `prog.transactors`), so it lands here and not
                            // in the data-only routes above.
                            if let Some(cid) = component_ids.get(simple) {
                                test_scope_components.push((f.name.name.clone(), *cid));
                            }
                        } else if let Some(ty) = tb_scalar_field_ir_type(&f.ty) {
                            let default = match &f.default {
                                None => 0,
                                Some(d) => match &*d.kind {
                                    ExprKind::Int(s) => {
                                        exprs::parse_int_literal(s).ok_or_else(|| {
                                            unsupported(
                                                &format!(
                                                    "testbench field default `{} default {s}`",
                                                    f.name.name
                                                ),
                                                "not a plain integer literal",
                                            )
                                        })?
                                    }
                                    ExprKind::Bool(b) => *b as u64,
                                    _ => {
                                        return Err(unsupported(
                                            &format!(
                                                "a non-literal default on testbench field `{}`",
                                                f.name.name
                                            ),
                                            "",
                                        ));
                                    }
                                },
                            };
                            scalar_fields.push(ir::TbScalarFieldSchema {
                                name: f.name.name.clone(),
                                ty,
                                default,
                            });
                        }
                    }
                    ComponentItem::Hookable(h) => {
                        tb_methods.insert(h.name.name.clone(), h.clone());
                    }
                    _ => {}
                }
            }
        }
    }
    // Merge test-scope-let transactor instances into the transactor
    // field set so the call/bind/regblock machinery resolves them
    // uniformly. They are accessed by their BARE name (test-scope lets
    // aren't `_tb`-prefixed by the desugaring), recorded separately so
    // resolution knows which access shape to expect.
    let mut bare_transactor_fields: HashSet<String> =
        test_scope_xactors.iter().map(|(n, _)| n.clone()).collect();
    for (name, xid) in &test_scope_xactors {
        if transactor_fields.iter().any(|(f, _)| f == name) {
            return Err(LowerError::Invalid(format!(
                "name `{name}` is both a testbench transactor field and a test-scope \
                 transactor instance in test `{}`",
                t.name.name
            )));
        }
        transactor_fields.push((name.clone(), *xid));
    }
    // Bound-to initiator-side BFM instances (`let helper : H active =
    // bind axil`). Validate the bound bus binding matches the
    // transactor's `bound to` bus, fill the placeholder bus prefix in
    // the (TYPE-shared) method bodies with the real binding name, and
    // register the helper as a bare-name active transactor field so the
    // regblock `via` frontdoor and direct method calls resolve. The
    // method bodies are shared per transactor TYPE, so the subset is one
    // bound instance per type per file — a second bind to a different
    // bus binding would clobber the first's filled prefix, so reject it.
    for (instance, xid, bus_field) in &initiator_bfm_binds {
        if transactor_fields.iter().any(|(f, _)| f == instance) {
            return Err(LowerError::Invalid(format!(
                "name `{instance}` is bound more than once in test `{}`",
                t.name.name
            )));
        }
        // The bound bus binding must be a `let <bus_field> : <Bus> =
        // bind dut` declared in this test, of the bound bus.
        let Some(binding) = bus_bindings.iter().find(|b| &b.field == bus_field) else {
            return Err(LowerError::Invalid(format!(
                "initiator-BFM `{instance}` is bound to `{bus_field}`, which is not a bus \
                 binding in test `{}`",
                t.name.name
            )));
        };
        let xschema = &prog.transactors[xid.index()];
        if xschema.bound_bus.as_deref() != Some(binding.bus.as_str()) {
            return Err(LowerError::Invalid(format!(
                "initiator-BFM `{instance} : {}` is bound to bus binding `{bus_field}` of bus \
                 `{}`, but the transactor is `bound to {}`",
                xschema.name,
                binding.bus,
                xschema.bound_bus.as_deref().unwrap_or("<none>"),
            )));
        }
        // Fill the placeholder bus prefix in the method bodies with the
        // real binding name. The bodies are shared per type; a second
        // bind to a different binding name is rejected (one instance per
        // type per file).
        let method_fns: Vec<usize> =
            xschema.methods.iter().map(|m| m.function.index()).collect();
        let xname = xschema.name.clone();
        for fidx in method_fns {
            if let Err(prev) =
                fill_initiator_bus_prefix(&mut prog.functions[fidx], bus_field)
            {
                return Err(unsupported(
                    &format!(
                        "initiator-BFM transactor `{xname}` bound to more than one bus \
                         binding (`{prev}`, `{bus_field}`)"
                    ),
                    "the initiator-BFM subset shares one method body per transactor type; \
                     multiple instances need per-instance bodies",
                ));
            }
        }
        transactor_fields.push((instance.clone(), *xid));
        bare_transactor_fields.insert(instance.clone());
    }
    // Composite-component instances → schema bindings (with the env's
    // resolved `connect` edges). A name collision with another binding
    // class would resolve ambiguously, so reject it.
    let mut component_field_map: HashMap<String, ir::ComponentId> = HashMap::new();
    let mut component_field_bindings: Vec<ir::ComponentFieldBinding> = Vec::new();
    for (field, cid) in &test_scope_components {
        if transactor_fields.iter().any(|(f, _)| f == field)
            || bus_binding_decls.contains_key(field)
            || component_field_map.contains_key(field)
        {
            return Err(LowerError::Invalid(format!(
                "name `{field}` is bound more than once in test `{}`",
                t.name.name
            )));
        }
        component_field_map.insert(field.clone(), *cid);
        component_field_bindings.push(ir::ComponentFieldBinding {
            field: field.clone(),
            component: *cid,
            connects: prog.components[cid.index()].connects.clone(),
        });
    }

    // Bus bindings (test-scope `let`s) and transactor fields (testbench
    // component fields) share the `CallTarget::TransactorMethod`
    // bus_field namespace; a name living in both would dispatch
    // ambiguously, so reject it here rather than shadow.
    for (field, _) in &transactor_fields {
        if bus_binding_decls.contains_key(field) {
            return Err(LowerError::Invalid(format!(
                "name `{field}` is both a bus binding and a transactor field in test `{}` — \
                 rename one; method calls through it would be ambiguous",
                t.name.name
            )));
        }
    }

    // Resolve regblock bindings now the transactor fields are known: the
    // `via <helper>` instance must be an active transactor field, and
    // that transactor must declare the frontdoor `write(addr, data)` /
    // `read(addr) -> data` methods. Build the per-binding access context
    // (carried in `LowerCtx`) plus the schema (carried for dump-ir).
    let transactor_field_ids: HashMap<&str, ir::TransactorId> = transactor_fields
        .iter()
        .map(|(f, x)| (f.as_str(), *x))
        .collect();
    let mut regblock_bindings_map: HashMap<String, regblock::RegblockBindingCtx> = HashMap::new();
    let mut regblock_binding_schemas: Vec<ir::RegblockBinding> = Vec::new();
    let mut regblock_init_order: Vec<String> = Vec::new();
    for (binding, rbid, helper_field) in &regblock_binds {
        if regblock_bindings_map.contains_key(binding) {
            return Err(LowerError::Invalid(format!(
                "duplicate regblock binding `{binding}` in test `{}`",
                t.name.name
            )));
        }
        // The binding name doubles as the mirror local; a name shared
        // with a bus binding, a transactor field, or a plain test-scope
        // let would shadow ambiguously at the access site. Reject.
        if bus_binding_decls.contains_key(binding)
            || transactor_fields.iter().any(|(f, _)| f == binding)
            || test_let_names.contains(binding)
        {
            return Err(LowerError::Invalid(format!(
                "name `{binding}` is a regblock binding and also a bus binding, transactor \
                 instance, or test-scope let in test `{}` — rename one",
                t.name.name
            )));
        }
        let Some(&xid) = transactor_field_ids.get(helper_field.as_str()) else {
            return Err(unsupported(
                &format!(
                    "regblock binding `{binding}` via `{helper_field}` (not an active \
                     transactor field of the testbench)"
                ),
                "the `via` helper must be a `transactor` instance that pokes the DUT; the \
                 bus-bound helper form is a follow-up slice",
            ));
        };
        // Validate the frontdoor methods exist at the expected arity so
        // a malformed helper fails at lowering, not at C++ compile.
        let xschema = &prog.transactors[xid.index()];
        for (m, n) in [("write", 2usize), ("read", 1usize)] {
            match xschema.method(m) {
                Some(ms) if ms.n_params == n => {}
                Some(ms) => {
                    return Err(LowerError::Invalid(format!(
                        "regblock `via` helper `{}` method `{m}` takes {} argument(s), \
                         the frontdoor needs {n}",
                        xschema.name, ms.n_params
                    )));
                }
                None => {
                    return Err(LowerError::Invalid(format!(
                        "regblock `via` helper `{}` has no `{m}(addr, data)`-style method",
                        xschema.name
                    )));
                }
            }
        }
        let rb = &prog.regblocks[rbid.index()];
        regblock_bindings_map.insert(
            binding.clone(),
            regblock::RegblockBindingCtx {
                record: rb.record,
                helper_field: helper_field.clone(),
                registers: rb.registers.clone(),
            },
        );
        regblock_binding_schemas.push(ir::RegblockBinding {
            field: binding.clone(),
            regblock: *rbid,
            helper_field: helper_field.clone(),
        });
        regblock_init_order.push(binding.clone());
    }

    // Resolve bound-to target-TLM responder binds: the bound bus binding
    // must exist in this test, its bus type must match the transactor's
    // `bound to` bus, and the instance name must be unique. Build the
    // per-instance state map (for test-scope `target.<field>` access) and
    // the actor schemas, and substitute the instance name into the
    // responder bodies' `TransactorState` placeholders (lowered with an
    // empty instance at transactor-decl time, before the bind was known).
    let mut target_tlm_actors: Vec<ir::TargetTlmActorSchema> = Vec::new();
    let mut target_state: HashMap<String, HashSet<String>> = HashMap::new();
    for (instance, xid, bus_field) in &target_tlm_binds {
        if target_state.contains_key(instance) {
            return Err(LowerError::Invalid(format!(
                "duplicate target-TLM responder instance `{instance}` in test `{}`",
                t.name.name
            )));
        }
        // The bound bus binding must be a `let <bus_field> : <Bus> = bind
        // dut` declared in this test.
        let Some(binding) = bus_bindings.iter().find(|b| &b.field == bus_field) else {
            return Err(LowerError::Invalid(format!(
                "target-TLM responder `{instance}` is bound to `{bus_field}`, which is not a \
                 bus binding in test `{}`",
                t.name.name
            )));
        };
        let xschema = &prog.transactors[xid.index()];
        if xschema.bound_bus.as_deref() != Some(binding.bus.as_str()) {
            return Err(LowerError::Invalid(format!(
                "target-TLM responder `{instance} : {}` is bound to bus binding `{bus_field}` \
                 of bus `{}`, but the transactor is `bound to {}`",
                xschema.name,
                binding.bus,
                xschema.bound_bus.as_deref().unwrap_or("<none>"),
            )));
        }
        // State map for test-scope `target.<field>` access.
        target_state.insert(
            instance.clone(),
            xschema.state_fields.iter().map(|f| f.name.clone()).collect(),
        );
        // Fill the instance into the responder bodies' state-access
        // placeholders. The responder `TbFunction`s are shared per
        // transactor TYPE across the whole file, so a second test binding
        // the same transactor to a DIFFERENT instance name would clobber
        // the first test's already-filled bodies. The subset is one
        // passive instance per bound transactor — reject the multi-
        // instance case loudly rather than silently mis-emit.
        let methods: Vec<usize> =
            xschema.target_methods.iter().map(|m| m.function.index()).collect();
        let xname = xschema.name.clone();
        for fidx in methods {
            if let Err(prev) =
                fill_transactor_state_instance(&mut prog.functions[fidx], instance)
            {
                return Err(unsupported(
                    &format!(
                        "bound-to transactor `{xname}` bound to more than one instance \
                         (`{prev}`, `{instance}`)"
                    ),
                    "the target-side TLM subset materializes one passive instance per bound \
                     transactor; multiple instances need per-instance responder bodies",
                ));
            }
        }
        target_tlm_actors.push(ir::TargetTlmActorSchema {
            instance: instance.clone(),
            bus_field: bus_field.clone(),
            transactor: *xid,
        });
    }

    // Resolve persistent state for the unbound DUT-poking transactor
    // instances (`drv : SeqXactor active` where `SeqXactor` declares a
    // `last_read : uint<32>` field). Same machinery as the bound-to
    // target form: register the per-instance state map (for test-scope
    // `drv.last_read` reads) and fill the instance into the method
    // bodies' `TransactorState`/`TransactorStateWrite` placeholders. The
    // method bodies are shared per transactor TYPE, so the subset is one
    // STATEFUL instance per type per file — a second instance would
    // clobber the first's filled instance name; reject it loudly.
    let mut unbound_state_actors: Vec<(String, ir::TransactorId)> = Vec::new();
    // Tracks which stateful transactor TYPES already have an instance.
    // The method bodies (and per-instance state map) are shared per
    // type, so the subset allows one stateful instance per type: a
    // second instance of the same type would clobber the first's filled
    // method-body instance name. Tracked independently of whether the
    // type has methods (a method-less stateful transactor still has one
    // shared per-instance struct in scope).
    let mut stateful_type_seen: HashMap<u32, String> = HashMap::new();
    for (field, xid) in &transactor_fields {
        let xschema = &prog.transactors[xid.index()];
        // Bound-to target instances are handled above (they appear in
        // `target_state`, not `transactor_fields`); a stateless unbound
        // transactor has no per-instance struct.
        if xschema.bound_bus.is_some() || xschema.state_fields.is_empty() {
            continue;
        }
        let xname = xschema.name.clone();
        if let Some(prev) = stateful_type_seen.get(&xid.0) {
            return Err(unsupported(
                &format!(
                    "stateful unbound transactor `{xname}` instantiated more than once \
                     (`{prev}`, `{field}`)"
                ),
                "the unbound state-field subset shares one method body per transactor \
                 type; multiple stateful instances need per-instance bodies",
            ));
        }
        stateful_type_seen.insert(xid.0, field.clone());
        if target_state.contains_key(field) {
            return Err(LowerError::Invalid(format!(
                "name `{field}` is both a stateful transactor instance and a target-TLM \
                 responder in test `{}`",
                t.name.name
            )));
        }
        target_state.insert(
            field.clone(),
            xschema.state_fields.iter().map(|f| f.name.clone()).collect(),
        );
        // Fill the instance into the (type-shared) method bodies'
        // `TransactorState`/`TransactorStateWrite` placeholders. With the
        // per-type uniqueness guarded above, this can only ever fill with
        // a single instance name, but `fill_transactor_state_instance`
        // still cross-checks defensively.
        let method_fns: Vec<usize> =
            xschema.methods.iter().map(|m| m.function.index()).collect();
        for fidx in method_fns {
            if let Err(prev) =
                fill_transactor_state_instance(&mut prog.functions[fidx], field)
            {
                return Err(unsupported(
                    &format!(
                        "stateful unbound transactor `{xname}` instantiated more than once \
                         (`{prev}`, `{field}`)"
                    ),
                    "the unbound state-field subset shares one method body per transactor \
                     type; multiple stateful instances need per-instance bodies",
                ));
            }
        }
        unbound_state_actors.push((field.clone(), *xid));
    }

    let tb_id = TestbenchId(prog.testbenches.len() as u32);
    prog.testbenches.push(TestbenchSchema {
        name: tb_schema_name,
        dut_field: "dut".to_string(),
        dut_type,
        cov_fields: cov_fields.clone(),
        scalar_fields: scalar_fields.clone(),
        bus_bindings: bus_bindings.clone(),
        transactor_fields: transactor_fields.clone(),
        scoreboard_fields: scoreboard_fields.clone(),
        regblock_bindings: regblock_binding_schemas,
        target_tlm_actors,
        component_fields: component_field_bindings,
        unbound_state_actors,
        synthetic,
    });

    // Assemble run/check statement lists. Mirrors v1: the coroutine
    // executes setup → run → check → teardown sequentially; the IR
    // splits them into a Run function (setup+run) and a Check function
    // (check+teardown) that backends emit back-to-back.
    // Hoisted test-scope lets first (v1 evaluates them at `main` scope
    // before the coroutine bootstraps — i.e. before any body statement
    // and before the first clock edge), then the body in scope order.
    let mut run_stmts: Vec<&AstStmt> = test_let_stmts.iter().collect();
    let n_hoisted_lets = run_stmts.len();
    let mut check_stmts: Vec<&AstStmt> = Vec::new();
    if let Some(s) = scope {
        if let Some(b) = &s.setup {
            collect_stmts(b, !synthetic, &mut run_stmts);
        }
        if let Some(b) = &s.run {
            collect_stmts(b, false, &mut run_stmts);
        }
        if let Some(b) = &s.check {
            collect_stmts(b, false, &mut check_stmts);
        }
        if let Some(b) = &s.teardown {
            collect_stmts(b, false, &mut check_stmts);
        }
    }
    if scope.is_some() && !bare_stmts.is_empty() {
        // v1 interleaves bare statements with scope blocks in item
        // order; the IR's run/check split cannot represent that
        // ordering yet, so reject rather than silently reorder.
        return Err(unsupported(
            "mixing bare statements with a `scope`/`run` block in one test",
            "",
        ));
    }
    run_stmts.extend(bare_stmts.iter().copied());
    if run_stmts.len() == n_hoisted_lets {
        return Err(LowerError::Invalid(format!(
            "test `{}` has no body — add a `scope sim`, `run`, or bare statements",
            t.name.name
        )));
    }

    // Inline `<phase>()` call sites with the phase block's statements. v1
    // emits each phase as a captured `[&]() -> void` lambda + a plain
    // call; the IR splices the body at the call site (observably identical
    // — the phase body runs in the run/check coroutine context). Recurses
    // so a phase may call another; a cycle is rejected.
    if !phases.is_empty() {
        let mut expanded_run = Vec::with_capacity(run_stmts.len());
        for s in &run_stmts {
            expand_phase_calls(s, &phases, &mut Vec::new(), &mut expanded_run, &t.name.name)?;
        }
        run_stmts = expanded_run;
        let mut expanded_check = Vec::with_capacity(check_stmts.len());
        for s in &check_stmts {
            expand_phase_calls(s, &phases, &mut Vec::new(), &mut expanded_check, &t.name.name)?;
        }
        check_stmts = expanded_check;
    }

    let ctx = LowerCtx {
        dut_field: "dut".to_string(),
        tb_field: if synthetic { None } else { Some("_tb".to_string()) },
        cov_fields: cov_fields.iter().cloned().collect(),
        covgroups: prog.covgroups.clone(),
        clock_names: clock_specs.iter().map(|c| c.name.clone()).collect(),
        record_ids: record_ids.clone(),
        records: prog.records.clone(),
        bus_bindings: bus_binding_decls,
        transactor_fields: transactor_fields.iter().cloned().collect(),
        transactors: prog.transactors.clone(),
        scoreboard_fields: scoreboard_fields.iter().cloned().collect(),
        scoreboards: prog.scoreboards.clone(),
        consts: consts.clone(),
        tb_scalar_fields: scalar_fields.iter().map(|f| f.name.clone()).collect(),
        tb_methods,
        test_scope_lets: test_let_names,
        regblock_bindings: regblock_bindings_map,
        regblock_init_order,
        bare_transactor_fields,
        target_state,
        components: prog.components.clone(),
        component_fields: component_field_map,
        txn_keeps: txn_keeps.clone(),
        randomize_problem_ids: randomize_problem_ids.clone(),
        tseqs: tseq_records.clone(),
    };

    // Synthesized auto-sampler functions, one per covergroup field, in
    // declaration order. Sampling is schema-driven (the bin counters
    // live in the emitted covergroup struct, outside IR locals), so
    // the function body is empty — the function records the
    // registration slot and covgroup binding for backends.
    for (field, cg) in &cov_fields {
        let id = FunctionId(prog.functions.len() as u32);
        prog.functions.push(TbFunction {
            id,
            name: format!("sample_{}_{}", t.name.name, field),
            kind: FunctionKind::SamplerAuto { covgroup: *cg },
            params: Vec::new(),
            locals: Vec::new(),
            blocks: vec![BasicBlock {
                stmts: Vec::new(),
                terminator: Terminator::Return,
            }],
            entry: BlockId(0),
            owner: Some(tb_id),
            ret: None,
        });
    }

    let run_id = FunctionId(prog.functions.len() as u32);
    let run_fn = lower_function(
        run_id,
        format!("run_{}", t.name.name),
        FunctionKind::Run,
        Some(tb_id),
        &run_stmts,
        &ctx,
        helpers,
        constraint_sites,
    )?;
    prog.functions.push(run_fn);

    let check = if check_stmts.is_empty() {
        None
    } else {
        let check_id = FunctionId(prog.functions.len() as u32);
        let check_fn = lower_function(
            check_id,
            format!("check_{}", t.name.name),
            FunctionKind::Check,
            Some(tb_id),
            &check_stmts,
            &ctx,
            helpers,
            constraint_sites,
        )?;
        prog.functions.push(check_fn);
        Some(check_id)
    };

    prog.tests.push(TestSchema {
        name: t.name.name.clone(),
        testbench: tb_id,
        run: run_id,
        check,
        clock_domain: clock_specs.first().and_then(|c| c.domain.clone()),
        clocks: clock_specs,
    });
    Ok(())
}

/// Recognize a bare phase-call statement `<name>()` — `StmtKind::Expr`
/// of a zero-arg `Call` whose callee is a plain identifier naming a
/// declared `phase`. Returns the phase name on a match.
fn phase_call_name<'a>(s: &AstStmt, phases: &HashMap<String, &'a Block>) -> Option<String> {
    let StmtKind::Expr(e) = &s.kind else {
        return None;
    };
    let ExprKind::Call { callee, args } = &*e.kind else {
        return None;
    };
    if !args.is_empty() {
        return None;
    }
    let ExprKind::Ident(id) = &*callee.kind else {
        return None;
    };
    phases.contains_key(&id.name).then(|| id.name.clone())
}

/// Inline a single statement into `out`, expanding any `<phase>()` call
/// site with the phase block's statements (recursively, so a phase may
/// call another). `active` tracks the phase-expansion stack to reject a
/// recursive cycle. A non-phase-call statement is pushed unchanged.
fn expand_phase_calls<'a>(
    s: &'a AstStmt,
    phases: &HashMap<String, &'a Block>,
    active: &mut Vec<String>,
    out: &mut Vec<&'a AstStmt>,
    test_name: &str,
) -> Result<(), LowerError> {
    if let Some(name) = phase_call_name(s, phases) {
        if active.contains(&name) {
            return Err(LowerError::Invalid(format!(
                "phase `{name}` is called recursively in test `{test_name}`"
            )));
        }
        active.push(name.clone());
        let body = phases[&name];
        for inner in &body.stmts {
            expand_phase_calls(inner, phases, active, out, test_name)?;
        }
        active.pop();
        return Ok(());
    }
    out.push(s);
    Ok(())
}

/// Flatten a block's statements, optionally skipping the synthesized
/// `_tb.dut = dut` wire (it is scaffolding-owned in the IR backend —
/// the emitter wires the TB struct's DUT pointer before the body runs).
fn collect_stmts<'a>(b: &'a Block, skip_tb_wire: bool, out: &mut Vec<&'a AstStmt>) {
    for s in &b.stmts {
        if skip_tb_wire && is_tb_dut_wire(s) {
            continue;
        }
        out.push(s);
    }
}

/// Scalar IR type of a testbench member field (`expected : uint<32>`),
/// or `None` when the type is outside the scalar subset. Mirrors v1's
/// `component_field_c_type` → `txn_field_c_type` C-type choice for
/// the ≤64-bit subset.
pub(super) fn tb_scalar_field_ir_type(t: &TypeExpr) -> Option<IrType> {
    let TypeExpr::Builtin { name, args, .. } = t else {
        return None;
    };
    let width = match args.first() {
        Some(crate::ast::TypeArg::Expr(e)) => match &*e.kind {
            ExprKind::Int(s) => Some(s.replace('_', "").parse::<u32>().ok()?),
            _ => return None,
        },
        Some(_) => return None,
        None => None,
    };
    if width.is_some_and(|w| w == 0 || w > 64) {
        return None;
    }
    match name {
        BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits => Some(IrType::UInt(width)),
        BuiltinTy::SInt | BuiltinTy::SIntCap => Some(IrType::SInt(width)),
        BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => Some(IrType::Bool),
        _ => None,
    }
}

/// Simple (last-segment) name of a `Named` type expression, if any.
fn type_simple_name(t: Option<&TypeExpr>) -> Option<&str> {
    match t? {
        TypeExpr::Named { name, .. } => name.segments.last().map(|s| s.name.as_str()),
        _ => None,
    }
}

fn is_tb_dut_wire(s: &AstStmt) -> bool {
    let StmtKind::Assign { target, value } = &s.kind else {
        return false;
    };
    let ExprKind::Field { target: ft, name } = &*target.kind else {
        return false;
    };
    let (ExprKind::Ident(root), ExprKind::Ident(v)) = (&*ft.kind, &*value.kind) else {
        return false;
    };
    root.name == "_tb" && name.name == "dut" && v.name == "dut"
}

pub(crate) fn time_literal_to_ps(s: &str) -> Result<i64, String> {
    let digits: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '_')
        .collect();
    let unit: String = s.chars().skip(digits.len()).collect();
    let factor: i64 = match unit.as_str() {
        "ps" => 1,
        "ns" => 1_000,
        "us" => 1_000_000,
        "ms" => 1_000_000_000,
        "s" => 1_000_000_000_000,
        other => {
            return Err(format!(
                "unsupported time unit `{other}` in `{s}` (expected ps/ns/us/ms/s)"
            ));
        }
    };
    // i64 picoseconds caps each unit at i64::MAX / factor (e.g. 9_223_372 s).
    let overflow = || {
        format!(
            "time literal `{s}` overflows the picosecond range (max {}{unit})",
            i64::MAX / factor
        )
    };
    let n: i64 = digits.replace('_', "").parse().map_err(|e| {
        if matches!(
            std::num::ParseIntError::kind(&e),
            std::num::IntErrorKind::PosOverflow
        ) {
            overflow()
        } else {
            format!("bad number in time literal `{s}`")
        }
    })?;
    n.checked_mul(factor).ok_or_else(overflow)
}

// ── Function builder ─────────────────────────────────────────────────

/// Per-test lowering context shared by all of the test's functions.
pub(crate) struct LowerCtx {
    /// Test-scope DUT field name (`"dut"`).
    pub dut_field: String,
    /// `Some("_tb")` for impl-form tests (testbench-bound).
    pub tb_field: Option<String>,
    /// Covergroup-typed testbench fields (`cov` → covgroup id).
    pub cov_fields: HashMap<String, ir::CovgroupId>,
    /// Snapshot of the program's covgroup schemas, for point/bin
    /// validation at `cov.<point>.<bin>` lowering sites.
    pub covgroups: Vec<CovgroupSchema>,
    /// Declared clock names in declaration order (index == the
    /// `TestSchema::clocks` / runtime scheduler index). Consulted by
    /// `wait N cycles on <clock>` lowering; empty for clockless tests
    /// and for the file-level pure-helper context (a pure helper can
    /// never contain a wait — waits make a helper impure, so it is
    /// CFG-inlined under the calling test's context instead).
    pub clock_names: Vec<String>,
    /// Transaction record names → ids, for `let t : TxnType`
    /// resolution (the IR mirror of v1's `transactions` set seeding
    /// let-type resolution).
    pub record_ids: HashMap<String, RecordId>,
    /// Snapshot of the program's record schemas, for field validation
    /// at `t.<field>` lowering sites.
    pub records: Vec<RecordSchema>,
    /// Test-scope bus bindings: binding name → bus declaration (with
    /// any unsupported features already rejected at the bind site).
    /// Empty for the file-level pure-helper context, so a pure helper
    /// can never resolve a bus access — which structurally keeps
    /// `TransactorMethod` call edges out of pure-helper bodies (design
    /// seam rule).
    pub bus_bindings: HashMap<String, crate::ast::BusDecl>,
    /// Transactor-typed testbench fields (`xact` → transactor id), for
    /// `xact.method(...)` call resolution and the `xact.dut = dut`
    /// bind. Disjoint from `bus_bindings` (collision rejected at
    /// testbench-schema construction). Empty for synthetic testbenches,
    /// helper contexts, and transactor method bodies.
    pub transactor_fields: HashMap<String, ir::TransactorId>,
    /// Snapshot of the program's transactor schemas, for method
    /// validation at call sites.
    pub transactors: Vec<TransactorSchema>,
    /// Scoreboard-typed testbench fields (`sb` → scoreboard id), for
    /// `sb.<field>` / `sb.<queue>.push(...)` resolution. Empty for
    /// synthetic testbenches (no `_tb` to hold the instance), helper
    /// contexts, and transactor method bodies.
    pub scoreboard_fields: HashMap<String, ScoreboardId>,
    /// Snapshot of the program's scoreboard schemas, for field
    /// validation at op/query sites.
    pub scoreboards: Vec<ScoreboardSchema>,
    /// File-scope named integer constants: `const` declarations plus
    /// enum variant names (variant index, first definition wins —
    /// v1's `enum_variants` rule). Substituted as literals at use
    /// sites; locals shadow (lookup order: local, then const — same
    /// effective shadowing as v1's C++ scoping).
    pub consts: HashMap<String, u64>,
    /// Scalar testbench field names (`TestbenchSchema::scalar_fields`),
    /// for `_tb.<field>` access lowering.
    pub tb_scalar_fields: HashSet<String>,
    /// Testbench helper methods (`function`/`hookable` declared inside
    /// the bound testbench), CFG-inlined at `_tb.<m>(...)` call sites
    /// like impure helpers — v1 emits them as `[&]`-capturing lambdas
    /// whose waits tick the same scheduler.
    pub tb_methods: HashMap<String, HookableMethod>,
    /// Test-scope let names (hoisted into the run function). Used for
    /// a precise rejection when the check phase references one — run
    /// and check are separate IR functions, so v1's shared-capture
    /// scoping is not representable.
    pub test_scope_lets: HashSet<String>,
    /// Register-block bindings (`let regs : R = bind <helper>`) →
    /// per-binding access context (mirror record, helper field,
    /// registers). Empty for helper/method/synthetic contexts.
    pub regblock_bindings: HashMap<String, regblock::RegblockBindingCtx>,
    /// Regblock binding names in declaration order. The Run function
    /// declares + `RecordInit`s each mirror local at entry, in this
    /// order — v1 declares the `<Name>_Mirror` struct once at the
    /// hoisted-let site. The mirror is run-scoped (a check-phase access
    /// is a precise rejection, like a test-scope let).
    pub regblock_init_order: Vec<String>,
    /// Transactor instances declared as test-scope lets (`let h :
    /// Xactor active`) rather than testbench fields. Accessed by their
    /// BARE name (`h.method(...)`, `h.dut = dut`) — the impl-for
    /// desugaring rewrites testbench-field access to `_tb.<field>` but
    /// leaves test-scope lets unqualified, so resolution must accept
    /// both shapes. A subset of `transactor_fields` keys.
    pub bare_transactor_fields: HashSet<String>,
    /// Bound-to target-transactor instances → their persistent state
    /// field names. Populated at test binding for `passive` instances of
    /// `transactor X bound to <Bus>` transactors. Resolves test-scope
    /// reads/writes `target.<field>` to `ir::Expr::TransactorState` /
    /// `ir::Stmt::TransactorStateWrite`. Empty everywhere else.
    pub target_state: HashMap<String, HashSet<String>>,
    /// Snapshot of the program's component schemas, for path/field/method
    /// resolution at access sites.
    pub components: Vec<ComponentSchema>,
    /// Test-scope composite-component instances (`let env : AnalysisEnv`)
    /// → `ComponentId`. A bare access whose head segment is in this map
    /// resolves through the component path machinery (`env.source.publish`,
    /// `env.sb.count`). Empty in helper/method/transactor contexts.
    pub component_fields: HashMap<String, ir::ComponentId>,
    /// Per-transaction `keep` constraint clauses as AST expressions, by
    /// transaction name. Merged ahead of a `randomize(t)` call-site
    /// `with {...}` body (v1's spec-§4 merge) when building the
    /// `ConstraintSite`. Empty for keep-free transactions and for
    /// contexts that cannot host a `randomize` (pure helpers).
    pub txn_keeps: HashMap<String, Vec<crate::ast::Expr>>,
    /// Randomize-target span → typed constraint-problem id. The handle
    /// (`ConstraintProblemId.0`) the constraint-IR layer assigned to the
    /// site, keyed exactly like v1's `runtime_randomize_problem_ids`.
    /// `None` at a site means no Z3-ready problem (lower/backend error).
    pub randomize_problem_ids: HashMap<(usize, usize), u32>,
    /// `tseq` name → element record type. A `let txns = Name(args)` whose
    /// callee is in this map lowers to a `CallTarget::Tseq` whose result
    /// types the local as `IrType::RecordSeq(record)`, and a `for t in
    /// txns` over such a local lowers to a counted loop over `txns`.
    pub tseqs: HashMap<String, RecordId>,
}

pub(crate) struct LoopFrame {
    pub continue_to: BlockId,
    pub break_to: BlockId,
}

/// One in-flight helper inline (innermost last). While a frame is
/// active, name lookup is fenced to scopes opened inside the frame
/// (helpers are free functions — they do not capture caller locals),
/// `break`/`continue` cannot bind to caller loops, and `return e`
/// becomes `Assign(ret_dest, e); Jump(ret_cont)`.
pub(crate) struct InlineFrame {
    /// Helper name, for recursion detection.
    pub(crate) name: String,
    /// Param names bound to the caller's DUT — `as_port_ref` resolves
    /// `<alias>.<port>` exactly like `dut.<port>`.
    pub(crate) dut_aliases: HashSet<String>,
    pub(crate) ret_dest: LocalId,
    pub(crate) ret_cont: BlockId,
    /// `scopes.len()` at frame entry — lookup floor.
    pub(crate) scope_floor: usize,
    /// `loop_stack.len()` at frame entry — break/continue floor.
    pub(crate) loop_floor: usize,
}

struct BlockInProgress {
    stmts: Vec<ir::Stmt>,
    term: Option<Terminator>,
}

pub(crate) struct FuncBuilder<'a> {
    pub(crate) ctx: &'a LowerCtx,
    pub(crate) helpers: &'a helpers::HelperRegistry<'a>,
    locals: Vec<TypedLocal>,
    local_names: HashSet<String>,
    scopes: Vec<HashMap<String, LocalId>>,
    blocks: Vec<BlockInProgress>,
    current: usize,
    pub(crate) loop_stack: Vec<LoopFrame>,
    temp_counter: u32,
    pub(crate) inline_frames: Vec<InlineFrame>,
    /// Return slot when lowering a standalone pure-helper body.
    pub(crate) helper_ret: Option<LocalId>,
    /// True while lowering a standalone pure-helper body — record
    /// locals are rejected there (pure helpers emit as file-scope
    /// uint64-only C++ functions in the tbir backend).
    pub(crate) in_pure_helper: bool,
    /// True while lowering `${...}` captures of a log/fail message —
    /// impure helper calls cannot inline there (messages evaluate
    /// lazily at the failure site).
    pub(crate) in_fmt_args: bool,
    /// True while lowering a transactor method body. Methods keep v1's
    /// synchronous hookable semantics (waits emit as `tick()` loops),
    /// so the constructs whose sync emission is out of this slice —
    /// clock-qualified waits and timed `wait until` — are rejected
    /// here, as are nested transactor calls.
    pub(crate) in_transactor_method: bool,
    /// State fields visible to a bound-to target-responder body
    /// (`thread bus.<m>(...)`). A bare ident that hits this set lowers
    /// to `ir::Expr::TransactorState`/`ir::Stmt::TransactorStateWrite` with an
    /// empty `instance` placeholder; the test-binding stage fills the
    /// instance once the passive transactor field is resolved. Empty in
    /// every non-responder context, so the resolution path is inert.
    pub(crate) target_state_fields: HashSet<String>,
    /// True while lowering a Check-kind function — used for the
    /// precise test-scope-let rejection (see `LowerCtx::
    /// test_scope_lets`).
    pub(crate) in_check: bool,
    /// Best-effort bit widths of locals with an explicit scalar type
    /// annotation (`let s64 : uint<64> = ...`). Consulted only by the
    /// width-method receiver inference (v1's `let_widths`); the
    /// declared `TypedLocal::ty` deliberately stays `Unknown` (see
    /// docs/tbir-mvp.md divergence 4).
    pub(crate) let_widths: HashMap<LocalId, u32>,
    /// `Some(component)` while lowering a `ComponentMethod` body: a bare
    /// field name that names a field of that component resolves self-
    /// relatively (`Expr::ComponentField { base: SelfField }`), and
    /// `emit <ev>(...)` resolves against the body's `out event` fields.
    pub(crate) self_component: Option<ir::ComponentId>,
    /// Program-wide constraint-site table, shared across every function
    /// lowered for one program so `ConstraintRef` indices are globally
    /// unique. A `randomize` site appends here and the resulting index
    /// becomes the terminator's `ConstraintRef`. `lower_program` drains
    /// it into `TbProgram::constraint_sites` after all functions lower.
    pub(crate) constraint_sites: &'a RefCell<Vec<ConstraintSite>>,
    /// Payload-field bindings for `recv()`-captured locals: `let r =
    /// bus.<ch>.recv()` records `r → [(field, captured-local)]` so a
    /// later `r.<field>` read resolves to the per-field captured local
    /// (v1 captures the whole payload struct; the IR captures each
    /// payload signal into its own local). The bare local (`r`) still
    /// holds the FIRST payload signal — preserving scalar `recv()`
    /// reads (`let v = bus.r.recv(); assert v == ...`) — so this map is
    /// consulted only for the dotted `r.<field>` form.
    pub(crate) recv_payloads: HashMap<LocalId, Vec<(String, LocalId)>>,

    /// The `RecordSeq` accumulator local of the `tseq` body currently
    /// being lowered (`Some` only inside a `FunctionKind::Tseq` body). A
    /// `yield t` lowers to `Stmt::SeqPush { seq: this, value: t }`; a
    /// `yield` reaching lowering with `None` is rejected (yield outside a
    /// tseq), matching v1's "`yield` outside a `tseq` body" error.
    pub(crate) tseq_result: Option<LocalId>,
}

#[allow(clippy::too_many_arguments)]
fn lower_function<'a>(
    id: FunctionId,
    name: String,
    kind: FunctionKind,
    owner: Option<TestbenchId>,
    stmts: &[&AstStmt],
    ctx: &'a LowerCtx,
    helpers: &'a helpers::HelperRegistry<'a>,
    constraint_sites: &'a RefCell<Vec<ConstraintSite>>,
) -> Result<TbFunction, LowerError> {
    let mut b = FuncBuilder::new(ctx, helpers, constraint_sites);
    b.in_check = kind == FunctionKind::Check;
    // Regblock mirror locals: declared + default-constructed (to their
    // reset values) at the head of the Run function, mirroring v1's
    // single `<Name>_Mirror regs;` declaration at the hoisted-let site.
    // Run-scoped — a check-phase regblock access fails the binding
    // lookup and is rejected precisely (like a test-scope let).
    if kind == FunctionKind::Run {
        for binding in &ctx.regblock_init_order {
            let rec = ctx.regblock_bindings[binding].record;
            let id = b.declare(binding);
            b.set_local_type(id, IrType::Record(rec));
            b.push(ir::Stmt::RecordInit(id, rec));
        }
    }
    for s in stmts {
        b.lower_stmt(s)?;
    }
    if !b.is_terminated() {
        b.terminate(Terminator::Return);
    }
    b.finish(id, name, kind, owner)
}

impl<'a> FuncBuilder<'a> {
    pub(crate) fn new(
        ctx: &'a LowerCtx,
        helpers: &'a helpers::HelperRegistry<'a>,
        constraint_sites: &'a RefCell<Vec<ConstraintSite>>,
    ) -> Self {
        FuncBuilder {
            ctx,
            helpers,
            locals: Vec::new(),
            local_names: HashSet::new(),
            scopes: vec![HashMap::new()],
            blocks: vec![BlockInProgress {
                stmts: Vec::new(),
                term: None,
            }],
            current: 0,
            loop_stack: Vec::new(),
            temp_counter: 0,
            inline_frames: Vec::new(),
            helper_ret: None,
            in_pure_helper: false,
            in_fmt_args: false,
            in_transactor_method: false,
            target_state_fields: HashSet::new(),
            in_check: false,
            let_widths: HashMap::new(),
            self_component: None,
            constraint_sites,
            recv_payloads: HashMap::new(),
            tseq_result: None,
        }
    }

    /// Mark the `RecordSeq` accumulator of the tseq body being lowered, so
    /// `yield` knows its push target.
    pub(crate) fn set_tseq_result(&mut self, acc: LocalId) {
        self.tseq_result = Some(acc);
    }
}

impl FuncBuilder<'_> {
    pub(crate) fn new_block(&mut self) -> BlockId {
        self.blocks.push(BlockInProgress {
            stmts: Vec::new(),
            term: None,
        });
        BlockId((self.blocks.len() - 1) as u32)
    }

    /// Make `b` the current insertion block. The block must not be
    /// terminated yet.
    pub(crate) fn start_block(&mut self, b: BlockId) {
        debug_assert!(self.blocks[b.index()].term.is_none());
        self.current = b.index();
    }

    pub(crate) fn push(&mut self, s: ir::Stmt) {
        debug_assert!(self.blocks[self.current].term.is_none());
        self.blocks[self.current].stmts.push(s);
    }

    pub(crate) fn terminate(&mut self, t: Terminator) {
        debug_assert!(self.blocks[self.current].term.is_none());
        self.blocks[self.current].term = Some(t);
    }

    pub(crate) fn is_terminated(&self) -> bool {
        self.blocks[self.current].term.is_some()
    }

    /// After a `break`/`continue`/`return` mid-block, trailing source
    /// statements are dead code; they lower into a fresh block that the
    /// reachability prune in `finish` removes.
    pub(crate) fn ensure_open_block(&mut self) {
        if self.is_terminated() {
            let b = self.new_block();
            self.start_block(b);
        }
    }

    pub(crate) fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    pub(crate) fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    pub(crate) fn scope_depth(&self) -> usize {
        self.scopes.len()
    }

    /// True when `name` refers to the caller's DUT in the current
    /// context: the test-scope DUT field itself (helpers see it the
    /// way v1's `[&]`-capturing lambdas do), or — inside an inline
    /// frame — a helper parameter bound to the DUT at the call site.
    pub(crate) fn is_dut_name(&self, name: &str) -> bool {
        if let Some(f) = self.inline_frames.last() {
            if f.dut_aliases.contains(name) {
                return true;
            }
        }
        name == self.ctx.dut_field
    }

    /// Declare a new local for a source name. Shadowed / reused names
    /// get a deduplicated stored name so backends can emit them as
    /// identifiers directly.
    pub(crate) fn declare(&mut self, source_name: &str) -> LocalId {
        let base = if source_name == "_" {
            "_anon".to_string()
        } else {
            source_name.to_string()
        };
        let mut candidate = base.clone();
        let mut n = 2;
        while !self.local_names.insert(candidate.clone()) {
            candidate = format!("{base}_{n}");
            n += 1;
        }
        let id = LocalId(self.locals.len() as u32);
        self.locals.push(TypedLocal {
            name: candidate,
            ty: IrType::Unknown,
        });
        self.scopes
            .last_mut()
            .expect("scope stack never empty")
            .insert(source_name.to_string(), id);
        id
    }

    pub(crate) fn fresh_temp(&mut self) -> LocalId {
        let name = loop {
            let candidate = format!("__t{}", self.temp_counter);
            self.temp_counter += 1;
            if self.local_names.insert(candidate.clone()) {
                break candidate;
            }
        };
        let id = LocalId(self.locals.len() as u32);
        self.locals.push(TypedLocal {
            name,
            ty: IrType::Unknown,
        });
        id
    }

    /// Name resolution, fenced at the innermost inline frame: helpers
    /// are free functions, so an inlined body must not see the caller's
    /// locals — only scopes opened inside the frame (its params and its
    /// own `let`s) resolve.
    pub(crate) fn lookup(&self, name: &str) -> Option<LocalId> {
        let floor = self
            .inline_frames
            .last()
            .map_or(0, |f| f.scope_floor.min(self.scopes.len()));
        for scope in self.scopes[floor..].iter().rev() {
            if let Some(id) = scope.get(name) {
                return Some(*id);
            }
        }
        None
    }

    pub(crate) fn set_local_type(&mut self, l: LocalId, ty: IrType) {
        self.locals[l.index()].ty = ty;
    }

    /// `Some(record)` when the local is record-typed (`let t : Txn`).
    pub(crate) fn record_of_local(&self, l: LocalId) -> Option<ir::RecordId> {
        match self.locals[l.index()].ty {
            IrType::Record(r) => Some(r),
            _ => None,
        }
    }

    /// `Some(record)` when the local is a transaction-sequence
    /// (`let txns = SomeTseq(...)`, typed `RecordSeq`).
    pub(crate) fn seq_of_local(&self, l: LocalId) -> Option<ir::RecordId> {
        match self.locals[l.index()].ty {
            IrType::RecordSeq(r) => Some(r),
            _ => None,
        }
    }

    /// Append a constraint site to the program-wide table and return its
    /// `ConstraintRef` handle (the index). Used by `randomize` lowering.
    pub(crate) fn push_constraint_site(&self, site: ConstraintSite) -> ConstraintRef {
        let mut sites = self.constraint_sites.borrow_mut();
        let id = ConstraintRef(sites.len() as u32);
        sites.push(site);
        id
    }

    /// Seal all blocks, prune the ones unreachable from the entry
    /// (block 0), and remap successor ids.
    fn finish(
        self,
        id: FunctionId,
        name: String,
        kind: FunctionKind,
        owner: Option<TestbenchId>,
    ) -> Result<TbFunction, LowerError> {
        let sealed: Vec<BasicBlock> = self
            .blocks
            .into_iter()
            .map(|b| BasicBlock {
                stmts: b.stmts,
                terminator: b.term.unwrap_or(Terminator::Return),
            })
            .collect();

        // Reachability from block 0.
        let mut reachable = vec![false; sealed.len()];
        let mut work = vec![0usize];
        while let Some(i) = work.pop() {
            if std::mem::replace(&mut reachable[i], true) {
                continue;
            }
            for s in sealed[i].terminator.successors() {
                work.push(s.index());
            }
        }
        let mut remap = vec![BlockId(0); sealed.len()];
        let mut kept = Vec::new();
        for (i, b) in sealed.into_iter().enumerate() {
            if reachable[i] {
                remap[i] = BlockId(kept.len() as u32);
                kept.push(b);
            }
        }
        for b in &mut kept {
            remap_terminator(&mut b.terminator, &remap);
        }

        Ok(TbFunction {
            id,
            name,
            kind,
            params: Vec::<TypedParam>::new(),
            locals: self.locals,
            blocks: kept,
            entry: BlockId(0),
            owner,
            ret: self.helper_ret,
        })
    }
}

fn remap_terminator(t: &mut Terminator, remap: &[BlockId]) {
    let m = |b: &mut BlockId| *b = remap[b.index()];
    match t {
        Terminator::Jump(b) => m(b),
        Terminator::Branch(_, a, b) => {
            m(a);
            m(b);
        }
        Terminator::WaitCycles(_, _, b) => m(b),
        Terminator::WaitCyclesSync(_, b) => m(b),
        Terminator::WaitTimePs(_, b) => m(b),
        Terminator::WaitUntil { succ, .. } => m(succ),
        Terminator::WaitUntilTimeout {
            on_fire,
            on_timeout,
            ..
        } => {
            m(on_fire);
            m(on_timeout);
        }
        Terminator::Randomize { succ, .. } => m(succ),
        Terminator::Return | Terminator::Fatal(_) => {}
    }
}

/// Fill the bound responder instance name into a target-method body's
/// `TransactorState` / `TransactorStateWrite` placeholders (lowered with
/// an empty instance at transactor-decl time). The responder bodies are
/// shared per transactor TYPE across the file, so this is idempotent for
/// the same instance but `Err(prev)` when a state node was already filled
/// with a DIFFERENT instance (a second test binding the same transactor
/// to another name) — the caller turns that into an `Unsupported`. The
/// scan-then-fill split keeps the body un-mutated on the error path.
fn fill_transactor_state_instance(func: &mut TbFunction, instance: &str) -> Result<(), String> {
    if let Some(prev) = existing_state_instance(func) {
        if prev != instance {
            return Err(prev);
        }
    }
    fill_transactor_state_instance_unchecked(func, instance);
    Ok(())
}

/// First non-empty instance name already present on any `TransactorState`
/// / `TransactorStateWrite` node in the body, or `None` (all empty
/// placeholders, the common single-bind case).
fn existing_state_instance(func: &TbFunction) -> Option<String> {
    fn in_expr(e: &ir::Expr) -> Option<String> {
        match e {
            ir::Expr::TransactorState { instance, .. } if !instance.is_empty() => {
                Some(instance.clone())
            }
            ir::Expr::Binary(_, a, b) => in_expr(a).or_else(|| in_expr(b)),
            ir::Expr::Unary(_, a) | ir::Expr::WidthCast { inner: a, .. } => in_expr(a),
            ir::Expr::Ternary(c, t, f) => in_expr(c).or_else(|| in_expr(t)).or_else(|| in_expr(f)),
            ir::Expr::Call(_, args) => args.iter().find_map(in_expr),
            // Component fields never carry a transactor-state instance.
            ir::Expr::ComponentField { .. } => None,
            _ => None,
        }
    }
    for block in &func.blocks {
        for s in &block.stmts {
            let found = match s {
                ir::Stmt::TransactorStateWrite { instance, value, .. } => {
                    if !instance.is_empty() {
                        Some(instance.clone())
                    } else {
                        in_expr(value)
                    }
                }
                ir::Stmt::Assign(_, e) | ir::Stmt::DutWrite(_, e) => in_expr(e),
                ir::Stmt::RecordFieldWrite { value, .. } | ir::Stmt::TbFieldWrite { value, .. } => {
                    in_expr(value)
                }
                ir::Stmt::AssertCheck { cond, on_fail } => {
                    in_expr(cond).or_else(|| on_fail.args.iter().find_map(|a| in_expr(&a.expr)))
                }
                ir::Stmt::Log { args, .. } | ir::Stmt::FailDiag { args, .. } => {
                    args.args.iter().find_map(|a| in_expr(&a.expr))
                }
                ir::Stmt::TransactorCall { call, .. } => in_expr(call),
                ir::Stmt::ScoreboardOp { op, .. } => match op {
                    ir::ScoreboardOp::QueuePush { value, .. }
                    | ir::ScoreboardOp::ScalarWrite { value, .. } => in_expr(value),
                    ir::ScoreboardOp::QueuePop { .. } => None,
                },
                // Component-method bodies never reach this TLM target-
                // state filler (they are not bound-to target responders);
                // any expr they carry holds no transactor-state node.
                ir::Stmt::ComponentFieldWrite { value, .. } => in_expr(value),
                ir::Stmt::ComponentEmit { args, .. } => args.iter().find_map(in_expr),
                ir::Stmt::ComponentCall { args, .. } => args.iter().find_map(in_expr),
                // tseq bodies never appear in a bound-to responder body
                // (transactor-method randomize / tseq is out of subset),
                // so the yielded value holds no transactor-state node.
                ir::Stmt::SeqPush { value, .. } => in_expr(value),
                ir::Stmt::DutRead(_, _) | ir::Stmt::RecordInit(_, _) | ir::Stmt::CovReport(_) => {
                    None
                }
            };
            if found.is_some() {
                return found;
            }
        }
    }
    None
}

fn fill_transactor_state_instance_unchecked(func: &mut TbFunction, instance: &str) {
    fn fill_expr(e: &mut ir::Expr, instance: &str) {
        match e {
            ir::Expr::TransactorState { instance: i, .. } => {
                debug_assert!(
                    i.is_empty() || i == instance,
                    "target-state instance already filled with a different name"
                );
                *i = instance.to_string();
            }
            ir::Expr::Binary(_, a, b) => {
                fill_expr(a, instance);
                fill_expr(b, instance);
            }
            ir::Expr::Unary(_, a) => fill_expr(a, instance),
            ir::Expr::Ternary(c, t, f) => {
                fill_expr(c, instance);
                fill_expr(t, instance);
                fill_expr(f, instance);
            }
            ir::Expr::WidthCast { inner, .. } => fill_expr(inner, instance),
            ir::Expr::ComponentIdle { n, .. } => fill_expr(n, instance),
            ir::Expr::SeqIndex { index, .. } => fill_expr(index, instance),
            ir::Expr::Call(_, args) => {
                for a in args {
                    fill_expr(a, instance);
                }
            }
            ir::Expr::Literal { .. }
            | ir::Expr::WideLiteral(_)
            | ir::Expr::Local(_)
            | ir::Expr::CycleCount
            | ir::Expr::Port(_)
            | ir::Expr::RecordField { .. }
            | ir::Expr::TbField(_)
            | ir::Expr::ComponentField { .. }
            | ir::Expr::ScoreboardQuery { .. }
            | ir::Expr::SeqLen(_)
            | ir::Expr::CovBin { .. } => {}
        }
    }
    fn fill_term(t: &mut Terminator, instance: &str) {
        match t {
            Terminator::Branch(c, _, _) => fill_expr(c, instance),
            Terminator::WaitCycles(n, _, _) | Terminator::WaitCyclesSync(n, _) => {
                fill_expr(n, instance)
            }
            Terminator::WaitUntil { preds, .. } => {
                for p in preds {
                    fill_expr(&mut p.expr, instance);
                }
            }
            Terminator::WaitUntilTimeout { preds, cycles, .. } => {
                for p in preds {
                    fill_expr(&mut p.expr, instance);
                }
                fill_expr(cycles, instance);
            }
            Terminator::Fatal(args) => {
                for a in &mut args.args {
                    fill_expr(&mut a.expr, instance);
                }
            }
            // Randomize carries no `TransactorState` placeholders, and
            // never appears in a responder body (transactor-method
            // randomize is out of subset) — nothing to fill.
            Terminator::Randomize { .. }
            | Terminator::Jump(_)
            | Terminator::WaitTimePs(_, _)
            | Terminator::Return => {}
        }
    }
    for block in &mut func.blocks {
        for s in &mut block.stmts {
            match s {
                ir::Stmt::TransactorStateWrite { instance: i, value, .. } => {
                    debug_assert!(
                        i.is_empty() || i == instance,
                        "target-state-write instance already filled with a different name"
                    );
                    *i = instance.to_string();
                    fill_expr(value, instance);
                }
                ir::Stmt::Assign(_, e) | ir::Stmt::DutWrite(_, e) => fill_expr(e, instance),
                ir::Stmt::RecordFieldWrite { value, .. } | ir::Stmt::TbFieldWrite { value, .. } => {
                    fill_expr(value, instance)
                }
                ir::Stmt::AssertCheck { cond, on_fail } => {
                    fill_expr(cond, instance);
                    for a in &mut on_fail.args {
                        fill_expr(&mut a.expr, instance);
                    }
                }
                ir::Stmt::Log { args, .. } | ir::Stmt::FailDiag { args, .. } => {
                    for a in &mut args.args {
                        fill_expr(&mut a.expr, instance);
                    }
                }
                ir::Stmt::TransactorCall { call, .. } => fill_expr(call, instance),
                ir::Stmt::ScoreboardOp { op, .. } => match op {
                    ir::ScoreboardOp::QueuePush { value, .. }
                    | ir::ScoreboardOp::ScalarWrite { value, .. } => fill_expr(value, instance),
                    ir::ScoreboardOp::QueuePop { .. } => {}
                },
                ir::Stmt::ComponentFieldWrite { value, .. } => fill_expr(value, instance),
                ir::Stmt::ComponentEmit { args, .. } => {
                    for a in args {
                        fill_expr(a, instance);
                    }
                }
                ir::Stmt::ComponentCall { args, .. } => {
                    for a in args {
                        fill_expr(a, instance);
                    }
                }
                ir::Stmt::SeqPush { value, .. } => fill_expr(value, instance),
                ir::Stmt::DutRead(_, _) | ir::Stmt::RecordInit(_, _) | ir::Stmt::CovReport(_) => {}
            }
        }
        fill_term(&mut block.terminator, instance);
    }
}

/// Fill the placeholder bus-binding prefix
/// (`transactors::INITIATOR_BUS_PLACEHOLDER`) in an initiator-side BFM
/// method body with the real bus binding name (the arch-com §19.6 flat
/// prefix). The body was lowered before the test's `let helper = bind
/// <binding>` named the binding, so every `bus.<ch>.<sig>` access carries
/// a `PortRef` whose first path segment is the placeholder.
///
/// Idempotent under the same binding (re-filling with the identical name
/// is a no-op). The method bodies are shared per transactor TYPE, so a
/// second bind to a DIFFERENT binding returns the previously filled name
/// (`Err`) — the one-instance-per-type subset gate, mirroring
/// `fill_transactor_state_instance`.
fn fill_initiator_bus_prefix(func: &mut TbFunction, binding: &str) -> Result<(), String> {
    use crate::ir::{Expr, PortRef, Stmt};
    let placeholder = transactors::INITIATOR_BUS_PLACEHOLDER;

    // Every PortRef carried by the body, whether at statement level
    // (`DutWrite`/`DutRead`), inside an expression (`Expr::Port` — bus
    // signal reads in wait predicates / assert conditions / format args
    // / DutWrite values), or in a terminator (a `Branch`/`WaitUntil`
    // condition reading a bus signal), prefixes its flat path with the
    // placeholder. A faithful fill must reach all of them — partial
    // coverage would leave a `bus_<ch>_<sig>` name with the wrong (still-
    // placeholder) prefix in the emitted C++.
    //
    // `visit` runs over both a check pass (detect a prior fill to a
    // DIFFERENT binding → the one-instance-per-type gate) and the rewrite
    // pass. It returns the first conflicting prefix it finds.
    fn visit_port(
        p: &mut PortRef,
        placeholder: &str,
        binding: &str,
        rewrite: bool,
        conflict: &mut Option<String>,
    ) {
        match p.port_path.first() {
            Some(seg) if seg == placeholder => {
                if rewrite {
                    p.port_path[0] = binding.to_string();
                }
            }
            // A non-placeholder, non-`binding` prefix means a prior bind
            // already rewrote this shared body to a different name.
            Some(seg) if seg != binding && conflict.is_none() => {
                *conflict = Some(seg.clone());
            }
            _ => {}
        }
    }
    fn visit_expr(
        e: &mut Expr,
        placeholder: &str,
        binding: &str,
        rewrite: bool,
        conflict: &mut Option<String>,
    ) {
        match e {
            Expr::Port(p) => visit_port(p, placeholder, binding, rewrite, conflict),
            Expr::Binary(_, a, b) => {
                visit_expr(a, placeholder, binding, rewrite, conflict);
                visit_expr(b, placeholder, binding, rewrite, conflict);
            }
            Expr::Unary(_, a)
            | Expr::WidthCast { inner: a, .. }
            | Expr::ComponentIdle { n: a, .. } => {
                visit_expr(a, placeholder, binding, rewrite, conflict)
            }
            Expr::Ternary(c, t, f) => {
                visit_expr(c, placeholder, binding, rewrite, conflict);
                visit_expr(t, placeholder, binding, rewrite, conflict);
                visit_expr(f, placeholder, binding, rewrite, conflict);
            }
            Expr::Call(_, args) => {
                for a in args {
                    visit_expr(a, placeholder, binding, rewrite, conflict);
                }
            }
            Expr::SeqIndex { index, .. } => {
                visit_expr(index, placeholder, binding, rewrite, conflict)
            }
            Expr::Literal { .. }
            | Expr::WideLiteral(_)
            | Expr::Local(_)
            | Expr::CycleCount
            | Expr::RecordField { .. }
            | Expr::TbField(_)
            | Expr::TransactorState { .. }
            | Expr::ComponentField { .. }
            | Expr::ScoreboardQuery { .. }
            | Expr::SeqLen(_)
            | Expr::CovBin { .. } => {}
        }
    }
    let mut run = |rewrite: bool| -> Option<String> {
        let mut conflict = None;
        for block in &mut func.blocks {
            for s in &mut block.stmts {
                match s {
                    Stmt::DutWrite(p, e) => {
                        visit_port(p, placeholder, binding, rewrite, &mut conflict);
                        visit_expr(e, placeholder, binding, rewrite, &mut conflict);
                    }
                    Stmt::DutRead(_, p) => {
                        visit_port(p, placeholder, binding, rewrite, &mut conflict)
                    }
                    Stmt::Assign(_, e)
                    | Stmt::RecordFieldWrite { value: e, .. }
                    | Stmt::TbFieldWrite { value: e, .. }
                    | Stmt::TransactorStateWrite { value: e, .. }
                    | Stmt::ComponentFieldWrite { value: e, .. } => {
                        visit_expr(e, placeholder, binding, rewrite, &mut conflict)
                    }
                    Stmt::AssertCheck { cond, on_fail } => {
                        visit_expr(cond, placeholder, binding, rewrite, &mut conflict);
                        for a in &mut on_fail.args {
                            visit_expr(&mut a.expr, placeholder, binding, rewrite, &mut conflict);
                        }
                    }
                    Stmt::Log { args, .. } | Stmt::FailDiag { args, .. } => {
                        for a in &mut args.args {
                            visit_expr(&mut a.expr, placeholder, binding, rewrite, &mut conflict);
                        }
                    }
                    Stmt::TransactorCall { call, .. } => {
                        visit_expr(call, placeholder, binding, rewrite, &mut conflict)
                    }
                    Stmt::ScoreboardOp { op, .. } => match op {
                        ir::ScoreboardOp::QueuePush { value, .. }
                        | ir::ScoreboardOp::ScalarWrite { value, .. } => {
                            visit_expr(value, placeholder, binding, rewrite, &mut conflict)
                        }
                        ir::ScoreboardOp::QueuePop { .. } => {}
                    },
                    Stmt::ComponentEmit { args, .. } | Stmt::ComponentCall { args, .. } => {
                        for a in args {
                            visit_expr(a, placeholder, binding, rewrite, &mut conflict);
                        }
                    }
                    Stmt::SeqPush { value, .. } => {
                        visit_expr(value, placeholder, binding, rewrite, &mut conflict)
                    }
                    Stmt::RecordInit(_, _) | Stmt::CovReport(_) => {}
                }
            }
            match &mut block.terminator {
                Terminator::Branch(c, _, _) => {
                    visit_expr(c, placeholder, binding, rewrite, &mut conflict)
                }
                Terminator::WaitCycles(n, _, _) | Terminator::WaitCyclesSync(n, _) => {
                    visit_expr(n, placeholder, binding, rewrite, &mut conflict)
                }
                Terminator::WaitUntil { preds, .. } => {
                    for p in preds {
                        visit_expr(&mut p.expr, placeholder, binding, rewrite, &mut conflict);
                    }
                }
                Terminator::WaitUntilTimeout { preds, cycles, .. } => {
                    for p in preds {
                        visit_expr(&mut p.expr, placeholder, binding, rewrite, &mut conflict);
                    }
                    visit_expr(cycles, placeholder, binding, rewrite, &mut conflict);
                }
                Terminator::Fatal(args) => {
                    for a in &mut args.args {
                        visit_expr(&mut a.expr, placeholder, binding, rewrite, &mut conflict);
                    }
                }
                Terminator::Randomize { .. }
                | Terminator::Jump(_)
                | Terminator::WaitTimePs(_, _)
                | Terminator::Return => {}
            }
        }
        conflict
    };
    // Check pass: a prior fill to a different binding is the multi-
    // instance gate. (Idempotent re-fill with the same binding is fine.)
    if let Some(prev) = run(false) {
        return Err(prev);
    }
    // Rewrite pass.
    run(true);
    Ok(())
}

#[cfg(test)]
mod time_literal_tests {
    use super::time_literal_to_ps;

    /// Largest value per unit whose picosecond conversion still fits in
    /// i64 (i64::MAX / factor), and the smallest rejected (one above).
    #[test]
    fn boundary_per_unit() {
        let cases = [
            ("ps", 9_223_372_036_854_775_807i64, 1i64),
            ("ns", 9_223_372_036_854_775, 1_000),
            ("us", 9_223_372_036_854, 1_000_000),
            ("ms", 9_223_372_036, 1_000_000_000),
            ("s", 9_223_372, 1_000_000_000_000),
        ];
        for (unit, max, factor) in cases {
            let ok = format!("{max}{unit}");
            assert_eq!(
                time_literal_to_ps(&ok),
                Ok(max.checked_mul(factor).unwrap()),
                "largest accepted value for {unit}"
            );
            // Smallest rejected: max+1 (for ps this overflows i64 itself,
            // so build the digit string from u64 instead).
            let over = format!("{}{unit}", (max as u64) + 1);
            let err = time_literal_to_ps(&over).expect_err("must reject");
            assert_eq!(
                err,
                format!("time literal `{over}` overflows the picosecond range (max {max}{unit})")
            );
        }
    }

    #[test]
    fn review_finding_repro_9300000s() {
        let err = time_literal_to_ps("9300000s").expect_err("must reject");
        assert_eq!(
            err,
            "time literal `9300000s` overflows the picosecond range (max 9223372s)"
        );
    }

    #[test]
    fn underscores_and_units_still_accepted() {
        assert_eq!(time_literal_to_ps("1_000ns"), Ok(1_000_000));
        assert_eq!(time_literal_to_ps("5ns"), Ok(5_000));
        assert!(time_literal_to_ps("5cycles").is_err());
    }
}
