//! Transitional home for lifecycle and scope source normalization.
//!
//! The implementations still live in the native emitter, but isolating these
//! re-exports keeps `source_transform` backend-neutral and the dependency graph
//! one-way. Moving the implementations here is the next extraction batch.

pub(crate) use crate::codegen::native::{
    desugar_impl_for_test_in_file, desugar_impl_for_test_sharing_lifecycle,
    rewrite_testbench_scope_body,
};
