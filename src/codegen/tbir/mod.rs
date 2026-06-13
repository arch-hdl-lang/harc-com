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

    // Scoreboard structs (data-only host-state records — they never name
    // a TB or DUT type), before the testbench structs that hold them.
    for sb in &prog.scoreboards {
        runtime::scoreboard_struct(&mut out, sb);
    }

    // Composite-component structs (env/agent cluster). A component holds
    // its sub-components by value, so each held type's struct must be
    // defined first. Source order usually puts subs before the env, but
    // a user may declare the env first — so emit in dependency order
    // (DFS over `Sub` fields), mirroring v1's `topo_sort_component_indices`.
    for ci in component_emit_order(prog) {
        runtime::component_struct(&mut out, &prog.components[ci], &prog.components);
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
            let sb_fields: Vec<(String, String)> = tb
                .scoreboard_fields
                .iter()
                .map(|(f, sb)| (f.clone(), prog.scoreboards[sb.index()].name.clone()))
                .collect();
            runtime::tb_struct(
                &mut out,
                &tb.name,
                &dut_type,
                &cov_fields,
                &tb.scalar_fields,
                &sb_fields,
            );
        }
    }
    runtime::context_struct(&mut out, &dut_type);

    for t in &prog.tests {
        emit_test(&mut out, prog, t, &dut_type, opts)?;
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

/// Dependency order for component-struct emission: a component appears
/// after every component it holds as a by-value `Sub` field, so the held
/// struct is already defined. DFS post-order over the `Sub` edges, in
/// id order for determinism (mirrors v1's `topo_sort_component_indices`).
/// The IR rejects sub-component cycles at lowering (a by-value cycle is
/// not constructible), so the visited-set DFS terminates.
fn component_emit_order(prog: &TbProgram) -> Vec<usize> {
    let n = prog.components.len();
    let mut order = Vec::with_capacity(n);
    let mut visited = vec![false; n];
    fn visit(
        i: usize,
        prog: &TbProgram,
        visited: &mut [bool],
        order: &mut Vec<usize>,
    ) {
        if visited[i] {
            return;
        }
        visited[i] = true;
        for f in &prog.components[i].fields {
            if let ir::ComponentFieldKind::Sub { component } = &f.kind {
                visit(component.index(), prog, visited, order);
            }
        }
        order.push(i);
    }
    for i in 0..n {
        visit(i, prog, &mut visited, &mut order);
    }
    order
}

fn emit_test(
    out: &mut String,
    prog: &TbProgram,
    test: &ir::TestSchema,
    dut_type: &str,
    opts: &EmitOpts,
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
    // Transactor method lambdas — one `<Type>_<method>` per method of
    // every transactor the testbench instantiates, declared before the
    // run coroutine so its `[&]` capture sees them (v1 emission order).
    // Two fields of the same transactor type share one lambda set: the
    // subset carries no per-instance state (the DUT bind is static).
    let mut emitted_xactors = HashSet::new();
    for (_, xid) in &tb.transactor_fields {
        if !emitted_xactors.insert(*xid) {
            continue;
        }
        let schema = prog.transactor(*xid);
        for m in &schema.methods {
            func::emit_method(out, prog, schema, m, 1)?;
        }
    }
    // Bound-to target-side TLM responder instances: one per-instance
    // state struct (state fields + activity stamps), declared in the
    // test scope so both the run/check coroutine (`target.read_count`)
    // and the actor coroutines capture it by reference, then one
    // background-coroutine actor per target method.
    for actor in &tb.target_tlm_actors {
        let schema = prog.transactor(actor.transactor);
        runtime::target_state_struct_inst(out, schema, &actor.instance);
    }
    for actor in &tb.target_tlm_actors {
        func::emit_target_actor(out, prog, actor, 1)?;
    }

    // Composite-component method lambdas — one `<Comp>_<method>` per
    // method of every component in the file, declared before the run
    // coroutine so its `[&]` capture (and the connect push_backs below)
    // see them. Dependency order (subs before holders) so a method body
    // that calls a sub-component's method sees that lambda first.
    for ci in component_emit_order(prog) {
        let comp = &prog.components[ci];
        for m in &comp.methods {
            func::emit_component_method(out, prog, comp, m, 1)?;
        }
    }
    // Composite-component test-scope instances (`let env : AnalysisEnv`):
    // a default-constructed run-scope local, then its env `connect`
    // push_backs (`<env>.<src_path>.<event>.push_back([&](auto _t){
    // <SinkComp>_<method>(<env>.<sink_path>, _t); })`). Declared at test
    // scope (before the coroutine) so run and check share the instance —
    // v1's `AnalysisEnv env;` placement.
    for cf in &tb.component_fields {
        let cname = &prog.components[cf.component.index()].name;
        writeln!(out, "{INDENT}{cname} {};", cf.field).ok();
        for edge in &cf.connects {
            let src = std::iter::once(cf.field.clone())
                .chain(edge.src_path.iter().cloned())
                .collect::<Vec<_>>()
                .join(".");
            let sink = std::iter::once(cf.field.clone())
                .chain(edge.sink_path.iter().cloned())
                .collect::<Vec<_>>()
                .join(".");
            let sink_comp = &prog.components[edge.sink_component.index()].name;
            writeln!(
                out,
                "{INDENT}{src}.{}.push_back([&](auto _t) {{ {sink_comp}_{}({sink}, _t); }});",
                edge.src_event, edge.sink_method
            )
            .ok();
        }
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
    func::emit_function(
        out,
        prog,
        prog.function(test.run),
        &prog.records,
        &tb.bus_bindings,
        &opts.vec_lane_widths,
        2,
    )?;
    if let Some(check) = test.check {
        func::emit_function(
            out,
            prog,
            prog.function(check),
            &prog.records,
            &tb.bus_bindings,
            &opts.vec_lane_widths,
            2,
        )?;
    }
    writeln!(out, "{INDENT}{INDENT}co_return;").ok();
    writeln!(out, "{INDENT}}}(&_run_slot);").ok();

    runtime::drive_loop(out, clocked);
    runtime::run_epilogue(out);
    Ok(())
}
