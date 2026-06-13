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
mod control;
mod covergroups;
mod exprs;
mod helpers;
mod records;
mod regblock;
mod scoreboards;
mod stmts;
mod transactors;

use crate::ast::{
    Block, BuiltinTy, BusDecl, ClockDecl, ComponentDecl, ComponentItem, ExprKind,
    HookableMethod, Item, ScopeDecl, SourceFile, Stmt as AstStmt, StmtKind, TestDecl, TestItem,
    TransactorMode, TypeExpr,
};
use crate::ir::{
    self, BasicBlock, BlockId, ClockSpec, CovgroupId, CovgroupSchema, FunctionId, FunctionKind,
    IrType, LocalId, RecordId, RecordSchema, RegblockId, ScoreboardId, ScoreboardSchema,
    TbFunction, TbProgram, TestSchema, TestbenchId, TestbenchSchema, Terminator, TransactorId,
    TransactorSchema, TypedLocal, TypedParam,
};
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
                    "regblock `{name}` collides with a transaction of the same name"
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
            Item::Env(c) => {
                if used_tbs.contains(&c.name.name) {
                    validate_testbench_component(
                        c,
                        &components,
                        &covgroup_ids,
                        &record_ids,
                        &transactor_ids,
                        &scoreboard_ids,
                    )?;
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

    // Eagerly lower pure helpers (declaration order) so call sites can
    // stay `Expr::Call` and backends emit them as plain C++ functions.
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
        let f = helpers::lower_pure_helper(id, fd, &helper_registry, &helper_ctx)?;
        prog.functions.push(f);
    }

    // Transactor declarations, in file order: one schema each plus one
    // `TbFunction` (kind `TransactorBody`) per method. All declarations
    // lower (even unreferenced ones), so unsupported transactor shapes
    // are rejected here rather than dropped.
    for it in &file.items {
        let Item::Transactor(t) = it else { continue };
        let id = TransactorId(prog.transactors.len() as u32);
        debug_assert_eq!(Some(&id), transactor_ids.get(&t.name.name));
        let (schema, funcs) = transactors::lower_transactor(
            id,
            t,
            FunctionId(prog.functions.len() as u32),
            &helper_registry,
            &helper_ctx,
        )?;
        prog.transactors.push(schema);
        prog.functions.extend(funcs);
    }

    for it in &file.items {
        let Item::Test(t) = it else { continue };
        lower_test(
            t,
            &tb_of_test,
            &components,
            &domains,
            &covgroup_ids,
            &record_ids,
            &regblock_ids,
            &buses,
            &consts,
            &helper_registry,
            &mut prog,
        )?;
    }

    if prog.tests.is_empty() {
        return Err(LowerError::Invalid(
            "no `test` declaration found".to_string(),
        ));
    }
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
) -> Result<(), LowerError> {
    for ci in &c.items {
        match ci {
            ComponentItem::Field(f) => {
                if let TypeExpr::Named { name, mode, .. } = &f.ty {
                    let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                    if covgroup_ids.contains_key(simple) {
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
                    if components.contains_key(simple) {
                        return Err(unsupported(
                            &format!(
                                "testbench field `{}` of component type `{}`",
                                f.name.name, simple
                            ),
                            "",
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
    domains: &HashMap<String, i64>,
    covgroup_ids: &HashMap<String, CovgroupId>,
    record_ids: &HashMap<String, RecordId>,
    regblock_ids: &HashMap<String, RegblockId>,
    buses: &HashMap<String, &BusDecl>,
    consts: &HashMap<String, u64>,
    helpers: &helpers::HelperRegistry<'_>,
    prog: &mut TbProgram,
) -> Result<(), LowerError> {
    if !t.params.is_empty() {
        return Err(unsupported("test parameters", ""));
    }

    let mut dut_type: Option<String> = None;
    let mut clocks: Vec<&ClockDecl> = Vec::new();
    let mut scope: Option<&ScopeDecl> = None;
    let mut bare_stmts: Vec<&AstStmt> = Vec::new();
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
            TestItem::Phase(name, _) => {
                return Err(unsupported(&format!("custom phase `{}`", name.name), ""));
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
    let bare_transactor_fields: HashSet<String> =
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
fn tb_scalar_field_ir_type(t: &TypeExpr) -> Option<IrType> {
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
) -> Result<TbFunction, LowerError> {
    let mut b = FuncBuilder::new(ctx, helpers);
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
    pub(crate) fn new(ctx: &'a LowerCtx, helpers: &'a helpers::HelperRegistry<'a>) -> Self {
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
            in_check: false,
            let_widths: HashMap::new(),
        }
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
        Terminator::Return | Terminator::Fatal(_) => {}
    }
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
