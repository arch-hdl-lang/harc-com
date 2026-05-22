//! Solver backend boundary for typed constraints.
//!
//! This module is intentionally non-invasive: current simulation still uses
//! the inline Z3 emission in `codegen/cpp_tb.rs`. Backends here consume
//! verified `CTypedProblem`s so codegen can migrate one feature at a time.

pub mod problem_table;
pub mod z3;

use std::collections::BTreeMap;

use crate::constraints::typed::FieldPath;
use crate::constraints::typed_verify::VerifyError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolverBuildError {
    Verify(Vec<VerifyError>),
    Unsupported {
        feature: &'static str,
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolverResult<M> {
    Sat(M),
    Unsat,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolvedTuple {
    pub fields: BTreeMap<FieldPath, FieldValue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    /// Little-endian 64-bit words. Width comes from the problem's `FieldEnv`.
    BvBits(Vec<u64>),
    Bool(bool),
    EnumIdx(u32),
}

pub trait SolverBackend {
    type Problem;
    type Model;

    fn build(
        &self,
        problem: &crate::constraints::typed::CTypedProblem,
    ) -> Result<Self::Problem, SolverBuildError>;

    fn check(&self, problem: &Self::Problem, seed: u64) -> SolverResult<Self::Model>;

    fn extract(&self, model: &Self::Model) -> SolvedTuple;
}
