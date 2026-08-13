pub mod ast;
pub mod check_backends;
pub mod codegen;
pub mod constraints;
pub mod diagnostics;
pub mod graph;
pub mod ir;
pub mod learn;
pub mod lexer;
pub mod parser;
pub mod pretty;
pub mod solver;

/// Maximum destination width accepted by the bit-vector width-method
/// intrinsics (`trunc`, `zext`, `sext`, and `resize`).
pub const MAX_WIDTH_METHOD_BITS: u32 = 1024;
