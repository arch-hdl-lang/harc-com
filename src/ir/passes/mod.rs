//! TB-IR passes (docs/tb-ir-design.md §"Pass interface contracts").
//!
//! Every pass lives in its own file `src/ir/passes/<name>.rs` with the
//! shape `pub fn run(prog: &TbProgram) -> PassOutput` (read-only) or
//! `pub fn run(prog: &mut TbProgram) -> PassOutput` (mutating). Passes
//! either annotate the IR, mutate it, or return a side-table; every
//! mutating pass must leave the program `verify`-clean.

pub mod bus_access;
pub mod callable_placement;
pub mod covergroup_hooks;
pub mod dut_access;
pub mod lower_coroutine;
pub mod placement;
pub mod runtime_cells;
