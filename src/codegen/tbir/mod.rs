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

use crate::ast::SourceFile;
use crate::codegen::cpp_tb::{EmitError, EmitOpts};
use crate::ir::{self, TbProgram};
use std::collections::HashSet;
use std::fmt::Write as _;

const INDENT: &str = "    ";

pub fn emit(prog: &TbProgram, file: &SourceFile, opts: &EmitOpts) -> Result<String, EmitError> {
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

    // Constraint-solver wiring (randomize sites). The runtime problem
    // table + per-site Z3-solve snippets are emitted by v1's shared
    // constraint codegen ("only the call site moves to the IR backend").
    // Empty when the program has no randomize site — the TB then never
    // links Z3, exactly like v1.
    let problem_table_cpp = if prog.constraint_sites.is_empty() {
        String::new()
    } else {
        let solver_table =
            crate::solver::problem_table::build_typed_solver_problem_table(file);
        let runtime_table =
            crate::solver::runtime::RuntimeProblemTable::from_typed_solver_table(&solver_table);
        if runtime_table.problems.is_empty() {
            String::new()
        } else {
            runtime_table.render_cpp_table("_harc_runtime_random_problem_table")
        }
    };
    // Per-`ConstraintRef` Z3-solve snippets, emitted at the loop-switch
    // body depth (run/check fn = depth 2 → block stmts at depth 5).
    let randomize_snippets =
        crate::codegen::cpp_tb::emit_randomize_snippets(file, opts, &prog.constraint_sites, 5)?;

    let mut out = String::new();
    runtime::preamble(&mut out, &dut_type, &test_names, &problem_table_cpp);

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
        runtime::component_struct(&mut out, &prog.components[ci], &prog.components, &prog.records);
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
        emit_test(&mut out, prog, t, &dut_type, opts, &randomize_snippets)?;
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

/// Register every `on <ev>(arg)` handler on `component` (and nested
/// sub-components) as a subscriber closure on the corresponding event
/// field of the instance reached by `inst_path`. The closure bumps the
/// owning instance's `_last_in_cycle` activity stamp, then runs the
/// handler body lambda — mirroring v1's `on`-subscriber registration.
fn emit_on_handler_regs(out: &mut String, prog: &TbProgram, component: ir::ComponentId, inst_path: &str) {
    let comp = &prog.components[component.index()];
    for oh in &comp.on_handlers {
        let lambda = func::on_handler_lambda_name(comp, oh);
        writeln!(
            out,
            "{INDENT}{inst_path}.{}.push_back([&](auto _t) {{ {inst_path}._last_in_cycle = (uint64_t)cycle_count; {lambda}({inst_path}, _t); }});",
            oh.event
        )
        .ok();
    }
    // Recurse into by-value sub-components (an env holding an agent).
    for f in &comp.fields {
        if let ir::ComponentFieldKind::Sub { component: sub } = &f.kind {
            let sub_path = format!("{inst_path}.{}", f.name);
            emit_on_handler_regs(out, prog, *sub, &sub_path);
        }
    }
}

/// Default watchdog clause values (spec §8.6), applied when the source
/// omits the `period`/`max_idle` clause. Mirror v1's
/// `WATCHDOG_DEFAULT_PERIOD` / `WATCHDOG_DEFAULT_MAX_IDLE`.
const WATCHDOG_DEFAULT_PERIOD: i64 = 1000;
const WATCHDOG_DEFAULT_MAX_IDLE: i64 = 10000;

/// Install the `_checkers` closures for a component's `on <N> cycles`
/// periodic handlers and its `watchdog` (and those of any nested
/// sub-component), each gated on a per-instance static last-fire stamp.
/// Mirrors v1's `emit_watchdog_checker` / periodic `_checkers` shape:
/// every cycle the closure re-reads the period (so a field-backed period
/// stays test-overridable), fires once it is due, and — for the watchdog
/// — runs the user body then the idle check.
fn emit_lifecycle_checkers(
    out: &mut String,
    prog: &TbProgram,
    component: ir::ComponentId,
    inst_path: &str,
) -> Result<(), EmitError> {
    let comp = &prog.components[component.index()];
    // A valid C++ identifier for the static tag (`env.agent` → `env_agent`).
    let inst_tag = inst_path.replace('.', "_");

    for ph in &comp.periodic_handlers {
        let lambda = func::periodic_handler_lambda_name(comp, ph);
        let period = func::clause_expr_cpp(prog, ph.function, inst_path, &ph.period)?;
        let tag = format!("_per_{inst_tag}_{}", ph.function.0);
        writeln!(out, "{INDENT}_checkers.push_back([&]() {{").ok();
        writeln!(out, "{INDENT}{INDENT}static int64_t {tag}_last = 0;").ok();
        writeln!(out, "{INDENT}{INDENT}int64_t {tag}_period = (int64_t)({period});").ok();
        writeln!(
            out,
            "{INDENT}{INDENT}if ({tag}_period > 0 && (int64_t)cycle_count - {tag}_last >= {tag}_period) {{"
        )
        .ok();
        writeln!(out, "{INDENT}{INDENT}{INDENT}{tag}_last = (int64_t)cycle_count;").ok();
        writeln!(out, "{INDENT}{INDENT}{INDENT}{lambda}({inst_path});").ok();
        writeln!(out, "{INDENT}{INDENT}}}").ok();
        writeln!(out, "{INDENT}}});").ok();
    }

    if let Some(w) = &comp.watchdog {
        let lambda = func::watchdog_lambda_name(comp, w);
        let period = match &w.period {
            Some(e) => func::clause_expr_cpp(prog, w.function, inst_path, e)?,
            None => WATCHDOG_DEFAULT_PERIOD.to_string(),
        };
        let max_idle = match &w.max_idle {
            Some(e) => func::clause_expr_cpp(prog, w.function, inst_path, e)?,
            None => WATCHDOG_DEFAULT_MAX_IDLE.to_string(),
        };
        let tag = format!("_wdog_{inst_tag}_{}", w.function.0);
        writeln!(out, "{INDENT}_checkers.push_back([&]() {{").ok();
        writeln!(out, "{INDENT}{INDENT}static int64_t {tag}_last = 0;").ok();
        writeln!(out, "{INDENT}{INDENT}int64_t {tag}_period = (int64_t)({period});").ok();
        writeln!(
            out,
            "{INDENT}{INDENT}if ({tag}_period > 0 && (int64_t)cycle_count - {tag}_last >= {tag}_period) {{"
        )
        .ok();
        writeln!(out, "{INDENT}{INDENT}{INDENT}{tag}_last = (int64_t)cycle_count;").ok();
        // 1. User body (typically a heartbeat log).
        writeln!(out, "{INDENT}{INDENT}{INDENT}{lambda}({inst_path});").ok();
        // 2. Idle check — trips FAIL when BOTH activity stamps are
        //    `max_idle` cycles behind. Mirrors v1's emit_watchdog idle
        //    block (errors++ on trip).
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}int64_t {tag}_max_idle = (int64_t)({max_idle});"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}if ({tag}_max_idle > 0 \
             && (int64_t)((uint64_t)cycle_count - {inst_path}._last_in_cycle) >= {tag}_max_idle \
             && (int64_t)((uint64_t)cycle_count - {inst_path}._last_out_cycle) >= {tag}_max_idle) {{"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}{INDENT}sim_log_line(\"FAIL\", \"watchdog: {} has been idle for >= %lld cycles\", (long long){tag}_max_idle);",
            comp.name
        )
        .ok();
        writeln!(out, "{INDENT}{INDENT}{INDENT}{INDENT}errors++;").ok();
        writeln!(out, "{INDENT}{INDENT}{INDENT}}}").ok();
        writeln!(out, "{INDENT}{INDENT}}}").ok();
        writeln!(out, "{INDENT}}});").ok();
    }

    // Recurse into by-value sub-components (an env holding an agent that
    // carries a watchdog / periodic handler).
    for f in &comp.fields {
        if let ir::ComponentFieldKind::Sub { component: sub } = &f.kind {
            let sub_path = format!("{inst_path}.{}", f.name);
            emit_lifecycle_checkers(out, prog, *sub, &sub_path)?;
        }
    }
    Ok(())
}

fn emit_test(
    out: &mut String,
    prog: &TbProgram,
    test: &ir::TestSchema,
    dut_type: &str,
    opts: &EmitOpts,
    randomize_snippets: &[String],
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
    // tseq generator lambdas — one `<Name>` per `tseq` declaration in the
    // file, declared before the run coroutine so its `[&]` capture sees
    // them (v1's `emit_tseq` placement). Each returns a
    // `std::vector<Record>` and runs the body's randomize/yield loop.
    for f in &prog.functions {
        if let ir::FunctionKind::Tseq { .. } = f.kind {
            func::emit_tseq(out, prog, f, &prog.records, randomize_snippets, 1)?;
        }
    }
    // Transactor method lambdas — one `<Type>_<method>` per method of
    // every transactor the testbench instantiates, declared before the
    // run coroutine so its `[&]` capture sees them (v1 emission order).
    // Two fields of the same transactor type share one lambda set: the
    // subset carries no per-instance state (the DUT bind is static).
    // Per-instance state structs for the unbound DUT-poking transactors
    // that carry persistent scalar state (`drv.last_read`). Declared
    // BEFORE the method lambdas so the lambdas' `[&]` capture binds the
    // instance struct the method bodies write (`drv.last_read = ...`) and
    // the run/check coroutine reads. Same per-instance struct shape as
    // the bound-to target form. The subset is one stateful instance per
    // transactor type (enforced at lowering), so the type-shared method
    // lambda references exactly this one instance.
    for (instance, xid) in &tb.unbound_state_actors {
        let schema = prog.transactor(*xid);
        runtime::target_state_struct_inst(out, schema, instance);
    }
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
        for oh in &comp.on_handlers {
            func::emit_component_on_handler(out, prog, comp, oh, 1)?;
        }
        for ph in &comp.periodic_handlers {
            func::emit_component_periodic_handler(out, prog, comp, ph, 1)?;
        }
        if let Some(w) = &comp.watchdog {
            func::emit_component_watchdog(out, prog, comp, w, 1)?;
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
        // `on <ev>(arg)` handler registrations, for this component and any
        // nested sub-components (an env holding an agent). Each subscribes
        // to the event field on its owning instance, bumps the instance's
        // `_last_in_cycle` activity stamp, then runs the handler body —
        // mirroring v1's `on`-subscriber registration.
        emit_on_handler_regs(out, prog, cf.component, &cf.field);
        // `on <N> cycles` periodic + `watchdog` lifecycle `_checkers`
        // closures, for this component and any nested sub-components.
        emit_lifecycle_checkers(out, prog, cf.component, &cf.field)?;
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
        randomize_snippets,
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
            randomize_snippets,
            2,
        )?;
    }
    writeln!(out, "{INDENT}{INDENT}co_return;").ok();
    writeln!(out, "{INDENT}}}(&_run_slot);").ok();

    runtime::drive_loop(out, clocked);
    runtime::run_epilogue(out);
    Ok(())
}
