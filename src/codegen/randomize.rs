//! Shared TBIR randomization emission contracts.

use crate::ast::*;
use crate::codegen::emission::{EmitError, EmitOpts};
use crate::codegen::lifecycle_transform::{
    desugar_impl_for_test_in_file, desugar_impl_for_test_sharing_lifecycle,
};
use crate::codegen::randomize_renderer::TbirRandomizeEmitter;
use crate::codegen::render_support::int_width_from_args;
use crate::codegen::source_transform::normalize_vec_record_elem_fields;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PerRunCell {
    pub tag: String,
    pub ctype: String,
    pub init: Option<String>,
}

pub(crate) fn scalar_binding_sentinel(local: crate::ir::LocalId) -> String {
    format!("\u{1e}harc-scalar-{}\u{1f}", local.0)
}

pub(crate) const TBIR_RANDOMIZE_TARGET_SENTINEL: &str = "\u{1e}harc-target\u{1f}";
pub(crate) const TBIR_RANDOMIZE_CONTEXT_SENTINEL: &str = "\u{1e}harc-context\u{1f}";
pub(crate) const TBIR_RANDOMIZE_STATE_SENTINEL: &str = "\u{1e}harc-state\u{1f}";
pub(crate) const TBIR_RANDOMIZE_TSEQ_SCOPE_SENTINEL: &str = "\u{1e}harc-tseq-scope\u{1f}";
pub(crate) const TBIR_RANDOMIZE_SEQUENCER_SCOPE_SENTINEL: &str = "\u{1e}harc-sequencer-scope\u{1f}";

#[derive(Debug, Clone)]
pub(crate) struct TbirRandomizeEmissionPlan {
    pub runtime_table: crate::solver::runtime::RuntimeProblemTable,
    pub snippets: Vec<String>,
    pub site_states: Vec<TbirRandomizeSiteState>,
}

#[derive(Debug, Clone)]
pub(crate) struct TbirRandomizeSiteState {
    pub problem_id: Option<u64>,
    pub cells: Vec<PerRunCell>,
}

pub(crate) fn plan_tbir_randomize_emission(
    file: &SourceFile,
    opts: &EmitOpts,
    sites: &[crate::ir::ConstraintSite],
    depth: usize,
) -> Result<TbirRandomizeEmissionPlan, EmitError> {
    let typed_table = crate::solver::problem_table::build_typed_solver_problem_table(file);
    let mut runtime_table =
        crate::solver::runtime::RuntimeProblemTable::from_typed_solver_table(&typed_table);
    if sites.is_empty() {
        return Ok(TbirRandomizeEmissionPlan {
            runtime_table,
            snippets: Vec::new(),
            site_states: Vec::new(),
        });
    }

    let required_problem_ids = sites
        .iter()
        .filter_map(|site| site.problem_id)
        .collect::<std::collections::HashSet<_>>();
    let runtime_has_all_sites = |table: &crate::solver::runtime::RuntimeProblemTable| {
        required_problem_ids
            .iter()
            .all(|id| table.problems.iter().any(|problem| problem.id == *id))
    };
    let mut transformed_source = None;
    if !runtime_has_all_sites(&runtime_table) {
        let (shared, _) = desugar_impl_for_test_sharing_lifecycle(file);
        let shared_typed = crate::solver::problem_table::build_typed_solver_problem_table(&shared);
        let shared_runtime =
            crate::solver::runtime::RuntimeProblemTable::from_typed_solver_table(&shared_typed);
        if runtime_has_all_sites(&shared_runtime) {
            transformed_source = Some(shared);
            runtime_table = shared_runtime;
        } else {
            let inlined = desugar_impl_for_test_in_file(file);
            let inlined_typed =
                crate::solver::problem_table::build_typed_solver_problem_table(&inlined);
            let inlined_runtime =
                crate::solver::runtime::RuntimeProblemTable::from_typed_solver_table(
                    &inlined_typed,
                );
            if runtime_has_all_sites(&inlined_runtime) {
                transformed_source = Some(inlined);
                runtime_table = inlined_runtime;
            }
        }
    }
    let emission_source = transformed_source.as_deref().unwrap_or(file);

    let mut emitter = TbirRandomizeEmitter::new(emission_source, opts);
    let mut snippets = Vec::with_capacity(sites.len());
    let mut site_states = Vec::with_capacity(sites.len());
    for (index, site) in sites.iter().enumerate() {
        if let Some(problem_id) = site.problem_id {
            if !runtime_table
                .problems
                .iter()
                .any(|problem| problem.id == problem_id)
            {
                return Err(EmitError(format!(
                    "TB-IR randomize site c{index} references missing runtime problem {problem_id}"
                )));
            }
        }
        let (snippet, cells) = emitter.emit_site(site, depth)?;
        snippets.push(snippet);
        site_states.push(TbirRandomizeSiteState {
            problem_id: site.problem_id,
            cells,
        });
    }

    let mut all_cells = emitter.all_cells().to_vec();
    all_cells.sort_by(|lhs, rhs| lhs.tag.cmp(&rhs.tag));
    all_cells
        .dedup_by(|lhs, rhs| lhs.tag == rhs.tag && lhs.ctype == rhs.ctype && lhs.init == rhs.init);
    for pair in all_cells.windows(2) {
        if pair[0].tag == pair[1].tag {
            return Err(EmitError(format!(
                "TB-IR common randomize cell `{}` has conflicting declarations",
                pair[0].tag
            )));
        }
    }

    Ok(TbirRandomizeEmissionPlan {
        runtime_table,
        snippets,
        site_states,
    })
}

/// Emit unconstrained-randomize helpers in dependency-safe record order.
pub fn emit_record_randomize_helpers(
    file: &SourceFile,
    opts: &EmitOpts,
    records: &[crate::ir::RecordSchema],
    record_order: &[usize],
) -> Result<String, EmitError> {
    if record_order.is_empty() {
        return Ok(String::new());
    }
    let source_prog = crate::ir::lower::lower_program(file).map_err(|error| {
        EmitError(format!(
            "tbir record randomize helper source check: {error}"
        ))
    })?;
    if source_prog.records != records {
        return Err(EmitError(
            "tbir record randomize helper source/schema mismatch".to_string(),
        ));
    }
    let normalized = normalize_vec_record_elem_fields(file);
    let normalized = desugar_impl_for_test_in_file(&normalized);
    let mut emitter = TbirRandomizeEmitter::new(&normalized, opts);
    emitter.begin_record_helpers();
    for &record_index in record_order {
        let Some(record) = records.get(record_index) else {
            return Err(EmitError(format!(
                "tbir randomize helper record index {record_index} does not resolve"
            )));
        };
        let name = &record.name;
        let Some(item) = normalized.items.iter().find(|item| match item {
            Item::Struct(record) => record.name.name == *name,
            Item::Transaction(record) => record.name.name == *name,
            _ => false,
        }) else {
            return Err(EmitError(format!(
                "tbir randomize helper record `{name}` does not resolve"
            )));
        };
        emitter.emit_record_helper(item)?;
    }
    Ok(emitter.finish_record_helpers())
}

/// True if any code path will emit the Z3 solver block. Drives the
/// `harc_z3_rt.h` include + Z3 link flags. The check needs to mirror the
/// codegen's actual decision:
///   * `randomize(t) with <body>` — always solver (user wrote constraints)
///   * bare `randomize(t)` where `t`'s transaction has `keep` items,
///     `[unique]` fields, or auto coverage goals — also solver (keeps and
///     policy preferences are call-site constraints)
pub fn uses_constraint_solver(file: &SourceFile) -> bool {
    let enum_names: std::collections::HashSet<&str> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Enum(e) => Some(e.name.name.as_str()),
            _ => None,
        })
        .collect();
    // First pass: collect names of transactions whose bare `randomize(t)`
    // needs the solver path. False positives are cheap (an unused include);
    // false negatives are compile failures.
    let solver_bearing: std::collections::HashSet<&str> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Transaction(t) => {
                if txn_body_has_keep(&t.body) || txn_body_has_solver_policy(&t.body, &enum_names) {
                    Some(t.name.name.as_str())
                } else {
                    None
                }
            }
            Item::Struct(s) => {
                if txn_body_has_keep(&s.body) || fields_have_solver_policy(&s.fields, &enum_names) {
                    Some(s.name.name.as_str())
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect();

    fn block(b: &Block, solver_bearing: &std::collections::HashSet<&str>) -> bool {
        b.stmts.iter().any(|s| stmt(s, solver_bearing))
    }
    fn component_item(
        item: &ComponentItem,
        solver_bearing: &std::collections::HashSet<&str>,
    ) -> bool {
        match item {
            ComponentItem::OnHandler(h) => block(&h.body, solver_bearing),
            ComponentItem::Hookable(h) => block(&h.body, solver_bearing),
            ComponentItem::TargetTlmThread(t) => block(&t.body, solver_bearing),
            ComponentItem::Lifecycle(_, body) => block(body, solver_bearing),
            ComponentItem::Watchdog(w) => block(&w.body, solver_bearing),
            _ => false,
        }
    }
    fn component_items(
        items: &[ComponentItem],
        solver_bearing: &std::collections::HashSet<&str>,
    ) -> bool {
        items
            .iter()
            .any(|item| component_item(item, solver_bearing))
    }
    fn stmt(s: &Stmt, solver_bearing: &std::collections::HashSet<&str>) -> bool {
        match &s.kind {
            StmtKind::Randomize { with_body, .. } => {
                !with_body.is_empty() || !solver_bearing.is_empty()
            }
            StmtKind::For(f) => block(&f.body, solver_bearing),
            StmtKind::Repeat(r) => block(&r.body, solver_bearing),
            StmtKind::Loop(b) => block(b, solver_bearing),
            StmtKind::While { body, .. } => block(body, solver_bearing),
            StmtKind::If(i) => {
                block(&i.then_block, solver_bearing)
                    || i.else_block
                        .as_ref()
                        .map_or(false, |b| block(b, solver_bearing))
                    || i.elsifs.iter().any(|(_, b)| block(b, solver_bearing))
            }
            StmtKind::Fork(f) => f.branches.iter().any(|b| block(b, solver_bearing)),
            StmtKind::Parallel(bs) | StmtKind::Schedule(bs) => {
                bs.iter().any(|b| block(b, solver_bearing))
            }
            StmtKind::Select(arms) => arms.iter().any(|a| block(&a.action, solver_bearing)),
            StmtKind::On(h) => block(&h.body, solver_bearing),
            StmtKind::After { body, .. } => block(body, solver_bearing),
            _ => false,
        }
    }
    file.items.iter().any(|it| match it {
        Item::Function(f) => block(&f.body, &solver_bearing),
        Item::Tseq(t) => block(&t.body, &solver_bearing),
        Item::Agent(c) | Item::Env(c) | Item::Scoreboard(c) | Item::Sequencer(c) => {
            component_items(&c.items, &solver_bearing)
        }
        Item::Transactor(t) => {
            component_items(&t.items, &solver_bearing)
                || t.when_active
                    .as_ref()
                    .map_or(false, |items| component_items(items, &solver_bearing))
        }
        Item::Test(t) => t.items.iter().any(|ti| match ti {
            TestItem::Stmt(s) => stmt(s, &solver_bearing),
            TestItem::Phase(_, body) => block(body, &solver_bearing),
            TestItem::Scope(sc) => {
                sc.setup
                    .as_ref()
                    .map_or(false, |b| block(b, &solver_bearing))
                    || sc.run.as_ref().map_or(false, |b| block(b, &solver_bearing))
                    || sc
                        .check
                        .as_ref()
                        .map_or(false, |b| block(b, &solver_bearing))
                    || sc
                        .teardown
                        .as_ref()
                        .map_or(false, |b| block(b, &solver_bearing))
            }
            _ => false,
        }),
        _ => false,
    })
}

fn txn_body_has_keep(items: &[TxnBodyItem]) -> bool {
    items.iter().any(|item| match item {
        TxnBodyItem::Keep(_) => true,
        TxnBodyItem::When(w) => txn_body_has_keep(&w.items),
        TxnBodyItem::Field(_) => false,
    })
}

fn txn_body_has_solver_policy(
    items: &[TxnBodyItem],
    enum_names: &std::collections::HashSet<&str>,
) -> bool {
    items.iter().any(|item| match item {
        TxnBodyItem::Field(f) => txn_field_has_solver_policy(f, enum_names),
        TxnBodyItem::When(w) => txn_body_has_solver_policy(&w.items, enum_names),
        TxnBodyItem::Keep(_) => false,
    })
}

fn fields_have_solver_policy(
    fields: &[Field],
    enum_names: &std::collections::HashSet<&str>,
) -> bool {
    fields
        .iter()
        .any(|field| txn_field_has_solver_policy(field, enum_names))
}

fn txn_field_has_solver_policy(f: &Field, enum_names: &std::collections::HashSet<&str>) -> bool {
    if f.attrs
        .iter()
        .any(|a| a.name.name == "unique" || a.name.name == "range")
    {
        return true;
    }
    match &f.ty {
        TypeExpr::Builtin { name, args, .. } => match name {
            BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => true,
            BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits => {
                int_width_from_args(args).is_some_and(|w| w <= 1024)
            }
            BuiltinTy::SInt | BuiltinTy::SIntCap => {
                int_width_from_args(args).is_some_and(|w| w <= 1024)
            }
            _ => false,
        },
        TypeExpr::Named { name, .. } => name
            .segments
            .last()
            .is_some_and(|segment| enum_names.contains(segment.name.as_str())),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        emit_record_randomize_helpers, plan_tbir_randomize_emission, uses_constraint_solver,
    };

    fn parse(source: &str) -> crate::ast::SourceFile {
        crate::parser::parse_source_named("solver_usage_test.harc", source).unwrap()
    }

    #[test]
    fn solver_usage_walks_component_lifecycle_blocks() {
        let file = parse(
            r#"transaction RegOp
    value : uint<32>
end transaction RegOp

testbench Tb
    dut : Top
    setup
        let r : RegOp
        randomize(r) with r.value > 0 end randomize
    end setup
end testbench Tb
"#,
        );

        assert!(uses_constraint_solver(&file));
    }

    #[test]
    fn solver_usage_walks_target_tlm_threads() {
        let file = parse(
            r#"bus B
    tlm_method go(addr: uint<32>): blocking;
end bus B

transaction RegOp
    value : uint<32>
end transaction RegOp

transactor X bound to B
    thread bus.go(addr: uint<32>)
        let r : RegOp
        randomize(r) with r.value > 0 end randomize
    end thread
end transactor X
"#,
        );

        assert!(uses_constraint_solver(&file));
    }

    #[test]
    fn tbir_randomize_renderer_matches_the_retiring_native_renderer() {
        let file = parse(
            r#"transaction Packet
    opcode : uint<3> with [range(1, 6)]
    data : uint<16> with [unique within test]
    keep data > opcode
end transaction Packet

test T
    let dut : Top
    run
        let packet : Packet
        randomize(packet) with packet.data < 100 end randomize
    end run
end test T
"#,
        );
        let program = crate::ir::lower::lower_program(&file).expect("lower");
        let opts = crate::codegen::emission::EmitOpts::default();
        let plan = plan_tbir_randomize_emission(&file, &opts, &program.constraint_sites, 3)
            .expect("standalone TBIR renderer");

        let mut legacy =
            crate::codegen::native::randomize_renderer::NativeRandomizeEmitter::new(&file, &opts);
        legacy.configure_tbir_capsule();
        let mut legacy_snippets = Vec::new();
        let mut legacy_cells = Vec::new();
        for site in &program.constraint_sites {
            let (snippet, cells) = legacy.emit_site(site, 3, true).expect("native renderer");
            legacy_snippets.push(snippet);
            legacy_cells.push(cells);
        }
        assert_eq!(plan.snippets, legacy_snippets);
        assert_eq!(
            plan.site_states
                .iter()
                .map(|state| state.cells.clone())
                .collect::<Vec<_>>(),
            legacy_cells
        );

        let lowered_records = program.records.clone();
        let order = (0..lowered_records.len()).collect::<Vec<_>>();
        let standalone = emit_record_randomize_helpers(&file, &opts, &lowered_records, &order)
            .expect("standalone record helpers");
        let normalized = crate::codegen::source_transform::normalize_vec_record_elem_fields(&file);
        let normalized =
            crate::codegen::lifecycle_transform::desugar_impl_for_test_in_file(&normalized);
        let mut legacy = crate::codegen::native::randomize_renderer::NativeRandomizeEmitter::new(
            &normalized,
            &opts,
        );
        legacy.begin_record_helpers();
        for record in &lowered_records {
            let item = normalized
                .items
                .iter()
                .find(|item| match item {
                    crate::ast::Item::Struct(decl) => decl.name.name == record.name,
                    crate::ast::Item::Transaction(decl) => decl.name.name == record.name,
                    _ => false,
                })
                .expect("record item");
            legacy
                .emit_record_helper(item)
                .expect("native record helper");
        }
        assert_eq!(standalone, legacy.finish_record_helpers());
    }
}
