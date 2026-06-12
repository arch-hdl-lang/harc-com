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

mod covergroup;
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

    // Transaction value-record structs, in declaration order. Mirrors
    // v1's `emit_record_struct` shape (field defaults as member
    // initializers, `operator==`/`!=`). v1's other record companions
    // — `randomize_<T>` and the pack/unpack helpers — are NOT emitted:
    // every construct that could reach them (`randomize`, bus sends)
    // is rejected at lowering, so they would be dead text here. They
    // land with their constructs.
    for r in &prog.records {
        record_struct(&mut out, r);
    }

    // Covergroup structs (leaf observables — they never name a TB or
    // DUT type), then one struct per unique non-synthetic testbench.
    for cg in &prog.covgroups {
        covergroup::covgroup_struct(&mut out, cg);
    }
    // Lowered pure helpers — file-scope C++ functions. Declaration
    // order is source order, which is not necessarily topological for
    // helper-to-helper calls, so prototypes go first.
    let helpers: Vec<&ir::TbFunction> = prog
        .functions
        .iter()
        .filter(|f| f.kind == ir::FunctionKind::Helper)
        .collect();
    for h in &helpers {
        func::emit_helper_prototype(&mut out, h);
    }
    if !helpers.is_empty() {
        writeln!(out).ok();
    }
    for h in &helpers {
        func::emit_helper_function(&mut out, h)?;
        writeln!(out).ok();
    }

    // One struct per unique non-synthetic testbench.
    let mut seen = HashSet::new();
    for tb in &prog.testbenches {
        if !tb.synthetic && seen.insert(tb.name.clone()) {
            let cov_fields: Vec<(String, String)> = tb
                .cov_fields
                .iter()
                .map(|(f, cg)| (f.clone(), prog.covgroups[cg.index()].name.clone()))
                .collect();
            runtime::tb_struct(&mut out, &tb.name, &dut_type, &cov_fields);
        }
    }
    runtime::context_struct(&mut out, &dut_type);

    for t in &prog.tests {
        emit_test(&mut out, prog, t, &dut_type)?;
    }

    runtime::dispatcher(&mut out, &test_names);
    Ok(out)
}

/// One transaction value-record struct. Field C types follow v1's
/// `txn_field_c_type` for the lowered (≤64-bit scalar) subset:
/// unsigned → `uint64_t`, signed → `int64_t`, bool/bit → `bool`.
fn record_struct(out: &mut String, r: &ir::RecordSchema) {
    writeln!(out, "struct {} {{", r.name).ok();
    for f in &r.fields {
        let (cty, init) = match f.ty {
            ir::IrType::Bool => (
                "bool",
                if f.default.is_some_and(|d| d != 0) { "true" } else { "false" }.to_string(),
            ),
            ir::IrType::SInt(_) => ("int64_t", f.default.unwrap_or(0).to_string()),
            _ => ("uint64_t", f.default.unwrap_or(0).to_string()),
        };
        writeln!(out, "{INDENT}{cty} {} = {init};", f.name).ok();
    }
    writeln!(out, "}};").ok();
    if r.fields.is_empty() {
        writeln!(
            out,
            "inline bool operator==(const {0}& a, const {0}& b) {{ (void)a; (void)b; return true; }}",
            r.name
        )
        .ok();
    } else {
        let eq = r
            .fields
            .iter()
            .map(|f| format!("a.{0} == b.{0}", f.name))
            .collect::<Vec<_>>()
            .join(" && ");
        writeln!(
            out,
            "inline bool operator==(const {0}& a, const {0}& b) {{ return {eq}; }}",
            r.name
        )
        .ok();
    }
    writeln!(
        out,
        "inline bool operator!=(const {0}& a, const {0}& b) {{ return !(a == b); }}",
        r.name
    )
    .ok();
    writeln!(out).ok();
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
    // Covergroup auto-sampler registration, in testbench-field
    // declaration order — the same `_checkers` slot v1 uses, so
    // sampling happens at the identical point in the cycle. Lowering
    // synthesized one SamplerAuto function per cov field; cross-check
    // the pairing so a lowering drift fails loudly here.
    let samplers: Vec<&ir::TbFunction> = prog
        .functions
        .iter()
        .filter(|f| {
            f.owner == Some(test.testbench)
                && matches!(f.kind, ir::FunctionKind::SamplerAuto { .. })
        })
        .collect();
    if samplers.len() != tb.cov_fields.len() {
        return Err(EmitError(format!(
            "tbir: test `{}` has {} cov field(s) but {} SamplerAuto function(s)",
            test.name,
            tb.cov_fields.len(),
            samplers.len()
        )));
    }
    for ((field, cg), sampler) in tb.cov_fields.iter().zip(&samplers) {
        let ir::FunctionKind::SamplerAuto { covgroup } = &sampler.kind else {
            unreachable!("filtered to SamplerAuto above");
        };
        if covgroup != cg {
            return Err(EmitError(format!(
                "tbir: sampler `{}` is bound to cg{} but field `{field}` expects cg{}",
                sampler.name, covgroup.0, cg.0
            )));
        }
        covergroup::sampler_registration(
            out,
            &prog.covgroups[cg.index()],
            &format!("_tb.{field}"),
        );
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
    func::emit_function(out, prog.function(test.run), &prog.records, 2)?;
    if let Some(check) = test.check {
        func::emit_function(out, prog.function(check), &prog.records, 2)?;
    }
    writeln!(out, "{INDENT}{INDENT}co_return;").ok();
    writeln!(out, "{INDENT}}}(&_run_slot);").ok();

    runtime::drive_loop(out, clocked);
    runtime::run_epilogue(out);
    Ok(())
}
