//! Shared TBIR randomization emission contracts.

use crate::ast::*;
use crate::codegen::render_support::int_width_from_args;

#[derive(Debug, Clone)]
pub(crate) struct PerRunCell {
    pub tag: String,
    pub ctype: String,
    pub init: Option<String>,
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

pub(crate) use crate::codegen::native::{
    emit_record_randomize_helpers, plan_tbir_randomize_emission,
};

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
    use super::uses_constraint_solver;

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
}
