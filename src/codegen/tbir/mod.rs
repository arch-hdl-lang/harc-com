//! TB-IR → C++ testbench backend (`--codegen tbir`).
//!
//! Consumes a verified `ir::TbProgram` and emits a Verilator-class C++
//! TB whose scaffolding mirrors the live v1 (`cpp_tb`) contract —
//! preamble, `HarcTestContext`, clock scheduler, `sim_log_line`
//! plumbing, trace events, exit status, and the `--test`/`HARC_TEST`
//! dispatcher `main()` — so a tbir binary is a drop-in replacement on
//! both the `--sv` (Verilator) and `--dut` (arch sim) paths and its
//! semantic trace diffs clean against v1.
//!
//! Function bodies use a loop-switch over `BlockId` instead of
//! re-structured control flow; see `func.rs`.

mod expr;
mod func;
mod runtime;

use crate::codegen::cpp_tb::{EmitError, EmitOpts};
use crate::ir::{self, TbProgram};
use std::collections::HashSet;
use std::fmt::Write as _;

const INDENT: &str = "    ";

pub fn emit(prog: &TbProgram, opts: &EmitOpts) -> Result<String, EmitError> {
    if opts.mt {
        return Err(EmitError(
            "--mt is not supported with --codegen tbir; re-run with --codegen v1".to_string(),
        ));
    }
    if prog.tests.is_empty() {
        return Err(EmitError("no `test` declaration found".to_string()));
    }

    // All tests in one binary share the DUT type (same v0 rule as v1).
    let dut_type = prog.testbench(prog.tests[0].testbench).dut_type.clone();
    for t in &prog.tests {
        let tb = prog.testbench(t.testbench);
        if tb.dut_type != dut_type {
            return Err(EmitError(format!(
                "multi-DUT tests in one binary are out of scope for v0; \
                 test `{}` uses `{}`, but a previous test used `{}`",
                t.name, tb.dut_type, dut_type,
            )));
        }
    }

    let test_names: Vec<String> = prog.tests.iter().map(|t| t.name.clone()).collect();
    let mut out = String::new();
    runtime::preamble(&mut out, &dut_type, &test_names);

    // One struct per unique non-synthetic testbench.
    let mut seen = HashSet::new();
    for tb in &prog.testbenches {
        if !tb.synthetic && seen.insert(tb.name.clone()) {
            runtime::tb_struct(&mut out, &tb.name, &dut_type);
        }
    }
    runtime::context_struct(&mut out, &dut_type);

    for t in &prog.tests {
        emit_test(&mut out, prog, t, &dut_type)?;
    }

    runtime::dispatcher(&mut out, &test_names);
    Ok(out)
}

fn emit_test(
    out: &mut String,
    prog: &TbProgram,
    test: &ir::TestSchema,
    dut_type: &str,
) -> Result<(), EmitError> {
    let tb = prog.testbench(test.testbench);
    let clocked = !test.clocks.is_empty();

    runtime::run_prologue(out, &test.name, dut_type);
    if clocked {
        runtime::clocked_scheduler(out, &test.clocks);
    } else {
        runtime::clockless_scheduler(out);
    }
    runtime::log_helpers_and_seed(out);

    if !tb.synthetic {
        writeln!(out, "{INDENT}{} _tb;", tb.name).ok();
    }
    writeln!(out, "{INDENT}harc_rt::ThreadSlot _run_slot;").ok();
    writeln!(out, "{INDENT}sched.slots.push_back(&_run_slot);").ok();
    writeln!(
        out,
        "{INDENT}_run_slot.thread = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{"
    )
    .ok();
    if !tb.synthetic {
        writeln!(out, "{INDENT}{INDENT}_tb.dut = dut;").ok();
    }
    func::emit_function(out, prog.function(test.run), 2)?;
    if let Some(check) = test.check {
        func::emit_function(out, prog.function(check), 2)?;
    }
    writeln!(out, "{INDENT}{INDENT}co_return;").ok();
    writeln!(out, "{INDENT}}}(&_run_slot);").ok();

    runtime::drive_loop(out, clocked);
    runtime::run_epilogue(out);
    Ok(())
}
