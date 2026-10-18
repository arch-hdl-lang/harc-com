//! Source normalization shared by lowering and native emitters.

use crate::ast::{Block, Expr, LifecyclePhase, SystemFn};
use crate::lexer::Span;

pub(crate) type SharedLifecycleBodies = std::collections::HashMap<(String, LifecyclePhase), Block>;

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub(crate) enum RecordLeafFate {
    Models,
    Flattens,
    Uncompilable,
}

pub(crate) struct Temporal {
    pub(crate) call_span: Span,
    pub(crate) kind: SystemFn,
    pub(crate) inner: Expr,
}

pub(crate) use crate::codegen::native::{
    collect_record_keeps, collect_temporal_occurrences, desugar_impl_for_test_in_file,
    desugar_impl_for_test_sharing_lifecycle, element_type_name, expr_source_str,
    fixed_vec_type_args, is_concurrent_assertion, process_interp, property_label, record_leaf_fate,
    rewrite_testbench_scope_body, string_value_literal_cpp, tb_lifecycle_marker_phase,
    verilated_handle_name,
};
