//! Backends. Today: only `cpp_tb` (Verilator-class C++ TB harness emitter).
//!
//! Future per spec §10: `sv_uvm` (transpile to SV+UVM, phase 5),
//! `formal` (BTOR2 / SMT-LIB2 export, phase 4), and a real Phase 1a native
//! runtime that lowers `tseq` to coroutines instead of straight-line C++.

pub mod cpp_tb;
pub mod merge;
