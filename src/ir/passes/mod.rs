//! TB-IR passes (docs/tb-ir-design.md §"Pass interface contracts").
//!
//! Every pass lives in its own file `src/ir/passes/<name>.rs` with the
//! shape `pub fn run(prog: &TbProgram) -> PassOutput` (read-only) or
//! `pub fn run(prog: &mut TbProgram) -> PassOutput` (mutating). Passes
//! either annotate the IR, mutate it, or return a side-table; every
//! mutating pass must leave the program `verify`-clean.

pub mod lower_coroutine;
