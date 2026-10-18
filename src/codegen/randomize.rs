//! Shared TBIR randomization emission contracts.

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

/// Whether the source needs the native constraint-solver runtime.
pub fn uses_constraint_solver(file: &crate::ast::SourceFile) -> bool {
    crate::codegen::native::uses_constraint_solver(file)
}
