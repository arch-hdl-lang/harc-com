//! Loop-switch emission of one `TbFunction` body, inside the v1
//! coroutine shape.
//!
//! No relooper: each function emits as
//!
//! ```cpp
//! {
//!     uint64_t <local> = 0; ...      // ALL locals hoisted
//!     int __bb = 0;
//!     bool __done = false;
//!     while (!__done) {
//!         switch (__bb) {
//!         case N: { ...stmts...; <terminator>; break; }
//!         }
//!     }
//! }
//! ```
//!
//! `co_await` inside a `switch` is legal C++20, so `WaitCycles` lowers
//! to the same `co_await harc_rt::wait_cycles(_slot, ...)` suspension
//! v1 uses — identical scheduler interaction, identical cycle timing.

use super::expr::{
    bounded_count_expr_cpp, comp_base_cpp_subst_cx, escape_c, expr_cpp, expr_is_signed,
    expr_static_width, fmt_arg_cpp, helper_cpp_name, lane_index_cpp, lane_width, local_cpp_name,
    local_ir_type, owner_tb, port_read, port_signal, required_testbench_receiver,
    scalar_assignment_expr_cpp, truthy_expr_cpp, wide_words_over_128, BusAdapterRenderBindings,
    CallableRenderBindings, ECx, RuntimeCellRenderBinding, TestbenchBusAdapterPlan,
};
use crate::codegen::cpp_tb::{
    EmitError, TBIR_RANDOMIZE_CONTEXT_SENTINEL, TBIR_RANDOMIZE_SEQUENCER_SCOPE_SENTINEL,
    TBIR_RANDOMIZE_STATE_SENTINEL, TBIR_RANDOMIZE_TARGET_SENTINEL,
    TBIR_RANDOMIZE_TSEQ_SCOPE_SENTINEL,
};
use crate::ir::{
    BlockId, BusBindingSchema, CallTarget, ConstraintRef, Expr, FileLogLevel, FmtArgs, FunctionId,
    FunctionKind, IrType, LocalId, LogLevel, PredSrc, RecordSchema, Stmt, TbFunction, TbProgram,
    Terminator, TransactorMethodSchema, TransactorSchema, WaitMode,
};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

const INDENT: &str = "    ";

/// Names that the surrounding scaffolding owns; user locals colliding
/// with them get a `_u_` prefix so the loop-switch body cannot shadow
/// captured references (`errors`, `dut`, ...).
const RESERVED: &[&str] = &[
    "dut",
    "tfp",
    "ctx",
    "self",
    "errors",
    "cycle_count",
    "trace",
    "log_ctx",
    "tick",
    "sched",
    "argc",
    "argv",
    "now_ps",
    "clocks_",
    "target",
    "harc_rng",
    "test_sel",
    "sim_log_line",
    "sim_logf_line",
    "eval_clocks_until",
    "_tb",
    "_fatal",
    "_trace_time",
    "_wave_path",
    "_checkers",
    "_post_eval_services",
    "_auto_cov_reports",
    "runtime_cells",
    "_harc_runtime_cells",
    "_harc_run_state",
    "_harc_opaque_state",
    "_run_slot",
    "_slot",
    "_harc_trace_dump_next",
    "_harc_trace_dump_at",
    "_harc_unique_tseq",
    "__bb",
    "__done",
    "_wu_budget",
    // The synchronous timed-wait poll loop's start stamp. Without this,
    // a user local named `_wu_start` in a method body shadows the
    // snapshot and the elapsed-cycle bound compares the wrong value.
    "_wu_start",
    "_wu_satisfied",
    // C++ keywords that are plausible HARC identifiers.
    "auto",
    "bool",
    "break",
    "case",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "delete",
    "do",
    "double",
    "else",
    "enum",
    "false",
    "float",
    "for",
    "if",
    "int",
    "long",
    "namespace",
    "new",
    "operator",
    "private",
    "protected",
    "public",
    "register",
    "return",
    "short",
    "signed",
    "static",
    "struct",
    "switch",
    "template",
    "this",
    "true",
    "union",
    "unsigned",
    "using",
    "virtual",
    "void",
    "while",
];

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct FlowRenderBindings<'a> {
    pub run_context: Option<&'a str>,
    pub dut_receiver: Option<&'a str>,
    pub dut_access: Option<&'a crate::ir::passes::dut_access::DutAccessPlan>,
    pub dut_lane_widths: Option<&'a HashMap<String, u32>>,
    pub testbench_receiver: Option<&'a str>,
    pub testbench_components: Option<&'a [super::expr::TestbenchComponentRenderBinding]>,
    pub testbench_transactor_states:
        Option<&'a [super::expr::TestbenchTransactorStateRenderBinding]>,
    pub bus_adapters: Option<BusAdapterRenderBindings<'a>>,
    pub clocks: Option<&'a [crate::ir::ClockSpec]>,
    pub reserved: &'a [&'a str],
    pub durable_callbacks: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct TestHookRenderBindings<'a> {
    pub flow: FlowRenderBindings<'a>,
    pub runtime_cells: Option<RuntimeCellRenderBinding<'a>>,
    pub common_contextual_tseqs: Option<&'a BTreeSet<FunctionId>>,
    pub durable_capture: bool,
}

/// Per-local emitted C++ names. Lowering already deduplicated names
/// within the function; this only steps around scaffolding collisions.
fn cpp_local_names(func: &TbFunction) -> Vec<String> {
    cpp_local_names_with_reserved(func, std::iter::empty())
}

fn cpp_local_names_with_reserved<'a>(
    func: &TbFunction,
    additional: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let mut used = RESERVED
        .iter()
        .map(|name| (*name).to_string())
        .collect::<HashSet<_>>();
    used.extend(additional.into_iter().map(str::to_string));
    func.locals
        .iter()
        .map(|local| {
            let mut name = local.name.clone();
            while used.contains(&name) {
                name = format!("_u_{name}");
            }
            used.insert(name.clone());
            name
        })
        .collect()
}

fn randomize_snippet_for(
    prog: &TbProgram,
    func: &TbFunction,
    names: &[String],
    target: LocalId,
    constraints: ConstraintRef,
    snippets: &[String],
    run_context: &str,
    state_receiver: &str,
    self_receiver: Option<&str>,
) -> Result<Option<String>, EmitError> {
    let Some(snippet) = snippets.get(constraints.index()) else {
        return Ok(None);
    };
    let Some(site) = prog.constraint_sites.get(constraints.index()) else {
        return Ok(None);
    };
    let Some(dst) = names.get(target.index()) else {
        return Ok(None);
    };
    let uses_tseq_unique = snippet.contains(TBIR_RANDOMIZE_TSEQ_SCOPE_SENTINEL);
    if uses_tseq_unique && !matches!(&func.kind, FunctionKind::Tseq { .. }) {
        return Err(EmitError(format!(
            "randomize({}): `[unique within tseq]` is only valid inside a tseq",
            site.record
        )));
    }
    let tseq_registry = "_harc_unique_tseq";
    let uses_sequencer_unique = snippet.contains(TBIR_RANDOMIZE_SEQUENCER_SCOPE_SENTINEL);
    if uses_sequencer_unique
        && !matches!(
            &func.kind,
            FunctionKind::ComponentMethod { component, .. }
                if prog.components.get(component.index()).is_some_and(|schema| {
                    schema.kind == crate::ir::ComponentKindTag::Sequencer
                })
        )
    {
        return Err(EmitError(format!(
            "randomize({}): `[unique within sequencer]` is only valid inside a sequencer",
            site.record
        )));
    }
    let sequencer_registry = match &func.kind {
        FunctionKind::ComponentMethod { component, .. }
            if prog
                .components
                .get(component.index())
                .is_some_and(|schema| schema.kind == crate::ir::ComponentKindTag::Sequencer) =>
        {
            let schema = &prog.components[component.index()];
            let member = crate::codegen::cpp_tb::component_unique_registry_name(
                schema.fields.iter().map(|field| field.name.as_str()),
            );
            self_receiver
                .map(|receiver| format!("{receiver}.{member}"))
                .ok_or_else(|| {
                    EmitError(format!(
                        "tbir: sequencer method {} has no receiver for unique history",
                        func.name
                    ))
                })?
        }
        _ => String::new(),
    };
    Ok(Some(
        snippet
            .replace(TBIR_RANDOMIZE_TARGET_SENTINEL, dst)
            .replace(TBIR_RANDOMIZE_CONTEXT_SENTINEL, run_context)
            .replace(TBIR_RANDOMIZE_STATE_SENTINEL, state_receiver)
            .replace(TBIR_RANDOMIZE_TSEQ_SCOPE_SENTINEL, tseq_registry)
            .replace(TBIR_RANDOMIZE_SEQUENCER_SCOPE_SENTINEL, &sequencer_registry),
    ))
}

fn randomize_state_receiver(
    func: &TbFunction,
    constraints: ConstraintRef,
    runtime_cells: Option<RuntimeCellRenderBinding<'_>>,
    run_context: &str,
) -> Result<String, EmitError> {
    if matches!(
        func.kind,
        FunctionKind::TestBody { .. } | FunctionKind::TestHook { .. }
    ) {
        if let Some(runtime_cells) = runtime_cells {
            return runtime_cells.constraint_field(constraints);
        }
    }
    Ok(format!("{run_context}._harc_randomize_c{}", constraints.0))
}

/// Emit one function as a loop-switch at `depth` indentation levels,
/// wrapped in its own brace scope so multiple functions (run + check)
/// can share one coroutine body without name collisions. `bindings`
/// is the owning testbench's bus-binding table — the metadata that
/// expands `CallTarget::TransactorMethod` call edges into the
/// canonical req/rsp wire protocol.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_function(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
    records: &[RecordSchema],
    bindings: &[BusBindingSchema],
    lanes: &HashMap<String, u32>,
    randomize_snippets: &[String],
    dut_type: &str,
    predeclared: &HashSet<LocalId>,
    depth: usize,
    runtime_cells: Option<RuntimeCellRenderBinding<'_>>,
    flow_bindings: FlowRenderBindings<'_>,
    // Native lifecycle functions emitted out-of-line for this layout.
    outofline_lifecycle: &HashMap<crate::ir::FunctionId, LifecycleEmit>,
) -> Result<(), EmitError> {
    emit_function_with_tseq_calls(
        out,
        prog,
        func,
        records,
        bindings,
        lanes,
        randomize_snippets,
        dut_type,
        predeclared,
        depth,
        None,
        runtime_cells,
        flow_bindings,
        outofline_lifecycle,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_common_function(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
    records: &[RecordSchema],
    bindings: &[BusBindingSchema],
    lanes: &HashMap<String, u32>,
    randomize_snippets: &[String],
    dut_type: &str,
    predeclared: &HashSet<LocalId>,
    depth: usize,
    contextual_tseqs: &std::collections::BTreeSet<FunctionId>,
    runtime_cells: Option<RuntimeCellRenderBinding<'_>>,
    flow_bindings: FlowRenderBindings<'_>,
    outofline_lifecycle: &HashMap<crate::ir::FunctionId, LifecycleEmit>,
) -> Result<(), EmitError> {
    emit_function_with_tseq_calls(
        out,
        prog,
        func,
        records,
        bindings,
        lanes,
        randomize_snippets,
        dut_type,
        predeclared,
        depth,
        Some(contextual_tseqs),
        runtime_cells,
        flow_bindings,
        outofline_lifecycle,
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_function_with_tseq_calls(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
    records: &[RecordSchema],
    bindings: &[BusBindingSchema],
    lanes: &HashMap<String, u32>,
    randomize_snippets: &[String],
    dut_type: &str,
    predeclared: &HashSet<LocalId>,
    depth: usize,
    common_contextual_tseqs: Option<&std::collections::BTreeSet<FunctionId>>,
    runtime_cells: Option<RuntimeCellRenderBinding<'_>>,
    flow_bindings: FlowRenderBindings<'_>,
    outofline_lifecycle: &HashMap<crate::ir::FunctionId, LifecycleEmit>,
) -> Result<(), EmitError> {
    let default_testbench_component_bindings = func
        .owner
        .and_then(|owner| prog.testbenches.get(owner.index()))
        .map(|testbench| {
            testbench
                .component_fields
                .iter()
                .map(|binding| super::expr::TestbenchComponentRenderBinding {
                    field: binding.field.clone(),
                    component: binding.component,
                    receiver: binding.field.clone(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let testbench_component_bindings = flow_bindings
        .testbench_components
        .unwrap_or(default_testbench_component_bindings.as_slice());
    let default_testbench_transactor_state_bindings = func
        .owner
        .and_then(|owner| prog.testbenches.get(owner.index()))
        .map(|testbench| {
            testbench
                .unbound_state_actors
                .iter()
                .map(|actor| super::expr::TestbenchTransactorStateRenderBinding {
                    field: actor.field.clone(),
                    transactor: actor.transactor,
                    receiver: actor.storage.clone(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let testbench_transactor_state_bindings = flow_bindings
        .testbench_transactor_states
        .unwrap_or(default_testbench_transactor_state_bindings.as_slice());
    let reserved = testbench_component_bindings
        .iter()
        .map(|binding| binding.receiver.as_str())
        .chain(
            testbench_transactor_state_bindings
                .iter()
                .map(|binding| binding.receiver.as_str()),
        )
        .chain(flow_bindings.reserved.iter().copied());
    let mut names = cpp_local_names_with_reserved(func, reserved);
    for local in predeclared {
        if let Some(name) = names.get_mut(local.index()) {
            *name = match runtime_cells {
                Some(binding) => binding.persistent_local(func, *local)?,
                None => flow_hook_capture_name(func, *local),
            };
        }
    }
    let cx = ECx {
        prog: Some(prog),
        func,
        names: &names,
        lanes,
        bindings: CallableRenderBindings {
            run_context: flow_bindings
                .run_context
                .or_else(|| common_contextual_tseqs.map(|_| "ctx")),
            dut_receiver: flow_bindings.dut_receiver,
            self_receiver: None,
            testbench_owner: func.owner,
            testbench_receiver: flow_bindings
                .testbench_receiver
                .or_else(|| func.owner.map(|_| "_tb")),
            bound_bus: None,
            testbench_bus_bindings: Some(bindings),
            bus_adapters: flow_bindings.bus_adapters,
            clocks: flow_bindings.clocks,
            actor_slot: None,
            testbench_components: (!testbench_component_bindings.is_empty())
                .then_some(testbench_component_bindings),
            testbench_transactor_states: (!testbench_transactor_state_bindings.is_empty())
                .then_some(testbench_transactor_state_bindings),
            runtime_cells,
            durable_callbacks: flow_bindings.durable_callbacks,
        },
        dut_type,
        dut_access: flow_bindings.dut_access,
        trace_component: "",
        state_receiver: None,
        temporal_widths: &[],
        temporal_cell_prefix: None,
        common_contextual_tseqs,
    };
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let pad3 = INDENT.repeat(depth + 3);
    writeln!(out, "{pad}{{ // {} (TB-IR loop-switch)", func.name).ok();
    declare_locals_except(
        out,
        prog,
        func,
        &names,
        0,
        predeclared,
        depth + 1,
        cx.dut_access,
    )?;
    declare_port_snapshots(out, &cx, depth + 1)?;
    writeln!(out, "{pad1}int __bb = {};", func.entry.0).ok();
    writeln!(out, "{pad1}bool __done = false;").ok();
    writeln!(out, "{pad1}while (!__done) {{").ok();
    writeln!(out, "{pad2}switch (__bb) {{").ok();
    for (bi, block) in func.blocks.iter().enumerate() {
        writeln!(out, "{pad2}case {bi}: {{").ok();
        for s in &block.stmts {
            emit_stmt(out, prog, &cx, records, bindings, s, depth + 3)?;
        }
        match &block.terminator {
            Terminator::Jump(b) => {
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::Branch(c, t, f) => {
                let cond = truthy_expr_cpp(&cx, c)?;
                writeln!(
                    out,
                    "{pad3}if ({cond}) {{ __bb = {}; }} else {{ __bb = {}; }}",
                    t.0, f.0
                )
                .ok();
            }
            Terminator::WaitCycles(n, clock, b) => {
                match clock {
                    None => {
                        let n = bounded_count_expr_cpp(&cx, n, u32::MAX as u64)?;
                        writeln!(
                            out,
                            "{pad3}co_await harc_rt::wait_cycles(_slot, (uint32_t)({n}));"
                        )
                        .ok();
                    }
                    Some(c) => {
                        let n = bounded_count_expr_cpp(&cx, n, i64::MAX as u64)?;
                        if let Some(context) = cx.bindings.run_context {
                            writeln!(
                                out,
                                "{pad3}harc_wait_clock_cycles({context}, \"{}\", (long long)({n}));",
                                super::expr::escape_c(&c.name)
                            )
                            .ok();
                        } else {
                            // `wait N cycles on <clock>` — mirror v1's
                            // inline eval_clocks_until loop (cpp_tb.rs,
                            // StmtKind::Wait with a clock): advance
                            // simulated time edge-by-edge until the named
                            // clock has seen N more rising edges, then run
                            // the checkers.
                            emit_local_qualified_clock_wait(out, &pad3, c.index, &n);
                        }
                    }
                }
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::WaitCyclesSync(n, b) => {
                // v1's non-coroutine wait path (helper / testbench-
                // method lambda bodies): synchronous tick loop, no
                // scheduler yield.
                let n = bounded_count_expr_cpp(&cx, n, i64::MAX as u64)?;
                let tick = cx
                    .bindings
                    .run_context
                    .map(|context| format!("harc_tseq_tick({context})"))
                    .unwrap_or_else(|| "tick()".to_string());
                writeln!(out, "{pad3}for (int _w = 0; _w < {n}; _w++) {tick};").ok();
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::WaitTimePs(ps, b) => {
                // Wall-clock wait — v1's inline emission for a Time
                // duration: advance absolute time, no coroutine yield,
                // no checker pass.
                if let Some(context) = cx.bindings.run_context {
                    writeln!(
                        out,
                        "{pad3}harc_eval_clocks_until({context}, {context}.now_ps + {ps});"
                    )
                    .ok();
                } else {
                    writeln!(out, "{pad3}eval_clocks_until(now_ps + {ps});").ok();
                }
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::Return => {
                writeln!(out, "{pad3}__done = true;").ok();
            }
            Terminator::TbLifecycleCall { function, succ } => {
                // #619 M4a: re-inline the once-lowered reusable testbench
                // lifecycle phase body right here, then resume at `succ`.
                // The callee is emitted as its OWN self-contained
                // loop-switch block (its `Return` sets its own `__done`,
                // exiting only the nested loop); any `wait` inside it
                // suspends this same run/check coroutine. Local names are
                // block-scoped to the nested `{ }`, so they cannot collide
                // with the caller's. This reproduces the exact statement
                // ORDER the historical per-test lifecycle inlining produced,
                // so the semantic trace matches — the emitted C++ is
                // trace-identical, not byte-identical (the nested loop-switch
                // and its local names differ textually). See
                // docs/619-m4a-ir-ownership.md.
                let callee = prog.function(*function);
                debug_assert!(
                    matches!(
                        callee.kind,
                        crate::ir::FunctionKind::TestbenchLifecycle { .. }
                    ),
                    "TbLifecycleCall must target a TestbenchLifecycle function"
                );
                let testbench_receiver = cx.bindings.testbench_receiver.ok_or_else(|| {
                    EmitError(format!(
                        "tbir: lifecycle call in {} has no testbench receiver",
                        func.name
                    ))
                })?;
                match outofline_lifecycle.get(function).copied() {
                    Some(LifecycleEmit::Plain) => {
                        // #619 M4b: the non-suspending callee is emitted once
                        // as a file-scope `void` function; lower the call edge
                        // to a real call. `ctx` and `_tb` are in scope at every
                        // call site (the run coroutine captures both), so the
                        // shared body reaches all its runtime state through the
                        // two params — the de-duplication #619 is about.
                        writeln!(
                            out,
                            "{pad3}{}(ctx, {testbench_receiver});",
                            lifecycle_cpp_name(&callee.name)
                        )
                        .ok();
                    }
                    Some(LifecycleEmit::Coro) => {
                        // #619 M4b: the SUSPENDING callee is emitted once as a
                        // file-scope `harc_rt::HarcThread` coroutine taking the
                        // caller's own `_slot`. The run coroutine drives it
                        // directly: `.resume()` runs it to its next `co_await
                        // wait_*(_slot, …)` (which parks the SHARED `_slot`),
                        // then the parent `co_await`s `harc_lifecycle_yield()`
                        // to suspend ITSELF on that already-set slot state
                        // (leaving `slot->thread` = this run coroutine). When
                        // the scheduler resumes the slot, the parent wakes and
                        // re-drives the child. The child's suspensions ARE the
                        // parent's suspensions on one slot — trace-identical to
                        // the M4a re-inline that pasted the same `co_await
                        // wait_*(_slot, …)` into the run coroutine. See
                        // `emit_lifecycle_coroutine` and the runtime awaiter.
                        writeln!(
                            out,
                            "{pad3}{{ auto _lc_sub = {}(ctx, {testbench_receiver}, _slot); _lc_sub.resume();",
                            lifecycle_cpp_name(&callee.name)
                        )
                        .ok();
                        writeln!(
                            out,
                            "{pad3}{INDENT}while (!_lc_sub.done()) {{ co_await \
                             harc_rt::harc_lifecycle_yield(); _lc_sub.resume(); }}"
                        )
                        .ok();
                        writeln!(out, "{pad3}{INDENT}_lc_sub.destroy(); }}").ok();
                    }
                    None => {
                        // M4a fallback: re-inline the once-lowered body here.
                        // The callee is emitted as its OWN self-contained
                        // loop-switch block (its `Return` sets its own
                        // `__done`, exiting only the nested loop); any `wait`
                        // inside it suspends this same run/check coroutine.
                        // Local names are block-scoped to the nested `{ }`, so
                        // they cannot collide with the caller's. Reproduces the
                        // exact statement ORDER the historical per-test
                        // lifecycle inlining produced. See
                        // docs/619-m4a-ir-ownership.md.
                        emit_function(
                            out,
                            prog,
                            callee,
                            records,
                            bindings,
                            lanes,
                            randomize_snippets,
                            dut_type,
                            &HashSet::new(),
                            depth + 3,
                            runtime_cells,
                            flow_bindings,
                            outofline_lifecycle,
                        )?;
                    }
                }
                writeln!(out, "{pad3}__bb = {};", succ.0).ok();
            }
            Terminator::Fatal(args) => {
                emit_log_call(out, &cx, "FATAL", None, args, depth + 3)?;
                writeln!(out, "{pad3}ctx.errors++; _fatal = true;").ok();
                writeln!(out, "{pad3}__done = true;").ok();
            }
            Terminator::WaitUntil { preds, mode, succ } => {
                // Mirrors v1's untimed coroutine path: one awaiter,
                // predicate re-evaluated by the scheduler each cycle.
                let cond = preds_cpp(&cx, preds, *mode)?;
                writeln!(
                    out,
                    "{pad3}co_await harc_rt::wait_until(_slot, [&]{{ return {cond}; }});"
                )
                .ok();
                writeln!(out, "{pad3}__bb = {};", succ.0).ok();
            }
            Terminator::WaitUntilTimeout {
                preds,
                mode,
                cycles,
                on_fire,
                on_timeout,
            } => {
                // Mirrors v1's timed coroutine path: budget evaluated
                // once, single `wait_until_timeout` awaiter returning
                // pred-fired (true) vs timed-out (false). v1 bumps
                // `errors` exactly once per timed-out wait; that bump
                // rides the timeout edge here so the `on_timeout`
                // block carries only the diagnostic text (FailDiag).
                let cond = preds_cpp(&cx, preds, *mode)?;
                let n = bounded_count_expr_cpp(&cx, cycles, u32::MAX as u64)?;
                writeln!(out, "{pad3}int64_t _wu_budget = (int64_t)({n});").ok();
                writeln!(
                    out,
                    "{pad3}bool _wu_satisfied = co_await harc_rt::wait_until_timeout(_slot, \
                     [&]{{ return {cond}; }}, (uint32_t)_wu_budget);"
                )
                .ok();
                writeln!(
                    out,
                    "{pad3}if (_wu_satisfied) {{ __bb = {}; }} else {{ ctx.errors++; __bb = {}; }}",
                    on_fire.0, on_timeout.0
                )
                .ok();
            }
            Terminator::Randomize {
                target,
                constraints,
                succ,
            } => {
                // Splice in v1's Z3-solve snippet for this site (built by
                // `cpp_tb::emit_randomize_snippets`, indexed by the
                // `ConstraintRef`, pre-indented at this body depth). The
                // solve writes the record fields back into the target
                // local and emits the trace event, exactly like v1.
                let run_context = cx.bindings.run_context.unwrap_or("ctx");
                let state_receiver = randomize_state_receiver(
                    func,
                    *constraints,
                    runtime_cells.filter(|_| cx.bindings.run_context.is_some()),
                    run_context,
                )?;
                let snippet = randomize_snippet_for(
                    prog,
                    func,
                    &names,
                    *target,
                    *constraints,
                    randomize_snippets,
                    run_context,
                    &state_receiver,
                    cx.bindings.self_receiver,
                )?
                .ok_or_else(|| {
                    EmitError(format!(
                        "tbir: Randomize in {} references missing constraint snippet c{}",
                        func.name, constraints.0
                    ))
                })?;
                out.push_str(&snippet);
                writeln!(out, "{pad3}__bb = {};", succ.0).ok();
            }
        }
        writeln!(out, "{pad3}break;").ok();
        writeln!(out, "{pad2}}}").ok();
    }
    writeln!(out, "{pad2}default: __done = true; break;").ok();
    writeln!(out, "{pad2}}}").ok();
    writeln!(out, "{pad1}}}").ok();
    writeln!(out, "{pad}}}").ok();
    Ok(())
}

/// C++ name of an out-of-line `TestbenchLifecycle` function (#619 M4b).
/// The IR name is already unique per (testbench, phase)
/// (`__tb_lifecycle_<tb>_<Phase>`); prefix it to keep the file-scope
/// symbol namespace obvious and to avoid colliding with user helpers.
pub(super) fn lifecycle_cpp_name(name: &str) -> String {
    format!("_harc_lc{name}")
}

/// #619 M4b: how an out-of-line reusable-lifecycle body is emitted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum LifecycleEmit {
    /// Non-suspending body → a plain `static void f(HarcTestContext&,
    /// <Tb>&)` called directly at the site.
    Plain,
    /// Suspending body → a `harc_rt::HarcThread f(HarcTestContext&, <Tb>&,
    /// ThreadSlot*)` coroutine driven by the run coroutine via the
    /// parent-drives-child loop (see the `TbLifecycleCall` arm).
    Coro,
}

/// #619 M4b: how (or whether) a `TestbenchLifecycle` body can be emitted
/// OUT-OF-LINE rather than re-inlined per the M4a fallback.
///
/// Returns `Some(Plain)` for a NON-suspending body (a `void` function the
/// run coroutine calls), `Some(Coro)` for a SUSPENDING body whose only
/// coroutine needs are the `_slot`-parking awaiters (`wait N cycles`
/// without a named clock, `wait until`, `wait until … timeout`), and
/// `None` when the body must stay re-inlined.
///
/// A body is shareable at all only when EVERY statement is in the
/// whitelist (`stmt_out_of_line_safe`) — value/DUT/`_tb`/log/assert forms
/// whose emission reaches only names the out-of-line prologue reconstructs
/// from `HarcTestContext& ctx` + `<Tb>& _tb` (plus the file-scope
/// `harc_rng`). That whitelist deliberately EXCLUDES the statements that
/// register a `[&]`-capturing closure into the per-test `_checkers` /
/// service lists (concurrent `cover`/property/`on`-handlers): a coroutine
/// or function frame that returns would leave those captures dangling
/// (same limitation as M4a), and it excludes `TransactorCall` (its
/// internal waits reach the frame-local `tick`).
///
/// Terminators then decide Plain vs Coro vs re-inline:
///   * `Jump`/`Branch`/`Return` — pure control flow; neither suspends nor
///     reaches the frame after every expression operand has passed the
///     frame-local scan below.
///   * `WaitCycles(None)`/`WaitUntil`/`WaitUntilTimeout` — suspend on the
///     shared `_slot` only → force the `Coro` variant.
///   * `Randomize` — an `harc_rng`-global Z3 solve that an out-of-line body
///     could technically emit, but sharing it is only RNG-order-safe because
///     `ShareScan` rejects randomize-bearing testbenches upstream; that
///     invariant is unenforced here, so `Randomize` returns `None`
///     defensively (kept re-inlined) rather than relying on it.
///   * `WaitCycles(Some(clock))` (`wait N cycles on <clk>`),
///     `WaitCyclesSync` (`tick()` loop), `WaitTimePs` (`after <dur>`),
///     `TbLifecycleCall`, `Fatal` — reach coroutine-frame-locals
///     (`clocks_`/`eval_clocks_until`/`tick`/`now_ps`) or are otherwise
///     unsupported here → the whole body stays re-inlined (conservative;
///     never emit a call that would reference an unreachable name).
pub(super) fn lifecycle_shareable_kind(func: &TbFunction) -> Option<LifecycleEmit> {
    let mut suspends = false;
    for b in &func.blocks {
        if !b.stmts.iter().all(stmt_out_of_line_safe) {
            return None;
        }
        if terminator_rvalue_reaches_frame_local(&b.terminator) {
            return None;
        }
        match &b.terminator {
            Terminator::Jump(_) | Terminator::Branch(..) | Terminator::Return => {}
            Terminator::WaitCycles(_, None, _)
            | Terminator::WaitUntil { .. }
            | Terminator::WaitUntilTimeout { .. } => suspends = true,
            // `Randomize` draws from the file-scope `harc_rng` global — an
            // out-of-line body COULD emit it — but sharing a randomize-bearing
            // lifecycle would only be RNG-order-correct because `ShareScan`
            // (`src/codegen/cpp_tb::desugar_impl_for_test_sharing_lifecycle`)
            // rejects randomize-bearing testbenches upstream, so no
            // `TestbenchLifecycle` is ever minted for one. That invariant is
            // not enforced HERE, so return `None` defensively: if a future
            // `ShareScan` relaxation started sharing randomize bodies, this
            // gate must NOT silently emit them out of line with a possibly
            // divergent RNG draw order. Keep re-inlined until a slice proves
            // the ordering (see docs/619-m4b-outofline.md, RNG-order risk).
            Terminator::Randomize { .. }
            // Frame-local-reaching or unsupported → keep re-inlined.
            | Terminator::WaitCycles(_, Some(_), _)
            | Terminator::WaitCyclesSync(..)
            | Terminator::WaitTimePs(..)
            | Terminator::TbLifecycleCall { .. }
            | Terminator::Fatal(_) => return None,
        }
    }
    Some(if suspends {
        LifecycleEmit::Coro
    } else {
        LifecycleEmit::Plain
    })
}

/// A terminator may carry the same frame-local host-state reads as a
/// whitelisted statement. These operands are emitted inside the out-of-line
/// lifecycle body (branch conditions, wait counts/predicates, and fatal
/// formatting), so scan them exhaustively before classifying the body.
fn terminator_rvalue_reaches_frame_local(t: &Terminator) -> bool {
    match t {
        Terminator::Jump(_)
        | Terminator::WaitTimePs(..)
        | Terminator::Randomize { .. }
        | Terminator::Return
        | Terminator::TbLifecycleCall { .. } => false,
        Terminator::Branch(cond, ..)
        | Terminator::WaitCycles(cond, ..)
        | Terminator::WaitCyclesSync(cond, ..) => expr_reaches_frame_local(cond),
        Terminator::WaitUntil { preds, .. } => preds
            .iter()
            .any(|pred| expr_reaches_frame_local(&pred.expr)),
        Terminator::WaitUntilTimeout { preds, cycles, .. } => {
            expr_reaches_frame_local(cycles)
                || preds
                    .iter()
                    .any(|pred| expr_reaches_frame_local(&pred.expr))
        }
        Terminator::Fatal(args) => args
            .args
            .iter()
            .any(|arg| expr_reaches_frame_local(&arg.expr)),
    }
}

/// Whitelist of `Stmt` kinds whose emission touches only `ctx`-reachable
/// state, `_tb`, the DUT, file-scope `harc_rng`, and reconstructed log
/// lambdas — i.e. nothing captured from the run coroutine's frame. See
/// `lifecycle_shareable_kind`.
///
/// The `Stmt` KIND is necessary but not sufficient: a whitelisted statement
/// can still carry an rvalue that emits code an out-of-line body cannot
/// support. Two failure classes:
///   1. A statement-level blocking TLM CALL, which lowers to
///      `Stmt::Assign(dest, Expr::Call(CallTarget::TransactorMethod, ..))`
///      (`ir::lower::bus`) and whose emission is `co_await
///      harc_rt::wait_cycles(_slot, …)` + `_slot` refs (`emit_transactor_call`).
///      A lifecycle body whose only content is such a call has terminator
///      `Return`, so without this guard it would classify as `Plain` and emit
///      `co_await`/`_slot` inside a `static void` function — a compile break.
///   2. A non-call HOST-STATE READ whose emitted receiver is a run-coroutine
///      frame local, not an ambient-prologue name — an owned/env scoreboard
///      (`ScoreboardQuery{nested_path: Some}`, emitted as bare `direct.count`)
///      or a bound-to transactor instance (`TransactorState*`). The out-of-line
///      body is handed only `ctx`/`_tb`/`dut` + log lambdas
///      (`emit_lifecycle_ambient_prologue`), so such a read references an
///      undeclared name (`use of undeclared identifier 'direct'`). See
///      `expr_reaches_frame_local`.
/// So a statement is out-of-line-safe only when its kind is whitelisted AND
/// none of its rvalue expressions reach a frame-local name
/// (`stmt_rvalue_reaches_frame_local`).
fn stmt_out_of_line_safe(s: &Stmt) -> bool {
    let kind_ok = matches!(
        s,
        Stmt::Assign(..)
            | Stmt::DutWrite(..)
            | Stmt::DutRead(..)
            | Stmt::ProbeRelease(..)
            | Stmt::RecordInit(..)
            | Stmt::RecordFieldWrite { .. }
            | Stmt::TbFieldWrite { .. }
            | Stmt::TbFieldVecElementWrite { .. }
            | Stmt::TbQueuePush { .. }
            | Stmt::TbQueuePop { .. }
            | Stmt::Log { .. }
            | Stmt::FailDiag { .. }
            | Stmt::AssertCheck { .. }
            | Stmt::AssumeCheck { .. }
    );
    kind_ok && !stmt_rvalue_reaches_frame_local(s)
}

/// A CALL target that an out-of-line lifecycle body cannot reach: a
/// bus/TLM `TransactorMethod` (emits `co_await`/`_slot`), a transactor
/// `TransactorSelfMethod`, or a `Tseq` generator (emitted as a
/// `[&]`-captured run-coroutine lambda). `Helper`/`ExternFn`/`Builtin`
/// resolve to file-scope symbols and are safe.
fn call_target_reaches_frame(t: &CallTarget) -> bool {
    matches!(
        t,
        CallTarget::TransactorMethod { .. }
            | CallTarget::TransactorSelfMethod { .. }
            | CallTarget::Tseq { .. }
    )
}

/// A composite-component read whose receiver is a run-frame local the
/// out-of-line ambient prologue does not reconstruct: `ComponentBase::Path`
/// (a bare `env.sb` test-scope path) or `SelfField` (`self`, whose
/// `self_subst` is `None` in a lifecycle). `ComponentBase::Local` is the
/// lifecycle function's own body-local and is safe.
fn component_base_reaches_frame_local(base: &crate::ir::ComponentBase) -> bool {
    use crate::ir::ComponentBase;
    matches!(base, ComponentBase::Path(_) | ComponentBase::SelfField)
}

/// A DUT port's RUNTIME lane index (`dut.vec[<expr>]`) is emitted by
/// `lane_index_cpp` as an arbitrary expression, so it can itself read a frame
/// local (`dut.vec[direct.count]`). A `None` / `Const` lane renders a literal
/// and is safe.
fn lane_reaches_frame_local(lane: &Option<crate::ir::LaneIndex>) -> bool {
    matches!(lane, Some(crate::ir::LaneIndex::Var(e)) if expr_reaches_frame_local(e))
}

/// Does an rvalue expression (recursively) reach a name an out-of-line
/// lifecycle body cannot provide? The out-of-line body is handed only
/// `ctx`/`_tb`/`dut` + the reconstructed log lambdas
/// (`emit_lifecycle_ambient_prologue`) plus the lifecycle function's OWN
/// locals; every other receiver in the emitted C++ is a run-coroutine frame
/// local. Two failure classes:
///   * a CALL to a frame-local / suspending target
///     (`call_target_reaches_frame`); or
///   * a non-call HOST-STATE READ whose emitted receiver is such a frame
///     local — an owned/env scoreboard (`ScoreboardQuery{nested_path: Some}`,
///     bare `direct.count`), a bound-to transactor instance / heartbeat stamp
///     (`TransactorState*` / `TransactorIdle`, bare `<storage>`), a
///     composite-component read through a `Path`/`SelfField` base (`Component*`
///     / `ComponentIdle`, bare `env.sb`), a concurrent-check latch cell
///     (`TemporalSlot`, `_harc_ps<i>`), a cover hook-sampler closure arg
///     (`CovHookParam`/`CovHookArg`), or a regblock mirror / bus-read helper
///     lambda (`RegRead`).
///
/// This match is EXHAUSTIVE — no `_` wildcard — so a new `Expr` variant is a
/// compile error here rather than a silent `false` that ships the next
/// undeclared-identifier miscompile. It mirrors the variant enumeration and
/// per-position recursion of `expr_uses_snapshot_lane`: safe receivers
/// (`_tb.<field>` / `dut` / body-locals / file scope) return `false` but every
/// sub-expression position (`inner`/`target`/`hi`/`lo`/`index`/`mid_indices`/
/// args/…) is still descended, so a frame local nested under a width-cast,
/// bit-slice, index, or arithmetic combinator (`_tb.mem[direct.count]`,
/// `(direct.count as uint<8>)`) is still caught.
fn expr_reaches_frame_local(e: &Expr) -> bool {
    match e {
        // ---- Frame-local LEAF reads: the emitted receiver is a run-frame
        //      local (bare instance/storage, closure cell, or `[&]` lambda)
        //      the ambient prologue does not reconstruct. ----
        Expr::TransactorState { .. }
        | Expr::TransactorStateRecordField { .. }
        | Expr::TransactorStateQueueQuery { .. }
        | Expr::TransactorIdle { .. }
        | Expr::TemporalSlot { .. }
        | Expr::CovHookParam { .. }
        | Expr::CovHookArg { .. }
        | Expr::RegRead { .. } => true,
        // Owned/env scoreboard read: `Some(path)` is a bare `<path>.<scalar>`
        // (or a `self`-rooted receiver, `self_subst == None` in a lifecycle);
        // `None` is `_tb.<field>` and is safe (in the safe-leaf arm below).
        Expr::ScoreboardQuery {
            nested_path: Some(_),
            ..
        } => true,
        // ---- Composite-component reads: frame-local only when the receiver
        //      base is `Path`/`SelfField`; `ComponentBase::Local` is this
        //      function's own body-local (safe). Index sub-expressions are
        //      still scanned. ----
        Expr::ComponentField { base, .. }
        | Expr::ComponentValue { base }
        | Expr::ComponentQueueQuery { base, .. } => component_base_reaches_frame_local(base),
        Expr::ComponentVecElement {
            base,
            index,
            inner_index,
            ..
        } => {
            component_base_reaches_frame_local(base)
                || expr_reaches_frame_local(index)
                || inner_index.as_deref().is_some_and(expr_reaches_frame_local)
        }
        Expr::ComponentIdle { base, n, .. } => {
            component_base_reaches_frame_local(base) || expr_reaches_frame_local(n)
        }
        // ---- Frame-reaching CALL target (TLM / tseq / transactor self-method);
        //      Helper/ExternFn/Builtin resolve to file scope. Args scanned. ----
        Expr::Call(target, args) => {
            call_target_reaches_frame(target) || args.iter().any(expr_reaches_frame_local)
        }
        // ---- Wrapper / index-bearing expressions: safe in themselves, but a
        //      sub-expression may reach a frame local. Recurse into EVERY
        //      sub-expression position. ----
        Expr::Binary(_, a, b) => expr_reaches_frame_local(a) || expr_reaches_frame_local(b),
        Expr::Unary(_, a) => expr_reaches_frame_local(a),
        Expr::Ternary(a, b, c) => {
            expr_reaches_frame_local(a)
                || expr_reaches_frame_local(b)
                || expr_reaches_frame_local(c)
        }
        Expr::WidthCast { inner, .. } => expr_reaches_frame_local(inner),
        Expr::BitSlice { target, .. } => expr_reaches_frame_local(target),
        Expr::BitSliceDyn { target, hi, lo } => {
            expr_reaches_frame_local(target)
                || expr_reaches_frame_local(hi)
                || expr_reaches_frame_local(lo)
        }
        Expr::DynamicListQuery { target, .. } => expr_reaches_frame_local(target),
        Expr::RecordField {
            mid_indices, index, ..
        } => {
            mid_indices
                .iter()
                .any(|(_, value)| expr_reaches_frame_local(value))
                || index.as_deref().is_some_and(expr_reaches_frame_local)
        }
        Expr::TbFieldVecElement {
            index, inner_index, ..
        } => {
            expr_reaches_frame_local(index)
                || inner_index.as_deref().is_some_and(expr_reaches_frame_local)
        }
        Expr::SeqIndex { index, .. } => expr_reaches_frame_local(index),
        Expr::PortSnapshotLane { index, .. } => expr_reaches_frame_local(index),
        // A DUT port read is `dut->…` (safe), but a runtime lane subscript is
        // an arbitrary expression that may reach a frame local.
        Expr::Port(p) => lane_reaches_frame_local(&p.lane),
        // ---- Safe leaves: literal / body-local / `_tb.<field>` / `dut` /
        //      file-scope receivers with no frame-local sub-expression. ----
        Expr::Literal { .. }
        | Expr::WideLiteral(_)
        | Expr::StringLiteral(_)
        | Expr::Local(_)
        | Expr::TbField(_)
        | Expr::TbQueueQuery { .. }
        | Expr::ScoreboardQuery {
            nested_path: None, ..
        }
        | Expr::CycleCount
        | Expr::ErrorCount
        | Expr::CovBin { .. }
        | Expr::SeqLen(_) => false,
    }
}

/// Does any expression carried by a whitelisted statement reach a frame-local
/// name (a frame-local/suspending call target OR a frame-local host-state
/// read)? Scans EVERY expression position of each whitelisted `Stmt` —
/// including DUT-port runtime lane subscripts (`dut.vec[direct.count] = …`),
/// not just rvalues — via `expr_reaches_frame_local`. Every whitelisted kind
/// (the same positive list as `stmt_out_of_line_safe`) is matched EXPLICITLY,
/// so the no-expression kinds are visibly `false` and a new expression field on
/// any of them is a compile error rather than a silent skip. The trailing
/// `_ => false` therefore covers ONLY non-whitelisted kinds, which
/// `stmt_out_of_line_safe` has already excluded before this is called.
fn stmt_rvalue_reaches_frame_local(s: &Stmt) -> bool {
    match s {
        Stmt::Assign(_, e) => expr_reaches_frame_local(e),
        // A DUT-port write/read/release: the lvalue `PortRef` may carry a
        // runtime lane subscript that reads a frame local.
        Stmt::DutWrite(port, e) => {
            lane_reaches_frame_local(&port.lane) || expr_reaches_frame_local(e)
        }
        Stmt::DutRead(_, port) | Stmt::ProbeRelease(port) => lane_reaches_frame_local(&port.lane),
        Stmt::TbFieldWrite { value, .. } | Stmt::TbQueuePush { value, .. } => {
            expr_reaches_frame_local(value)
        }
        Stmt::TbFieldVecElementWrite {
            index,
            inner_index,
            value,
            ..
        } => {
            expr_reaches_frame_local(index)
                || inner_index.as_ref().is_some_and(expr_reaches_frame_local)
                || expr_reaches_frame_local(value)
        }
        Stmt::RecordFieldWrite {
            mid_indices,
            index,
            value,
            ..
        } => {
            mid_indices.iter().any(|(_, e)| expr_reaches_frame_local(e))
                || index.as_ref().is_some_and(expr_reaches_frame_local)
                || expr_reaches_frame_local(value)
        }
        Stmt::Log { args, .. } => args.args.iter().any(|a| expr_reaches_frame_local(&a.expr)),
        Stmt::FailDiag { guard, args } => {
            guard.as_ref().is_some_and(expr_reaches_frame_local)
                || args.args.iter().any(|a| expr_reaches_frame_local(&a.expr))
        }
        Stmt::AssertCheck { cond, on_fail } | Stmt::AssumeCheck { cond, on_fail } => {
            expr_reaches_frame_local(cond)
                || on_fail
                    .args
                    .iter()
                    .any(|a| expr_reaches_frame_local(&a.expr))
        }
        // Whitelisted kinds that carry NO expression — explicit so a future
        // expression field breaks the build here instead of slipping past.
        // (`DutRead`/`ProbeRelease` are handled above for their port lane.)
        Stmt::RecordInit(_, _) => false,
        Stmt::TbQueuePop { field: _, dest: _ } => false,
        // Non-whitelisted kinds never reach here (gated by
        // `stmt_out_of_line_safe`'s kind whitelist).
        _ => false,
    }
}

/// #619 M4b: reconstruct, at the top of an out-of-line lifecycle
/// function/coroutine body, the ambient names the re-inlined body reads
/// from the run coroutine's frame. All are `HarcTestContext` members (M3
/// seam) or the file-scope `harc_rng`, so an out-of-line body reaches them
/// through the `ctx` param: `dut`/`errors`/`_fatal`/`cycle_count`/`trace`/
/// `log_ctx`/`_checkers` alias the matching members, and `sim_log_line` /
/// `sim_logf_line` are rebuilt as local lambdas over those aliases (they
/// are coroutine-locals in the inline form, not context members). Shared
/// by the `Plain` and `Coro` emitters so both see an identical name
/// environment (and therefore emit identical body text).
fn emit_lifecycle_ambient_prologue(out: &mut String, common_context: bool) {
    let trace_time = if common_context {
        "ctx.trace_time"
    } else {
        "ctx._trace_time"
    };
    let fatal = if common_context {
        "ctx.fatal"
    } else {
        "ctx._fatal"
    };
    out.push_str(
        "    (void)_tb;\n\
         \x20   auto* dut = ctx.dut; (void)dut;\n\
         #if HARC_TRACE_ENABLED\n\
             auto* tfp = ctx.tfp; (void)tfp;\n\
         #endif\n",
    );
    writeln!(
        out,
        "    auto& _trace_time = {trace_time}; (void)_trace_time;"
    )
    .ok();
    out.push_str("    auto& errors = ctx.errors; (void)errors;\n");
    writeln!(out, "    auto& _fatal = {fatal}; (void)_fatal;").ok();
    out.push_str(
        "    auto& cycle_count = ctx.cycle_count; (void)cycle_count;\n\
         \x20   auto& trace = ctx.trace; (void)trace;\n\
         \x20   auto& log_ctx = ctx.log_ctx; (void)log_ctx;\n\
         \x20   auto& _checkers = ctx._checkers; (void)_checkers;\n\
         \x20   auto sim_logf_line = [&](FILE* f, const char* sev, const char* fmt, ...) {\n\
         \x20       HARC_RT_LOG_FILE_ONLY_PRINTF(f, cycle_count, sev, fmt);\n\
         \x20   }; (void)sim_logf_line;\n\
         \x20   auto sim_log_line = [&](const char* sev, const char* fmt, ...) {\n\
         \x20       HARC_RT_LOG_PRINTF(log_ctx.sim_log, &trace, cycle_count, sev, fmt);\n\
         \x20   }; (void)sim_log_line;\n",
    );
}

/// #619 M4b: C++ signature (no trailing brace/semicolon) for a Plain
/// (non-suspending) out-of-line lifecycle function. `static_linkage` picks
/// internal (`static void …`, monolithic single-TU) vs external (`void …`,
/// the split/common layout where a shard in another TU must call it).
fn lifecycle_plain_sig(func: &TbFunction, tb_name: &str, static_linkage: bool) -> String {
    format!(
        "{}void {}(HarcTestContext& ctx, {tb_name}& _tb)",
        if static_linkage { "static " } else { "" },
        lifecycle_cpp_name(&func.name)
    )
}

/// #619 M4b: C++ signature (no trailing brace/semicolon) for a Coro
/// (suspending) out-of-line lifecycle coroutine. `static_linkage` picks
/// internal (`static harc_rt::HarcThread …`) vs external
/// (`harc_rt::HarcThread …`). A coroutine may have internal linkage, and it
/// MUST here for a complete single TU (monolithic + self-contained split):
/// the self-contained split emits EVERY shard as a full TU that defines the
/// shared bodies, and all shards link into one executable — an EXTERNAL
/// coroutine symbol would then be multiply-defined (duplicate-symbol link
/// error). Only the separate/COMMON layout keeps it external (one
/// definition in `common.cpp` + a header prototype the shards call).
fn lifecycle_coro_sig(func: &TbFunction, tb_name: &str, static_linkage: bool) -> String {
    format!(
        "{}harc_rt::HarcThread {}(HarcTestContext& ctx, {tb_name}& _tb, harc_rt::ThreadSlot* _slot)",
        if static_linkage { "static " } else { "" },
        lifecycle_cpp_name(&func.name)
    )
}

/// #619 M4b (split/common layout): emit a forward declaration for a shared
/// out-of-line lifecycle function into the interface header, so a shard in
/// another translation unit can call the definition that lives in the
/// common `.cpp`. Plain functions are declared with EXTERNAL linkage (no
/// `static`), matching their common-source definition.
pub(super) fn emit_lifecycle_prototype(
    out: &mut String,
    func: &TbFunction,
    tb_name: &str,
    kind: LifecycleEmit,
) {
    match kind {
        LifecycleEmit::Plain => {
            writeln!(out, "{};", lifecycle_plain_sig(func, tb_name, false)).ok();
        }
        LifecycleEmit::Coro => {
            // Prototypes exist only for the separate/COMMON layout, where the
            // single definition in `common.cpp` has EXTERNAL linkage.
            writeln!(out, "{};", lifecycle_coro_sig(func, tb_name, false)).ok();
        }
    }
}

/// #619 M4b: emit one NON-suspending `TestbenchLifecycle` body as a
/// file-scope function, ONCE per (testbench, phase). The run coroutine's
/// `TbLifecycleCall` lowers to `<name>(ctx, _tb)` (see the `TbLifecycleCall`
/// arm), so the shared body compiles once per suite instead of being
/// re-inlined once per test — the de-duplication #619 is about. Only bodies
/// classified `LifecycleEmit::Plain` reach here, so no `_slot`/`tick`/
/// scheduler name is ever referenced. `static_linkage` is `true` for the
/// monolithic single-TU emission (`static void`) and `false` for the
/// split/common layout (`void`, callable from a shard TU via the header
/// prototype).
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_lifecycle_function(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
    tb_name: &str,
    records: &[RecordSchema],
    bindings: &[BusBindingSchema],
    lanes: &HashMap<String, u32>,
    randomize_snippets: &[String],
    dut_type: &str,
    dut_access: Option<&crate::ir::passes::dut_access::DutAccessPlan>,
    common_context: bool,
    static_linkage: bool,
    outofline_lifecycle: &HashMap<crate::ir::FunctionId, LifecycleEmit>,
) -> Result<(), EmitError> {
    writeln!(
        out,
        "{} {{",
        lifecycle_plain_sig(func, tb_name, static_linkage)
    )
    .ok();
    emit_lifecycle_ambient_prologue(out, common_context);
    emit_function(
        out,
        prog,
        func,
        records,
        bindings,
        lanes,
        randomize_snippets,
        dut_type,
        &HashSet::new(),
        1,
        None,
        FlowRenderBindings {
            run_context: Some("ctx"),
            dut_receiver: Some("dut"),
            dut_access,
            testbench_receiver: Some("_tb"),
            dut_lane_widths: Some(lanes),
            ..FlowRenderBindings::default()
        },
        outofline_lifecycle,
    )?;
    writeln!(out, "}}").ok();
    writeln!(out).ok();
    Ok(())
}

/// #619 M4b: emit one SUSPENDING `TestbenchLifecycle` body as a file-scope
/// `harc_rt::HarcThread` coroutine taking the caller's own `_slot`, ONCE
/// per (testbench, phase). Its `co_await wait_*(_slot, …)` awaiters park
/// the SHARED `_slot` exactly as the M4a re-inline did; the run coroutine
/// drives it via the parent-drives-child loop emitted at the
/// `TbLifecycleCall` site. Only bodies classified `LifecycleEmit::Coro`
/// reach here, so the only coroutine construct the body needs is `_slot`
/// (no `tick`/`eval_clocks_until`/`now_ps`/`clocks_`). An explicit
/// `co_return;` after the loop-switch makes the body unambiguously a
/// coroutine even when its only awaits are on branches not taken.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_lifecycle_coroutine(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
    tb_name: &str,
    records: &[RecordSchema],
    bindings: &[BusBindingSchema],
    lanes: &HashMap<String, u32>,
    randomize_snippets: &[String],
    dut_type: &str,
    dut_access: Option<&crate::ir::passes::dut_access::DutAccessPlan>,
    common_context: bool,
    static_linkage: bool,
    outofline_lifecycle: &HashMap<crate::ir::FunctionId, LifecycleEmit>,
) -> Result<(), EmitError> {
    writeln!(
        out,
        "{} {{",
        lifecycle_coro_sig(func, tb_name, static_linkage)
    )
    .ok();
    writeln!(out, "{INDENT}(void)_slot;").ok();
    emit_lifecycle_ambient_prologue(out, common_context);
    emit_function(
        out,
        prog,
        func,
        records,
        bindings,
        lanes,
        randomize_snippets,
        dut_type,
        &HashSet::new(),
        1,
        None,
        FlowRenderBindings {
            run_context: Some("ctx"),
            dut_receiver: Some("dut"),
            dut_access,
            testbench_receiver: Some("_tb"),
            dut_lane_widths: Some(lanes),
            ..FlowRenderBindings::default()
        },
        outofline_lifecycle,
    )?;
    writeln!(out, "{INDENT}co_return;").ok();
    writeln!(out, "}}").ok();
    writeln!(out).ok();
    Ok(())
}

/// Emit one `tseq` body as a `[&]`-capturing lambda named after the tseq,
/// returning `std::vector<Record>` — v1's `emit_tseq` shape. The body is
/// the same loop-switch as a method, but `Return` returns the `RecordSeq`
/// accumulator (`ret`), `SeqPush` appends to it, and `Randomize` splices
/// the shared Z3-solve snippet (tseq randomize sites live in the same
/// global constraint table as test-body sites). A tseq body is host-side
/// (randomize + yield + literal-range loops) — synchronous waits emit as
/// `tick()` loops (v1's lambda semantics), and coroutine-only suspensions
/// are a lowering-gate failure.
pub(super) fn emit_tseq(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
    records: &[RecordSchema],
    randomize_snippets: &[String],
    depth: usize,
) -> Result<(), EmitError> {
    emit_tseq_impl(
        out,
        prog,
        func,
        records,
        randomize_snippets,
        depth,
        TseqEmission::LocalLambda,
    )
}

pub(super) fn tseq_dependencies(function: &TbFunction) -> BTreeSet<FunctionId> {
    let mut dependencies = BTreeSet::new();
    for_each_function_expr(function, |expr| {
        if let Expr::Call(CallTarget::Tseq { function, .. }, _) = expr {
            dependencies.insert(*function);
        }
    });
    dependencies
}

pub(super) fn tseq_emit_order(prog: &TbProgram) -> Result<Vec<FunctionId>, EmitError> {
    let tseqs: Vec<&TbFunction> = prog
        .functions
        .iter()
        .filter(|function| matches!(function.kind, FunctionKind::Tseq { .. }))
        .collect();
    let mut by_id = HashMap::new();
    for (index, function) in tseqs.iter().enumerate() {
        if by_id.insert(function.id, index).is_some() {
            return Err(EmitError(format!(
                "tbir: duplicate tseq name `{}` in callable dependency graph",
                function.name
            )));
        }
    }
    let mut dependencies = vec![Vec::new(); tseqs.len()];
    for (index, function) in tseqs.iter().enumerate() {
        for dependency in tseq_dependencies(function) {
            let Some(&dependency_index) = by_id.get(&dependency) else {
                return Err(EmitError(format!(
                    "tbir: tseq `{}` references missing tseq fn{}",
                    function.name, dependency.0
                )));
            };
            dependencies[index].push(dependency_index);
        }
    }

    fn visit(
        index: usize,
        tseqs: &[&TbFunction],
        dependencies: &[Vec<usize>],
        state: &mut [u8],
        stack: &mut Vec<usize>,
        order: &mut Vec<FunctionId>,
    ) -> Result<(), EmitError> {
        match state[index] {
            2 => return Ok(()),
            1 => {
                let start = stack.iter().position(|node| *node == index).unwrap_or(0);
                let mut names: Vec<&str> = stack[start..]
                    .iter()
                    .map(|node| tseqs[*node].name.as_str())
                    .collect();
                names.push(tseqs[index].name.as_str());
                return Err(EmitError(format!(
                    "tbir: tseq dependency cycle: {}",
                    names.join(" -> ")
                )));
            }
            _ => {}
        }
        state[index] = 1;
        stack.push(index);
        for &dependency in &dependencies[index] {
            visit(dependency, tseqs, dependencies, state, stack, order)?;
        }
        stack.pop();
        state[index] = 2;
        order.push(tseqs[index].id);
        Ok(())
    }

    let mut state = vec![0; tseqs.len()];
    let mut order = Vec::with_capacity(tseqs.len());
    for index in 0..tseqs.len() {
        visit(
            index,
            &tseqs,
            &dependencies,
            &mut state,
            &mut Vec::new(),
            &mut order,
        )?;
    }
    Ok(order)
}

pub(super) fn for_each_function_expr(function: &TbFunction, mut visit: impl FnMut(&Expr)) {
    for block in &function.blocks {
        for stmt in &block.stmts {
            crate::ir::visit::try_visit_stmt_exprs(stmt, &mut |expr| {
                crate::ir::visit::walk_expr(expr, &mut visit);
                Ok::<(), std::convert::Infallible>(())
            })
            .unwrap_or_else(|error| match error {});
        }
        crate::ir::visit::try_visit_terminator_exprs(&block.terminator, &mut |expr| {
            crate::ir::visit::walk_expr(expr, &mut visit);
            Ok::<(), std::convert::Infallible>(())
        })
        .unwrap_or_else(|error| match error {});
    }
}

pub(super) fn emit_common_tseq_declaration(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
    needs_context: bool,
) -> Result<(), EmitError> {
    let ret_ty = tseq_return_cty(prog, func)?;
    let params = tseq_param_list(prog, func, needs_context)?;
    writeln!(
        out,
        "{ret_ty} {}({params});",
        super::expr::tseq_cpp_name(&func.name)
    )
    .ok();
    Ok(())
}

pub(super) fn emit_common_tseq_function(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
    records: &[RecordSchema],
    randomize_snippets: &[String],
    needs_context: bool,
    contextual_tseqs: &BTreeSet<FunctionId>,
) -> Result<(), EmitError> {
    emit_tseq_impl(
        out,
        prog,
        func,
        records,
        randomize_snippets,
        0,
        TseqEmission::Common {
            needs_context,
            contextual_tseqs,
        },
    )
}

#[derive(Clone, Copy)]
enum TseqEmission<'a> {
    LocalLambda,
    Common {
        needs_context: bool,
        contextual_tseqs: &'a BTreeSet<FunctionId>,
    },
}

fn tseq_return_cty(prog: &TbProgram, func: &TbFunction) -> Result<String, EmitError> {
    let acc_ty = func
        .ret
        .map(|r| func.local(r).ty.clone())
        .unwrap_or(IrType::Unknown);
    match &acc_ty {
        IrType::RecordSeq(_) | IrType::Seq(_) => super::callable_value_cty(prog, &acc_ty),
        _ => {
            return Err(EmitError(format!(
                "tbir: tseq `{}` has no RecordSeq/Seq return accumulator (lowering bug)",
                func.name
            )));
        }
    }
}

fn callable_value_cty(prog: &TbProgram, ty: &IrType) -> Result<String, EmitError> {
    super::callable_value_cty(prog, ty)
}

#[cfg(test)]
mod value_abi_type_tests {
    use super::super::field_scalar_cty;
    use crate::codegen::cpp_tb::ir_vec_elem_class;
    use crate::ir::{sequence_element_compatible, value_abi_class, IrType, ValueAbiClass};

    #[test]
    fn neutral_classes_and_both_codegen_storage_mappings_agree() {
        let cases = [
            (IrType::Bool, ValueAbiClass::Bool, "bool"),
            (IrType::UInt(Some(8)), ValueAbiClass::Unsigned64, "uint64_t"),
            (
                IrType::UInt(Some(64)),
                ValueAbiClass::Unsigned64,
                "uint64_t",
            ),
            (IrType::SInt(Some(8)), ValueAbiClass::Signed64, "int64_t"),
            (IrType::UInt(Some(65)), ValueAbiClass::Wide128, "_harc_u128"),
            (
                IrType::SInt(Some(128)),
                ValueAbiClass::Wide128,
                "_harc_u128",
            ),
            (
                IrType::UInt(Some(129)),
                ValueAbiClass::WideWords(5),
                "harc_rt::HarcWide<5>",
            ),
        ];
        for (ty, class, cpp) in cases {
            assert_eq!(value_abi_class(&ty), Some(class));
            assert_eq!(ir_vec_elem_class(&ty).as_deref(), Some(cpp));
            assert_eq!(field_scalar_cty(&ty), cpp);
        }
    }

    #[test]
    fn sequence_compatibility_requires_safe_direction_and_one_carrier() {
        assert!(sequence_element_compatible(
            &IrType::UInt(Some(32)),
            &IrType::UInt(Some(8))
        ));
        assert!(!sequence_element_compatible(
            &IrType::UInt(Some(8)),
            &IrType::UInt(Some(32))
        ));
        assert!(!sequence_element_compatible(
            &IrType::UInt(Some(8)),
            &IrType::Bool
        ));
        assert!(!sequence_element_compatible(
            &IrType::UInt(Some(65)),
            &IrType::UInt(Some(64))
        ));
        assert!(!sequence_element_compatible(
            &IrType::UInt(Some(32)),
            &IrType::SInt(Some(8))
        ));
        assert!(sequence_element_compatible(
            &IrType::UInt(Some(128)),
            &IrType::UInt(Some(65))
        ));
        assert!(sequence_element_compatible(
            &IrType::UInt(Some(130)),
            &IrType::UInt(Some(129))
        ));
        assert!(!sequence_element_compatible(
            &IrType::Seq(Box::new(IrType::UInt(Some(8)))),
            &IrType::Seq(Box::new(IrType::UInt(Some(8))))
        ));
    }
}

fn tseq_param_list(
    prog: &TbProgram,
    func: &TbFunction,
    needs_context: bool,
) -> Result<String, EmitError> {
    let names = cpp_local_names(func);
    let mut params = Vec::with_capacity(func.params.len() + usize::from(needs_context));
    if needs_context {
        params.push("HarcTestContext& ctx".to_string());
    }
    for (index, name) in names[..func.params.len()].iter().enumerate() {
        params.push(format!(
            "{} {name}",
            callable_value_cty(prog, &func.locals[index].ty)?
        ));
    }
    Ok(params.join(", "))
}

fn emit_tseq_impl(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
    records: &[RecordSchema],
    randomize_snippets: &[String],
    depth: usize,
    emission: TseqEmission<'_>,
) -> Result<(), EmitError> {
    let names = cpp_local_names(func);
    // tseq bodies hold no packed-lane DUT access (no DUT at all), so no
    // probe access either (`dut_type` unused → `""`).
    let empty_lanes = HashMap::new();
    let cx = ECx {
        prog: Some(prog),
        func,
        names: &names,
        lanes: &empty_lanes,
        bindings: CallableRenderBindings {
            run_context: matches!(emission, TseqEmission::Common { .. }).then_some("ctx"),
            dut_receiver: None,
            self_receiver: None,
            testbench_owner: None,
            testbench_receiver: None,
            bound_bus: None,
            testbench_bus_bindings: None,
            bus_adapters: None,
            clocks: None,
            actor_slot: None,
            testbench_components: None,
            testbench_transactor_states: None,
            runtime_cells: None,
            durable_callbacks: false,
        },
        dut_type: "",
        dut_access: None,
        trace_component: "",
        state_receiver: None,
        temporal_widths: &[],
        temporal_cell_prefix: None,
        common_contextual_tseqs: match emission {
            TseqEmission::LocalLambda => None,
            TseqEmission::Common {
                contextual_tseqs, ..
            } => Some(contextual_tseqs),
        },
    };
    let nparams = func.params.len();
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let pad3 = INDENT.repeat(depth + 3);

    // The return type preserves the accumulator's record or scalar element.
    let ret_ty = tseq_return_cty(prog, func)?;
    match emission {
        TseqEmission::LocalLambda => {
            let params = tseq_param_list(prog, func, false)?;
            writeln!(
                out,
                "{pad}auto {} = [&]({params}) -> {ret_ty} {{",
                super::expr::tseq_cpp_name(&func.name)
            )
            .ok();
        }
        TseqEmission::Common { needs_context, .. } => {
            let params = tseq_param_list(prog, func, needs_context)?;
            writeln!(
                out,
                "{pad}{ret_ty} {}({params}) {{",
                super::expr::tseq_cpp_name(&func.name)
            )
            .ok();
            if needs_context {
                writeln!(out, "{pad1}auto* dut = ctx.dut; (void)dut;").ok();
                writeln!(out, "{pad1}auto& errors = ctx.errors;").ok();
                writeln!(out, "{pad1}auto& _fatal = ctx.fatal;").ok();
                writeln!(out, "{pad1}auto& cycle_count = ctx.cycle_count;").ok();
                writeln!(out, "{pad1}auto& trace = ctx.trace;").ok();
                writeln!(out, "{pad1}auto& log_ctx = ctx.log_ctx;").ok();
                writeln!(
                    out,
                    "{pad1}auto& _auto_cov_reports = ctx._auto_cov_reports;"
                )
                .ok();
                writeln!(out, "{pad1}auto& harc_rng = ctx.rng; (void)harc_rng;").ok();
                writeln!(out, "{pad1}auto tick = [&]() {{ harc_tseq_tick(ctx); }};").ok();
                writeln!(
                    out,
                    "{pad1}auto sim_logf_line = [&](FILE* f, const char* sev, const char* fmt, ...) {{"
                )
                .ok();
                writeln!(
                    out,
                    "{pad2}HARC_RT_LOG_FILE_ONLY_PRINTF(f, cycle_count, sev, fmt);"
                )
                .ok();
                writeln!(out, "{pad1}}};").ok();
                writeln!(
                    out,
                    "{pad1}auto sim_log_line = [&](const char* sev, const char* fmt, ...) {{"
                )
                .ok();
                writeln!(out, "{pad2}va_list ap;").ok();
                writeln!(out, "{pad2}va_start(ap, fmt);").ok();
                writeln!(out, "{pad2}harc_rt::log::harc_log_vline(log_ctx.sim_log, &trace, cycle_count, sev, fmt, ap);").ok();
                writeln!(out, "{pad2}va_end(ap);").ok();
                writeln!(out, "{pad1}}};").ok();
            }
        }
    }
    let uses_tseq_unique = func.blocks.iter().any(|block| {
        let Terminator::Randomize { constraints, .. } = &block.terminator else {
            return false;
        };
        randomize_snippets
            .get(constraints.index())
            .is_some_and(|snippet| snippet.contains(TBIR_RANDOMIZE_TSEQ_SCOPE_SENTINEL))
    });
    if uses_tseq_unique {
        writeln!(
            out,
            "{pad1}harc_rt::random::HarcUniqueRegistry _harc_unique_tseq;"
        )
        .ok();
    }
    declare_locals(out, prog, func, &names, nparams, depth + 1, cx.dut_access)?;
    writeln!(out, "{pad1}int __bb = {};", func.entry.0).ok();
    writeln!(out, "{pad1}while (true) {{").ok();
    writeln!(out, "{pad2}switch (__bb) {{").ok();
    for (bi, block) in func.blocks.iter().enumerate() {
        writeln!(out, "{pad2}case {bi}: {{").ok();
        for s in &block.stmts {
            // tseq bodies have no testbench bus-binding scope.
            emit_stmt(out, prog, &cx, records, &[], s, depth + 3)?;
        }
        match &block.terminator {
            Terminator::Jump(b) => {
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::Branch(c, t, f) => {
                let cond = truthy_expr_cpp(&cx, c)?;
                writeln!(
                    out,
                    "{pad3}if ({cond}) {{ __bb = {}; }} else {{ __bb = {}; }}",
                    t.0, f.0
                )
                .ok();
            }
            Terminator::WaitCycles(n, None, b) => {
                // v1's synchronous lambda wait: one tick() per cycle.
                let n = bounded_count_expr_cpp(&cx, n, i64::MAX as u64)?;
                writeln!(
                    out,
                    "{pad3}for (int64_t _w = 0; _w < (int64_t)({n}); _w++) tick();"
                )
                .ok();
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::WaitUntil { preds, mode, succ } => {
                let cond = preds_cpp(&cx, preds, *mode)?;
                writeln!(out, "{pad3}while (!({cond})) tick();").ok();
                writeln!(out, "{pad3}__bb = {};", succ.0).ok();
            }
            Terminator::Randomize {
                target,
                constraints,
                succ,
            } => {
                let run_context = cx.bindings.run_context.unwrap_or("ctx");
                let state_receiver =
                    randomize_state_receiver(func, *constraints, None, run_context)?;
                let snippet = randomize_snippet_for(
                    prog,
                    func,
                    &names,
                    *target,
                    *constraints,
                    randomize_snippets,
                    run_context,
                    &state_receiver,
                    cx.bindings.self_receiver,
                )?
                .ok_or_else(|| {
                    EmitError(format!(
                        "tbir: Randomize in tseq {} references missing constraint snippet c{}",
                        func.name, constraints.0
                    ))
                })?;
                out.push_str(&snippet);
                writeln!(out, "{pad3}__bb = {};", succ.0).ok();
            }
            Terminator::Return => match func.ret {
                Some(r) => {
                    writeln!(out, "{pad3}return {};", names[r.index()]).ok();
                }
                None => {
                    // A tseq always has a RecordSeq accumulator; defend
                    // anyway with an empty return.
                    writeln!(out, "{pad3}return {{}};").ok();
                }
            },
            other @ (Terminator::WaitCycles(_, Some(_), _)
            | Terminator::WaitCyclesSync(_, _)
            | Terminator::WaitTimePs(_, _)
            | Terminator::WaitUntilTimeout { .. }
            | Terminator::TbLifecycleCall { .. }
            | Terminator::Fatal(_)) => {
                return Err(EmitError(format!(
                    "tbir: tseq `{}` contains terminator {other:?} — lowering gate failed",
                    func.name
                )));
            }
        }
        writeln!(out, "{pad3}break;").ok();
        writeln!(out, "{pad2}}}").ok();
    }
    writeln!(out, "{pad2}default: return {{}};").ok();
    writeln!(out, "{pad2}}}").ok();
    writeln!(out, "{pad1}}}").ok();
    match emission {
        TseqEmission::LocalLambda => writeln!(out, "{pad}}};").ok(),
        TseqEmission::Common { .. } => writeln!(out, "{pad}}}").ok(),
    };
    Ok(())
}

/// Return type for a helper: the declared type of its return slot, or
/// `void` when it has none. Kept identical between the prototype and the
/// definition so the two C++ signatures match.
fn helper_ret_cty(prog: &TbProgram, func: &TbFunction) -> Result<String, EmitError> {
    match func.ret {
        Some(r) => func
            .locals
            .get(r.index())
            .ok_or_else(|| {
                EmitError(format!(
                    "tbir: helper `{}` return local %{} does not resolve",
                    func.name, r.0
                ))
            })
            .and_then(|local| callable_value_cty(prog, &local.ty)),
        None => Ok("void".to_string()),
    }
}

/// Comma-joined parameter list for a helper's C++ signature. The first
/// `params.len()` locals ARE the parameters (TB-IR convention), so each
/// param's declared type comes from `func.locals[i].ty`.
fn helper_param_list(
    prog: &TbProgram,
    func: &TbFunction,
    names: &[String],
) -> Result<String, EmitError> {
    names[..func.params.len()]
        .iter()
        .enumerate()
        .map(|(i, n)| {
            func.locals
                .get(i)
                .ok_or_else(|| {
                    EmitError(format!(
                        "tbir: helper `{}` parameter {} has no mirrored local",
                        func.name, i
                    ))
                })
                .and_then(|local| callable_value_cty(prog, &local.ty))
                .map(|cty| format!("{cty} {n}"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|params| params.join(", "))
}

/// Forward declaration for a lowered pure helper, so source-order
/// emission supports helper-to-helper calls in any order.
pub(super) fn emit_helper_prototype(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
) -> Result<(), EmitError> {
    emit_helper_declaration(out, prog, func, true)
}

pub(super) fn emit_common_helper_declaration(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
) -> Result<(), EmitError> {
    emit_helper_declaration(out, prog, func, false)
}

fn emit_helper_declaration(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
    internal: bool,
) -> Result<(), EmitError> {
    let names = cpp_local_names(func);
    let ret_ty = helper_ret_cty(prog, func)?;
    let params = helper_param_list(prog, func, &names)?;
    let linkage = if internal { "static " } else { "" };
    writeln!(
        out,
        "{linkage}{ret_ty} {}({params});",
        helper_cpp_name(&func.name)
    )
    .ok();
    Ok(())
}

/// Emit one `FunctionKind::Helper` function (a lowered *pure* helper)
/// as a file-scope C++ function. Pure helpers contain only value
/// computation — no DUT access, no logging, no suspension — so the
/// loop-switch body is restricted to `Assign` statements and
/// `Jump`/`Branch`/`Return` terminators; anything else is a lowering
/// bug surfaced as an `EmitError`.
///
/// Signature convention: the first `params.len()` locals ARE the
/// parameters (TB-IR convention), so they emit as parameters and are
/// not re-declared in the body. Parameters, internal locals, and the
/// return slot use the ordinary TBIR scalar mapping or the aggregate
/// `std::array`/`std::vector` carriers so their complete types survive the
/// helper ABI.
pub(super) fn emit_helper_function(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
) -> Result<(), EmitError> {
    emit_helper_function_with_linkage(out, prog, func, true)
}

pub(super) fn emit_common_helper_function(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
) -> Result<(), EmitError> {
    emit_helper_function_with_linkage(out, prog, func, false)
}

fn emit_helper_function_with_linkage(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
    internal: bool,
) -> Result<(), EmitError> {
    let names = cpp_local_names(func);
    // Pure helpers have no DUT access, so no lane table or probe access
    // (`dut_type` unused → `""`). Record locals use the program's
    // already-emitted record structs.
    let empty_lanes = HashMap::new();
    let cx = ECx {
        prog: Some(prog),
        func,
        names: &names,
        lanes: &empty_lanes,
        bindings: CallableRenderBindings::default(),
        dut_type: "",
        dut_access: None,
        trace_component: "",
        state_receiver: None,
        temporal_widths: &[],
        temporal_cell_prefix: None,
        common_contextual_tseqs: None,
    };
    let nparams = func.params.len();

    let ret_ty = helper_ret_cty(prog, func)?;
    let params = helper_param_list(prog, func, &names)?;
    let linkage = if internal { "static " } else { "" };
    writeln!(
        out,
        "{linkage}{ret_ty} {}({params}) {{",
        helper_cpp_name(&func.name)
    )
    .ok();
    for (local, name) in func.locals.iter().zip(&names).skip(nparams) {
        match local.ty {
            IrType::Record(record) => {
                let schema = prog.records.get(record.index()).ok_or_else(|| {
                    EmitError(format!(
                        "tbir: local `{name}` in pure helper {} references missing record r{}",
                        func.name, record.0
                    ))
                })?;
                // A scalar parameter or another local may hide the record
                // type name before this hoisted declaration.
                writeln!(out, "{INDENT}::{} {name}{{}}; (void){name};", schema.name).ok();
            }
            IrType::Component(component) => {
                let schema = prog.components.get(component.index()).ok_or_else(|| {
                    EmitError(format!(
                        "tbir: local `{name}` in pure helper {} references missing component c{}",
                        func.name, component.0
                    ))
                })?;
                writeln!(out, "{INDENT}::{} {name}{{}}; (void){name};", schema.name).ok();
            }
            IrType::RecordSeq(_) | IrType::Seq(_) | IrType::FixedVec { .. } => {
                let cty = callable_value_cty(prog, &local.ty)?;
                writeln!(out, "{INDENT}{cty} {name}{{}}; (void){name};").ok();
            }
            _ => {
                let cty = callable_value_cty(prog, &local.ty)?;
                let init = if matches!(local.ty, IrType::String) {
                    "nullptr"
                } else {
                    "0"
                };
                writeln!(out, "{INDENT}{cty} {name} = {init}; (void){name};").ok();
            }
        }
    }
    writeln!(out, "{INDENT}int __bb = {};", func.entry.0).ok();
    writeln!(out, "{INDENT}while (true) {{").ok();
    writeln!(out, "{INDENT}{INDENT}switch (__bb) {{").ok();
    let pad2 = INDENT.repeat(2);
    let pad3 = INDENT.repeat(3);
    for (bi, block) in func.blocks.iter().enumerate() {
        writeln!(out, "{pad2}case {bi}: {{").ok();
        for s in &block.stmts {
            match s {
                Stmt::Assign(..) => {
                    emit_stmt(out, prog, &cx, &prog.records, &[], s, 3)?;
                }
                Stmt::RecordInit(local, record) => {
                    let name = &names[local.index()];
                    prog.records.get(record.index()).ok_or_else(|| {
                        EmitError(format!(
                            "tbir: RecordInit of `{name}` in pure helper {} references missing record r{}",
                            func.name, record.0
                        ))
                    })?;
                    // The local may legally have the same source name as its
                    // record type (`let Req : Req`). After declaration that
                    // identifier hides the type, so `Req = Req{};` is invalid
                    // C++. Construct through the local's declared type.
                    writeln!(out, "{pad3}{name} = decltype({name}){{}};").ok();
                }
                Stmt::AggregateInit(local) => {
                    let name = &names[local.index()];
                    writeln!(out, "{pad3}{name} = decltype({name}){{}};").ok();
                }
                Stmt::ComponentInit {
                    local, component, ..
                } => {
                    let name = &names[local.index()];
                    prog.components.get(component.index()).ok_or_else(|| {
                        EmitError(format!(
                            "tbir: ComponentInit of `{name}` in pure helper {} references missing component c{}",
                            func.name, component.0
                        ))
                    })?;
                    writeln!(out, "{pad3}{name} = decltype({name}){{}};").ok();
                }
                Stmt::RecordFieldWrite { .. } => {
                    emit_stmt(out, prog, &cx, &prog.records, &[], s, 3)?;
                }
                other => {
                    return Err(EmitError(format!(
                        "tbir: pure helper `{}` contains a non-Assign statement ({other:?}) — \
                         categorization bug",
                        func.name
                    )));
                }
            }
        }
        match &block.terminator {
            Terminator::Jump(b) => {
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::Branch(c, t, f) => {
                let cond = truthy_expr_cpp(&cx, c)?;
                writeln!(
                    out,
                    "{pad3}if ({cond}) {{ __bb = {}; }} else {{ __bb = {}; }}",
                    t.0, f.0
                )
                .ok();
            }
            Terminator::Return => match func.ret {
                Some(r) => {
                    writeln!(out, "{pad3}return {};", names[r.index()]).ok();
                }
                None => {
                    writeln!(out, "{pad3}return;").ok();
                }
            },
            other => {
                return Err(EmitError(format!(
                    "tbir: pure helper `{}` contains terminator {other:?} — categorization bug",
                    func.name
                )));
            }
        }
        writeln!(out, "{pad3}break;").ok();
        writeln!(out, "{pad2}}}").ok();
    }
    match func.ret {
        Some(_) => writeln!(out, "{pad2}default: return {{}};").ok(),
        None => writeln!(out, "{pad2}default: return;").ok(),
    };
    writeln!(out, "{INDENT}{INDENT}}}").ok();
    writeln!(out, "{INDENT}}}").ok();
    writeln!(out, "}}").ok();
    Ok(())
}

pub(super) fn temporal_cell_cpp_type(
    prog: &TbProgram,
    func: &TbFunction,
    lanes: &HashMap<String, u32>,
    dut_type: &str,
    dut_access: Option<&crate::ir::passes::dut_access::DutAccessPlan>,
    slot: &crate::ir::TemporalSlot,
) -> String {
    let names = cpp_local_names(func);
    let cx = ECx {
        prog: Some(prog),
        func,
        names: &names,
        lanes,
        bindings: CallableRenderBindings {
            testbench_owner: func.owner,
            testbench_receiver: func.owner.map(|_| "_tb"),
            ..CallableRenderBindings::default()
        },
        dut_type,
        dut_access,
        trace_component: "",
        state_receiver: None,
        temporal_widths: &[],
        temporal_cell_prefix: None,
        common_contextual_tseqs: None,
    };
    match expr_static_width(&cx, &slot.inner) {
        Some(width) if width > 128 => {
            format!("harc_rt::HarcWide<{}>", width.div_ceil(32))
        }
        Some(width) if width > 64 => "_harc_u128".to_string(),
        _ => "int64_t".to_string(),
    }
}

fn require_runtime_cell_receiver<'a>(
    cx: &ECx<'a>,
    kind: &crate::ir::passes::runtime_cells::RuntimeCellKind,
) -> Result<&'a str, EmitError> {
    cx.bindings
        .runtime_cells
        .ok_or_else(|| {
            EmitError(format!(
                "tbir: {} requires runtime cell {kind:?} without a typed binding",
                cx.func.name
            ))
        })?
        .require(kind)
}

fn test_hook_cpp_name(cx: &ECx<'_>, function: FunctionId) -> Result<String, EmitError> {
    cx.bindings
        .runtime_cells
        .ok_or_else(|| {
            EmitError(format!(
                "tbir: {} references test-hook fn{} without a typed runtime-cell binding",
                cx.func.name, function.0
            ))
        })?
        .test_hook(function)
}

fn component_base_schema<'a>(
    cx: &ECx<'a>,
    base: &crate::ir::ComponentBase,
) -> Result<&'a crate::ir::ComponentSchema, EmitError> {
    use crate::ir::{ComponentBase, ComponentFieldKind, FunctionKind};
    let prog = cx.prog.ok_or_else(|| {
        EmitError(format!(
            "tbir: component value in {} has no typed program binding",
            cx.func.name
        ))
    })?;
    let (mut component, tail): (crate::ir::ComponentId, &[String]) = match base {
        ComponentBase::Local(local) => match cx.func.locals.get(local.index()).map(|l| &l.ty) {
            Some(IrType::Component(component)) => (*component, &[]),
            _ => {
                return Err(EmitError(format!(
                    "tbir: component value local %{} in {} has no component type",
                    local.0, cx.func.name
                )))
            }
        },
        ComponentBase::SelfField => match cx.func.kind {
            FunctionKind::ComponentMethod { component, .. } => (component, &[]),
            _ => {
                return Err(EmitError(format!(
                    "tbir: self-relative component value in {} has no component owner",
                    cx.func.name
                )))
            }
        },
        ComponentBase::Path(path) => {
            let (root, mut tail) = path.split_first().ok_or_else(|| {
                EmitError(format!("tbir: empty component path in {}", cx.func.name))
            })?;
            if root == "self" {
                let FunctionKind::ComponentMethod { component, .. } = cx.func.kind else {
                    return Err(EmitError(format!(
                        "tbir: self-rooted component path in {} has no component owner",
                        cx.func.name
                    )));
                };
                (component, tail)
            } else if let Some(binding) = cx
                .bindings
                .testbench_components
                .and_then(|bindings| bindings.iter().find(|binding| binding.field == *root))
            {
                tail = &path[1..];
                (binding.component, tail)
            } else {
                let owner = cx
                    .bindings
                    .testbench_owner
                    .or(cx.func.owner)
                    .ok_or_else(|| {
                        EmitError(format!(
                            "tbir: component path `{}` in {} has no testbench owner",
                            path.join("."),
                            cx.func.name
                        ))
                    })?;
                let testbench = prog.testbenches.get(owner.index()).ok_or_else(|| {
                    EmitError(format!(
                        "tbir: component path `{}` references missing testbench tb{}",
                        path.join("."),
                        owner.0
                    ))
                })?;
                let binding = testbench
                    .component_fields
                    .iter()
                    .find(|binding| binding.field == *root)
                    .ok_or_else(|| {
                        EmitError(format!(
                            "tbir: component path `{}` has no typed root binding",
                            path.join(".")
                        ))
                    })?;
                tail = &path[1..];
                (binding.component, tail)
            }
        }
    };
    for field in tail {
        let schema = prog.components.get(component.index()).ok_or_else(|| {
            EmitError(format!("tbir: component c{} does not resolve", component.0))
        })?;
        component = schema
            .field(field)
            .and_then(|field| match field.kind {
                ComponentFieldKind::Sub { component, .. } => Some(component),
                _ => None,
            })
            .ok_or_else(|| {
                EmitError(format!(
                    "tbir: component path segment `{field}` in {} is not a sub-component",
                    cx.func.name
                ))
            })?;
    }
    prog.components
        .get(component.index())
        .ok_or_else(|| EmitError(format!("tbir: component c{} does not resolve", component.0)))
}

fn component_subpath_schema<'a>(
    cx: &ECx<'a>,
    base: &crate::ir::ComponentBase,
    subpath: &[String],
) -> Result<(crate::ir::ComponentId, &'a crate::ir::ComponentSchema), EmitError> {
    let prog = cx.prog.ok_or_else(|| {
        EmitError(format!(
            "tbir: component value in {} has no typed program binding",
            cx.func.name
        ))
    })?;
    let mut schema = component_base_schema(cx, base)?;
    let mut component = prog
        .components
        .iter()
        .position(|candidate| std::ptr::eq(candidate, schema))
        .map(|index| crate::ir::ComponentId(index as u32))
        .ok_or_else(|| EmitError("tbir: resolved component schema is not in the program".into()))?;
    for field in subpath {
        component = schema
            .field(field)
            .and_then(|field| match field.kind {
                crate::ir::ComponentFieldKind::Sub { component, .. } => Some(component),
                _ => None,
            })
            .ok_or_else(|| {
                EmitError(format!(
                    "tbir: component subpath segment `{field}` in {} is not a sub-component",
                    cx.func.name
                ))
            })?;
        schema = prog.components.get(component.index()).ok_or_else(|| {
            EmitError(format!("tbir: component c{} does not resolve", component.0))
        })?;
    }
    Ok((component, schema))
}

fn scalar_shape_type(cx: &ECx<'_>, destination: &Expr) -> Option<IrType> {
    let width = expr_static_width(cx, destination)?;
    Some(if expr_is_signed(cx, destination) {
        IrType::SInt(Some(width))
    } else {
        IrType::UInt(Some(width))
    })
}

fn scalar_sink_expr_cpp(
    cx: &ECx<'_>,
    value: &Expr,
    destination: Option<IrType>,
) -> Result<String, EmitError> {
    match destination {
        Some(ty @ (IrType::UInt(Some(target)) | IrType::SInt(Some(target))))
            if expr_is_signed(cx, value)
                && expr_static_width(cx, value)
                    .is_some_and(|source| source < target && (target <= 128 || source > 64)) =>
        {
            scalar_assignment_expr_cpp(cx, value, &ty)
        }
        _ => expr_cpp(cx, value),
    }
}

fn emit_local_qualified_clock_wait(out: &mut String, pad: &str, clock_index: usize, cycles: &str) {
    writeln!(
        out,
        "{pad}{{ if (_harc_actor_tick_due) {{ _harc_advance_actors(); _harc_actor_tick_due = false; }} long long _target = clocks_[{clock_index}].rising_count + (long long)({cycles}); while (clocks_[{clock_index}].rising_count < _target && !_fatal) {{"
    )
    .ok();
    writeln!(out, "{pad}{INDENT}int _before_cycle = cycle_count;").ok();
    writeln!(
        out,
        "{pad}{INDENT}long long _next = clocks_[0].next_edge_ps;"
    )
    .ok();
    writeln!(out, "{pad}{INDENT}for (auto& _ck : clocks_) if (_ck.next_edge_ps < _next) _next = _ck.next_edge_ps;").ok();
    writeln!(out, "{pad}{INDENT}eval_clocks_until(_next);").ok();
    writeln!(
        out,
        "{pad}{INDENT}if (cycle_count != _before_cycle) _harc_advance_actors();"
    )
    .ok();
    writeln!(out, "{pad}}} for (auto& _c : _checkers) _c(); }}").ok();
}

fn event_payload_ir_type(payload: crate::ir::EventPayload) -> IrType {
    payload.value_ir_type()
}

/// Declare each temporal latch's previous-value cell and this
/// cycle's value local, at the head of a concurrent-check closure.
/// Returns nothing; the matching write-back is `emit_temporal_writeback`.
fn emit_temporal_latches(
    out: &mut String,
    cx: &ECx<'_>,
    temporals: &[crate::ir::TemporalSlot],
    depth: usize,
) -> Result<Vec<Option<u32>>, EmitError> {
    let pad = INDENT.repeat(depth);
    let widths: Vec<_> = temporals
        .iter()
        .map(|slot| expr_static_width(cx, &slot.inner))
        .collect();
    if !temporals.is_empty() && cx.temporal_cell_prefix.is_none() {
        return Err(EmitError(format!(
            "tbir: {} has temporal history without a runtime-cell binding",
            cx.func.name
        )));
    }
    for (i, slot) in temporals.iter().enumerate() {
        let inner = expr_cpp(cx, &slot.inner)?;
        match widths[i] {
            Some(width) if width > 128 => {
                let words = width.div_ceil(32);
                writeln!(
                    out,
                    "{pad}harc_rt::HarcWide<{words}> _harc_cur{i} = {inner};"
                )
                .ok();
            }
            Some(width) if width > 64 => {
                writeln!(out, "{pad}_harc_u128 _harc_cur{i} = (_harc_u128)({inner});").ok();
            }
            _ => {
                writeln!(out, "{pad}int64_t _harc_cur{i} = (int64_t)({inner});").ok();
            }
        }
    }
    Ok(widths)
}

/// Copy each latch's current value into its per-run cell, so the next
/// cycle's `past`/`rose`/`fell`/`stable` reads see it.
fn emit_temporal_writeback(
    out: &mut String,
    cx: &ECx<'_>,
    n: usize,
    depth: usize,
) -> Result<(), EmitError> {
    if n == 0 {
        return Ok(());
    }
    let prefix = cx.temporal_cell_prefix.ok_or_else(|| {
        EmitError(format!(
            "tbir: {} has temporal history without a runtime-cell binding",
            cx.func.name
        ))
    })?;
    let pad = INDENT.repeat(depth);
    for i in 0..n {
        let previous = format!("{prefix}_{i}");
        writeln!(out, "{pad}{previous} = _harc_cur{i};").ok();
    }
    Ok(())
}

/// One concurrent property check: a `_checkers` closure evaluated after
/// every primary-clock edge from this registration point onward. Mirrors
/// v1's `emit_property_check` — same three temporal shapes, same failure
/// lines, same error-counter policy (`assert` bumps, `assume` does not).
fn emit_property_check(
    out: &mut String,
    cx: &ECx<'_>,
    id: crate::ir::PropertyCheckId,
    schema: &crate::ir::PropertyCheckSchema,
    depth: usize,
) -> Result<(), EmitError> {
    use crate::ir::PropertyShape;
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let sev = schema.severity.tag();
    let label = escape_c(&schema.label);

    let capture = persistent_callback_capture(cx);
    writeln!(out, "{pad}_checkers.push_back([{capture}]() {{").ok();
    emit_persistent_callback_bindings(out, cx, depth + 1);
    let temporal_prefix = if schema.temporals.is_empty() {
        None
    } else {
        for slot in 0..schema.temporals.len() {
            require_runtime_cell_receiver(
                cx,
                &crate::ir::passes::runtime_cells::RuntimeCellKind::TemporalPrevious {
                    check: crate::ir::passes::runtime_cells::TemporalCheck::Property(id),
                    slot: slot as u32,
                },
            )?;
        }
        let first = cx
            .bindings
            .runtime_cells
            .expect("binding required above")
            .field(
                &crate::ir::passes::runtime_cells::RuntimeCellKind::TemporalPrevious {
                    check: crate::ir::passes::runtime_cells::TemporalCheck::Property(id),
                    slot: 0,
                },
            )?;
        Some(first.strip_suffix("_0").unwrap_or(&first).to_string())
    };
    let cell_cx = ECx {
        temporal_cell_prefix: temporal_prefix.as_deref(),
        ..*cx
    };
    let temporal_widths = emit_temporal_latches(out, &cell_cx, &schema.temporals, depth + 1)?;
    let temporal_cx = ECx {
        temporal_widths: &temporal_widths,
        ..cell_cx
    };
    let cx = &temporal_cx;

    // The failure arm is identical across shapes apart from the operator
    // suffix in the generic message, so build it once. An `else fail(...)`
    // clause replaces the generic line entirely — including the operator
    // suffix, which only ever explained the generic wording.
    let fail_arm = |out: &mut String, suffix: &str| -> Result<(), EmitError> {
        match &schema.message {
            Some(args) => emit_log_call(out, cx, sev, None, args, depth + 2)?,
            None => {
                writeln!(
                    out,
                    "{pad2}sim_log_line(\"{sev}\", \"property `{label}` failed{suffix}\");"
                )
                .ok();
            }
        }
        if schema.severity.counts_as_error() {
            writeln!(out, "{pad2}ctx.errors++;").ok();
        }
        Ok(())
    };

    match &schema.shape {
        // `a |=> b` — the antecedent is remembered for one cycle, so the
        // check fires on the cycle AFTER `a` held.
        PropertyShape::ImpliesNext { ante, cons } => {
            let a = truthy_expr_cpp(cx, ante)?;
            let b = truthy_expr_cpp(cx, cons)?;
            let previous = cx
                .bindings
                .runtime_cells
                .ok_or_else(|| {
                    EmitError(format!(
                        "tbir: {} has a property implication without runtime-cell storage",
                        cx.func.name
                    ))
                })?
                .field(
                    &crate::ir::passes::runtime_cells::RuntimeCellKind::PropertyImplicationPrevious {
                        property: id,
                    },
                )?;
            writeln!(out, "{pad1}bool _harc_a = (bool)({a});").ok();
            writeln!(out, "{pad1}bool _harc_b = (bool)({b});").ok();
            writeln!(out, "{pad1}if ({previous} && !_harc_b) {{").ok();
            fail_arm(out, " (|=>)")?;
            writeln!(out, "{pad1}}}").ok();
            writeln!(out, "{pad1}{previous} = _harc_a;").ok();
        }
        PropertyShape::Implies { ante, cons } => {
            let a = truthy_expr_cpp(cx, ante)?;
            let b = truthy_expr_cpp(cx, cons)?;
            writeln!(out, "{pad1}if ((bool)({a}) && !(bool)({b})) {{").ok();
            fail_arm(out, " (|->)")?;
            writeln!(out, "{pad1}}}").ok();
        }
        PropertyShape::Invariant(e) => {
            let c = truthy_expr_cpp(cx, e)?;
            writeln!(out, "{pad1}if (!({c})) {{").ok();
            fail_arm(out, "")?;
            writeln!(out, "{pad1}}}").ok();
        }
    }

    emit_temporal_writeback(out, cx, schema.temporals.len(), depth + 1)?;
    writeln!(out, "{pad}}});").ok();
    Ok(())
}

/// One concurrent `cover` witness: a `_checkers` closure that bumps the
/// check's per-run hit counter on every primary-clock edge where the
/// predicate holds. Mirrors v1's `StmtKind::Cover` emission, plus the
/// temporal-latch machinery v1's cover path lacks.
fn emit_cover_check(
    out: &mut String,
    cx: &ECx<'_>,
    id: crate::ir::CoverCheckId,
    schema: &crate::ir::CoverCheckSchema,
    depth: usize,
) -> Result<(), EmitError> {
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let binding = cx.bindings.runtime_cells.ok_or_else(|| {
        EmitError(format!(
            "tbir: {} has a cover check without runtime-cell storage",
            cx.func.name
        ))
    })?;
    let counter = binding
        .field(&crate::ir::passes::runtime_cells::RuntimeCellKind::CoverHits { cover: id })?;
    let capture = persistent_callback_capture(cx);
    writeln!(out, "{pad}_checkers.push_back([{capture}]() {{").ok();
    emit_persistent_callback_bindings(out, cx, depth + 1);
    for slot in 0..schema.temporals.len() {
        require_runtime_cell_receiver(
            cx,
            &crate::ir::passes::runtime_cells::RuntimeCellKind::TemporalPrevious {
                check: crate::ir::passes::runtime_cells::TemporalCheck::Cover(id),
                slot: slot as u32,
            },
        )?;
    }
    let temporal_prefix = if schema.temporals.is_empty() {
        None
    } else {
        let first = binding.field(
            &crate::ir::passes::runtime_cells::RuntimeCellKind::TemporalPrevious {
                check: crate::ir::passes::runtime_cells::TemporalCheck::Cover(id),
                slot: 0,
            },
        )?;
        Some(first.strip_suffix("_0").unwrap_or(&first).to_string())
    };
    let cell_cx = ECx {
        temporal_cell_prefix: temporal_prefix.as_deref(),
        ..*cx
    };
    let temporal_widths = emit_temporal_latches(out, &cell_cx, &schema.temporals, depth + 1)?;
    let temporal_cx = ECx {
        temporal_widths: &temporal_widths,
        ..cell_cx
    };
    let cond = truthy_expr_cpp(&temporal_cx, &schema.cond)?;
    writeln!(out, "{pad1}if ((bool)({cond})) {counter}++;").ok();
    emit_temporal_writeback(out, &temporal_cx, schema.temporals.len(), depth + 1)?;
    writeln!(out, "{pad}}});").ok();
    Ok(())
}

/// Arm one statement-position `on` handler: a `_checkers` /
/// `_post_eval_services` closure that calls the handler body's lambda
/// when its trigger fires. Byte-for-byte the state machine v1's
/// `emit_cycle_trigger` installs — a rising/falling edge latch or a
/// last-fire stamp — with the body factored into a named lambda instead
/// of inlined, because the TB-IR body is its own function.
///
/// The body lambda is declared at test scope (the `TestHook` emission
/// loop in `mod::emit_test`), NOT here: a lambda declared inside the
/// run coroutine's `switch` case would die at the end of the case block
/// while the registered closure still referenced it.
fn emit_cycle_handler(
    out: &mut String,
    cx: &ECx<'_>,
    id: crate::ir::CycleHandlerId,
    schema: &crate::ir::CycleHandlerSchema,
    depth: usize,
) -> Result<(), EmitError> {
    use crate::ir::CycleHandlerKind;
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let lambda = test_hook_cpp_name(cx, schema.function)?;
    // Per-handler tag for temporary values. It is unique per closure and
    // keeps debugger output attributable to the source handler.
    let tag = format!("_onh_{}", id.0);
    let vec = schema.phase.service_vec();
    let capture = persistent_callback_capture(cx);
    writeln!(out, "{pad}{vec}.push_back([{capture}]() {{").ok();
    emit_persistent_callback_bindings(out, cx, depth + 1);
    match &schema.kind {
        CycleHandlerKind::Periodic { period } => {
            // `last = 0` means the FIRST firing is at cycle `period`, not
            // cycle 0 — "every N cycles", not "now and every N cycles".
            // Re-read the expression every cycle, matching v1 and allowing a
            // captured statement local to change after registration.
            let period = expr_cpp(cx, period)?;
            let last = cx
                .bindings
                .runtime_cells
                .ok_or_else(|| {
                    EmitError(format!(
                        "tbir: {} has a periodic handler without runtime-cell storage",
                        cx.func.name
                    ))
                })?
                .field(
                    &crate::ir::passes::runtime_cells::RuntimeCellKind::StatementPeriodicLast {
                        handler: id,
                    },
                )?;
            writeln!(out, "{pad1}int64_t {tag}_period = (int64_t)({period});").ok();
            writeln!(
                out,
                "{pad1}if ({tag}_period > 0 && (int64_t)cycle_count - {last} >= {tag}_period) {{"
            )
            .ok();
            writeln!(out, "{pad2}{last} = (int64_t)cycle_count;").ok();
            writeln!(out, "{pad2}{lambda}();").ok();
            writeln!(out, "{pad1}}}").ok();
        }
        CycleHandlerKind::Trigger { trigger, edge } => {
            let t = truthy_expr_cpp(cx, trigger)?;
            match edge {
                crate::ir::CycleEdge::Level => {
                    writeln!(out, "{pad1}if ((bool)({t})) {{").ok();
                    writeln!(out, "{pad2}{lambda}();").ok();
                    writeln!(out, "{pad1}}}").ok();
                }
                crate::ir::CycleEdge::Rising | crate::ir::CycleEdge::Falling => {
                    let previous = cx
                        .bindings
                        .runtime_cells
                        .ok_or_else(|| {
                            EmitError(format!(
                                "tbir: {} has an edge handler without runtime-cell storage",
                                cx.func.name
                            ))
                        })?
                        .field(
                            &crate::ir::passes::runtime_cells::RuntimeCellKind::StatementEdgePrevious {
                                handler: id,
                            },
                        )?;
                    writeln!(out, "{pad1}bool {tag}_curr = (bool)({t});").ok();
                    let cond = match edge {
                        crate::ir::CycleEdge::Rising => format!("!{previous} && {tag}_curr"),
                        _ => format!("{previous} && !{tag}_curr"),
                    };
                    writeln!(out, "{pad1}if ({cond}) {{").ok();
                    writeln!(out, "{pad2}{lambda}();").ok();
                    writeln!(out, "{pad1}}}").ok();
                    writeln!(out, "{pad1}{previous} = {tag}_curr;").ok();
                }
            }
        }
    }
    writeln!(out, "{pad}}});").ok();
    Ok(())
}

/// C++ payload type of an event-channel local — the parameter type of
/// its `std::function`. Mirrors v1's `payload_type_for_arg`: a scalar
/// widens to `uint64_t` / `int64_t`, a record payload is the record
/// struct by value.
fn event_payload_cty(prog: &TbProgram, cx: &ECx<'_>, event: LocalId) -> Result<String, EmitError> {
    match cx.func.locals.get(event.index()).map(|l| &l.ty) {
        // `field_scalar_cty` on the payload's own `IrType`, so a wide
        // payload becomes `_harc_u128` / `harc_rt::HarcWide<N>` rather
        // than a 64-bit parameter that silently truncates every
        // notification. v1 emits the same.
        Some(IrType::Event(p @ crate::ir::EventPayload::Scalar { .. })) => Ok(
            super::field_scalar_cty(&p.scalar_ir_type().expect("a scalar payload types")),
        ),
        Some(IrType::Event(crate::ir::EventPayload::Record(r))) => {
            Ok(prog.records[r.index()].name.clone())
        }
        Some(IrType::Event(p @ crate::ir::EventPayload::FixedVec { .. })) => Ok(
            super::aggregate_value_cty(&p.value_ir_type(), &prog.records),
        ),
        _ => Err(EmitError(format!(
            "tbir: {} uses local {} as an event channel but it is not event-typed",
            cx.func.name, event.0
        ))),
    }
}

fn hook_param_cty(prog: &TbProgram, ty: &IrType) -> Result<String, EmitError> {
    super::callable_value_cty(prog, ty)
}

fn persistent_callback_capture(cx: &ECx<'_>) -> &'static str {
    if cx.bindings.durable_callbacks {
        "_harc_callback_state = &_harc_run_state, _harc_callback_context = &ctx"
    } else {
        "&"
    }
}

fn emit_persistent_callback_bindings(out: &mut String, cx: &ECx<'_>, depth: usize) {
    if !cx.bindings.durable_callbacks {
        return;
    }
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    writeln!(out, "{pad}auto& _harc_run_state = *_harc_callback_state;").ok();
    writeln!(out, "{pad}auto& ctx = *_harc_callback_context;").ok();
    writeln!(out, "{pad}auto* dut = ctx.dut;").ok();
    writeln!(out, "{pad}auto& errors = ctx.errors;").ok();
    writeln!(out, "{pad}auto& _fatal = ctx.fatal;").ok();
    writeln!(out, "{pad}auto& cycle_count = ctx.cycle_count;").ok();
    writeln!(out, "{pad}auto& trace = ctx.trace;").ok();
    writeln!(out, "{pad}auto& log_ctx = ctx.log_ctx;").ok();
    writeln!(out, "{pad}auto& _checkers = ctx._checkers;").ok();
    writeln!(
        out,
        "{pad}auto& _post_eval_services = ctx._post_eval_services;"
    )
    .ok();
    writeln!(out, "{pad}auto& _auto_cov_reports = ctx._auto_cov_reports;").ok();
    writeln!(out, "{pad}auto& harc_rng = ctx.rng;").ok();
    if let Some(testbench) = super::expr::owner_tb(cx) {
        for binding in &testbench.regblock_bindings {
            writeln!(
                out,
                "{pad}auto& {} = _harc_run_state.{};",
                binding.field, binding.field
            )
            .ok();
            if !binding.callbacks.is_empty() {
                writeln!(
                    out,
                    "{pad}auto& {}_cb_depth = _harc_run_state.{}_cb_depth;",
                    binding.field, binding.field
                )
                .ok();
            }
        }
    }
    writeln!(
        out,
        "{pad}auto sim_logf_line = [&](FILE* f, const char* sev, const char* fmt, ...) {{"
    )
    .ok();
    writeln!(
        out,
        "{pad1}HARC_RT_LOG_FILE_ONLY_PRINTF(f, cycle_count, sev, fmt);"
    )
    .ok();
    writeln!(out, "{pad}}};").ok();
    writeln!(
        out,
        "{pad}auto sim_log_line = [&](const char* sev, const char* fmt, ...) {{"
    )
    .ok();
    writeln!(out, "{pad1}va_list ap;").ok();
    writeln!(out, "{pad1}va_start(ap, fmt);").ok();
    writeln!(
        out,
        "{pad1}harc_rt::log::harc_log_vline(log_ctx.sim_log, &trace, cycle_count, sev, fmt, ap);"
    )
    .ok();
    writeln!(out, "{pad1}va_end(ap);").ok();
    writeln!(out, "{pad}}};").ok();
}

fn emit_stmt(
    out: &mut String,
    prog: &TbProgram,
    cx: &ECx<'_>,
    records: &[RecordSchema],
    bindings: &[BusBindingSchema],
    s: &Stmt,
    depth: usize,
) -> Result<(), EmitError> {
    let pad = INDENT.repeat(depth);
    let func = cx.func;
    let names = cx.names;
    match s {
        Stmt::Assign(l, e) => {
            // TransactorMethod call edge — the IR carries only the
            // call; the single-site backend expands it here into v1's
            // blocking req/rsp wire protocol (the verifier pinned the
            // edge to exactly this position).
            if let Expr::Call(
                CallTarget::TransactorMethod {
                    bus_field,
                    method,
                    target,
                },
                args,
            ) = e
            {
                return emit_transactor_call(
                    out, cx, records, bindings, *l, bus_field, method, target, args, depth,
                );
            }
            let name = local_cpp_name(cx, *l)?;
            let e = scalar_sink_expr_cpp(cx, e, local_ir_type(cx, *l).cloned())?;
            writeln!(out, "{pad}{name} = {e};").ok();
        }
        Stmt::RecordInit(l, r) => {
            let name = local_cpp_name(cx, *l)?;
            // Shared test-scope records are default-constructed once by
            // the enclosing test. Re-initializing them inside run/check/
            // hook bodies would wipe persistent host state.
            if func
                .testbench_record_locals
                .iter()
                .any(|binding| binding.local == *l)
                || shared_record_names(prog, func).contains(&name)
            {
                return Ok(());
            }
            let rec = records.get(r.index()).ok_or_else(|| {
                EmitError(format!(
                    "tbir: RecordInit of `{name}` in {} references missing record r{}",
                    func.name, r.0
                ))
            })?;
            writeln!(out, "{pad}{name} = {}{{}};", rec.name).ok();
        }
        Stmt::AggregateInit(l) => {
            let name = &names[l.index()];
            writeln!(out, "{pad}{name} = decltype({name}){{}};").ok();
        }
        Stmt::ComponentInit {
            local, component, ..
        } => {
            let name = local_cpp_name(cx, *local)?;
            prog.components.get(component.index()).ok_or_else(|| {
                EmitError(format!(
                    "tbir: ComponentInit of `{name}` in {} references missing component c{}",
                    func.name, component.0
                ))
            })?;
            writeln!(out, "{pad}{name} = decltype({name}){{}};").ok();
        }
        Stmt::TransactorCall { dest, call } => {
            let Expr::Call(
                CallTarget::TransactorMethod {
                    bus_field,
                    method,
                    target,
                },
                args,
            ) = call
            else {
                return Err(EmitError(format!(
                    "tbir: TransactorCall in {} carries a non-call-edge payload \
                     (verifier invariant violated)",
                    func.name
                )));
            };
            let crate::ir::TransactorMethodTarget::Callable {
                transactor,
                function,
            } = target
            else {
                return Err(EmitError(format!(
                    "tbir: TransactorCall `{bus_field}.{method}` in {} does not carry a typed callable target",
                    func.name
                )));
            };
            // Resolve the instance field to its transactor type via the
            // owner testbench — the lambda is named `<Type>_<method>`,
            // mirroring v1's hookable lambda naming.
            let owner = super::expr::owner_tb(cx).ok_or_else(|| {
                EmitError(format!(
                    "tbir: TransactorCall `{bus_field}.{method}` in {} does not \
                         resolve through the owner testbench",
                    func.name
                ))
            })?;
            let receiver_type = owner
                .transactor_fields
                .iter()
                .find(|(field, _)| field == bus_field)
                .map(|(_, transactor)| *transactor)
                .ok_or_else(|| {
                    EmitError(format!(
                        "tbir: TransactorCall `{bus_field}.{method}` in {} has no typed receiver field",
                        func.name
                    ))
                })?;
            if receiver_type != *transactor {
                return Err(EmitError(format!(
                    "tbir: TransactorCall `{bus_field}.{method}` in {} carries transactor x{} but the receiver has type x{}",
                    func.name, transactor.0, receiver_type.0
                )));
            }
            let xschema = prog.transactors.get(transactor.index()).ok_or_else(|| {
                EmitError(format!(
                    "tbir: TransactorCall `{bus_field}.{method}` in {} references missing transactor x{}",
                    func.name, transactor.0
                ))
            })?;
            let Some(method_schema) = xschema
                .method(method)
                .filter(|schema| schema.function == *function)
            else {
                return Err(EmitError(format!(
                    "tbir: TransactorCall `{bus_field}.{method}` in {} carries stale callable fn{}",
                    func.name, function.0
                )));
            };
            let xname = xschema.emission_name();
            let mut rendered = Vec::with_capacity(args.len() + 3);
            if let Some(context) = cx.bindings.run_context {
                rendered.push(context.to_string());
            }
            // State-receiver ABI (#494 P1b): an unbound stateful
            // transactor's method takes the calling instance's per-instance
            // state struct by reference as the leading arg, so `a.go()` and
            // `b.go()` mutate their own state through one shared body.
            if uses_state_receiver(xschema) {
                let explicit = cx
                    .bindings
                    .testbench_transactor_states
                    .and_then(|bindings| {
                        bindings.iter().find(|binding| {
                            binding.field == *bus_field && binding.transactor == *transactor
                        })
                    })
                    .map(|binding| binding.receiver.clone());
                let receiver = explicit
                    .or_else(|| {
                        owner
                            .unbound_state_actors
                            .iter()
                            .find(|actor| {
                                actor.field == *bus_field && actor.transactor == *transactor
                            })
                            .map(|actor| {
                                if cx.bindings.durable_callbacks {
                                    format!("_harc_run_state.{}", actor.storage)
                                } else {
                                    actor.storage.clone()
                                }
                            })
                    })
                    .ok_or_else(|| {
                        EmitError(format!(
                            "tbir: stateful TransactorCall `{bus_field}.{method}` in {} has no typed state receiver",
                            func.name
                        ))
                    })?;
                rendered.push(receiver);
            }
            if cx.bindings.run_context.is_some() && xschema.bound_bus.is_some() {
                let bound = owner
                    .bound_bus_instances
                    .iter()
                    .find(|instance| {
                        instance.field == *bus_field
                            && instance.owner
                                == crate::ir::BoundBusOwner::Transactor(*transactor)
                    })
                    .and_then(|instance| owner.bus_binding(instance.binding))
                    .ok_or_else(|| {
                        EmitError(format!(
                            "tbir: common TransactorCall `{bus_field}.{method}` in {} has no explicit bound-bus adapter",
                            func.name
                        ))
                    })?;
                rendered.extend(testbench_bus_adapter_args(cx, *function, Some(bound))?);
            }
            for (index, a) in args.iter().enumerate() {
                rendered.push(scalar_sink_expr_cpp(
                    cx,
                    a,
                    method_schema.param_tys.get(index).cloned(),
                )?);
            }
            let invoke = format!("{xname}_{method}({})", rendered.join(", "));
            match dest {
                Some(d) => {
                    writeln!(out, "{pad}{} = {invoke};", &names[d.index()]).ok();
                }
                None => {
                    writeln!(out, "{pad}{invoke};").ok();
                }
            }
        }
        Stmt::TransactorSelfCall { dest, call } => {
            let Expr::Call(
                CallTarget::TransactorSelfMethod {
                    transactor,
                    transactor_name,
                    method,
                    function,
                },
                args,
            ) = call
            else {
                return Err(EmitError(format!(
                    "tbir: TransactorSelfCall in {} carries a non-self-call payload \
                     (verifier invariant violated)",
                    func.name
                )));
            };
            let schema = prog.transactors.get(transactor.index()).ok_or_else(|| {
                EmitError(format!(
                    "tbir: TransactorSelfCall in {} references missing transactor x{}",
                    func.name, transactor.0
                ))
            })?;
            if schema.name != *transactor_name {
                return Err(EmitError(format!(
                    "tbir: TransactorSelfCall `{}.{method}` in {} carries stale typed callable fn{}",
                    transactor_name, func.name, function.0
                )));
            }
            let Some(method_schema) = schema
                .method(method)
                .filter(|candidate| candidate.function == *function)
            else {
                return Err(EmitError(format!(
                    "tbir: TransactorSelfCall `{}.{method}` in {} carries stale typed callable fn{}",
                    transactor_name, func.name, function.0
                )));
            };
            let mut rendered = Vec::with_capacity(args.len() + 3);
            if let Some(context) = cx.bindings.run_context {
                rendered.push(context.to_string());
            }
            // Same-type self-call: forward the current state receiver so the
            // callee mutates the SAME per-instance struct (#494 P1b). A
            // self-call only appears inside a method body, and a stateful
            // type's methods all carry `self_state`, so the presence of a
            // receiver in scope tracks the callee's ABI exactly.
            if let Some(recv) = cx.state_receiver {
                rendered.push(recv.to_string());
            }
            if cx.bindings.run_context.is_some() && schema.bound_bus.is_some() {
                rendered.extend(testbench_bus_adapter_args(cx, *function, None)?);
            }
            for (index, a) in args.iter().enumerate() {
                rendered.push(scalar_sink_expr_cpp(
                    cx,
                    a,
                    method_schema.param_tys.get(index).cloned(),
                )?);
            }
            let invoke = format!("{}_{method}({})", schema.name, rendered.join(", "));
            match dest {
                Some(d) => {
                    writeln!(out, "{pad}{} = {invoke};", &names[d.index()]).ok();
                }
                None => {
                    writeln!(out, "{pad}{invoke};").ok();
                }
            }
        }
        Stmt::RecordFieldWrite {
            local,
            field,
            path,
            mid_indices,
            index,
            value,
        } => {
            // `rec.a.b = value`, `rec.data[i] = value` (`std::array`
            // element store), or a mid-chain element write
            // (`tbl.entries[i].tag = value`) — one shared chain renderer
            // with the `Expr::RecordField` read side.
            let name = local_cpp_name(cx, *local)?;
            let destination = Expr::RecordField {
                local: *local,
                field: field.clone(),
                path: path.clone(),
                mid_indices: mid_indices.clone(),
                index: index.clone().map(Box::new),
            };
            let e = scalar_sink_expr_cpp(cx, value, scalar_shape_type(cx, &destination))?;
            let dst = super::expr::record_access_cpp(
                cx,
                &name,
                field,
                path,
                mid_indices,
                index.as_ref(),
            )?;
            writeln!(out, "{pad}{dst} = {e};").ok();
        }
        Stmt::RecordRead {
            dest,
            local,
            regblock,
            addr,
        } => {
            let dst = &names[dest.index()];
            let mirror = &names[local.index()];
            let addr = expr_cpp(cx, addr)?;
            let schema = &prog.regblocks[regblock.index()];
            writeln!(out, "{pad}{{").ok();
            let p1 = INDENT.repeat(depth + 1);
            writeln!(out, "{p1}uint64_t _rec_addr = (uint64_t)({addr});").ok();
            writeln!(out, "{p1}{dst} = 0;").ok();
            for (i, reg) in schema.registers.iter().enumerate() {
                let kw = if i == 0 { "if" } else { "else if" };
                writeln!(
                    out,
                    "{p1}{kw} (_rec_addr == {offset}ull) {{ {dst} = {mirror}.{field}; }}",
                    offset = reg.offset,
                    field = reg.name,
                )
                .ok();
            }
            writeln!(out, "{pad}}}").ok();
        }
        Stmt::RecordWrite {
            local,
            binding,
            regblock,
            addr,
            value,
        } => {
            let mirror = &names[local.index()];
            let addr = expr_cpp(cx, addr)?;
            let value = expr_cpp(cx, value)?;
            let schema = &prog.regblocks[regblock.index()];
            let binding_schema = func
                .owner
                .and_then(|owner| prog.testbenches.get(owner.index()))
                .and_then(|tb| {
                    tb.regblock_bindings
                        .iter()
                        .find(|b| b.field == *binding && b.regblock == *regblock)
                })
                .ok_or_else(|| {
                    EmitError(format!(
                        "tbir: RecordWrite binding `{binding}` and regblock in {} do not resolve through its owner testbench",
                        func.name
                    ))
                })?;
            writeln!(out, "{pad}{{").ok();
            let p1 = INDENT.repeat(depth + 1);
            let p2 = INDENT.repeat(depth + 2);
            writeln!(out, "{p1}uint64_t _rec_addr = (uint64_t)({addr});").ok();
            writeln!(out, "{p1}uint64_t _rec_data = (uint64_t)({value});").ok();
            let has_callbacks = !binding_schema.callbacks.is_empty();
            if has_callbacks {
                writeln!(
                    out,
                    "{p1}if ({binding}_cb_depth >= HARC_RAL_CB_MAX_DEPTH) {{ \
                     sim_log_line(\"FATAL\", \"RAL record_write callback recursion exceeded \
                     HARC_RAL_CB_MAX_DEPTH (%u) on binding `{binding}` at addr 0x%llx\", \
                     (unsigned)HARC_RAL_CB_MAX_DEPTH, (unsigned long long)_rec_addr); \
                     ctx.errors++; _fatal = true; }} else {{"
                )
                .ok();
                writeln!(out, "{p2}{binding}_cb_depth++;").ok();
            }
            let decode_pad = if has_callbacks { &p2 } else { &p1 };
            for (i, reg) in schema.registers.iter().enumerate() {
                let kw = if i == 0 { "if" } else { "else if" };
                let mask = if reg.width >= 64 {
                    u64::MAX
                } else {
                    (1u64 << reg.width) - 1
                };
                let callback = binding_schema
                    .callbacks
                    .iter()
                    .find(|(field, _)| field == &reg.name)
                    .map(|(_, fid)| format!(" {}(_rec_data);", prog.function(*fid).name))
                    .unwrap_or_default();
                writeln!(
                    out,
                    "{decode_pad}{kw} (_rec_addr == {offset}ull) {{ {mirror}.{field} = _rec_data & 0x{mask:x}ull;{callback} }}",
                    offset = reg.offset,
                    field = reg.name,
                )
                .ok();
            }
            if has_callbacks {
                writeln!(out, "{p2}{binding}_cb_depth--;").ok();
                writeln!(out, "{p1}}}").ok();
            }
            writeln!(out, "{pad}}}").ok();
        }
        Stmt::RecordWriteCb {
            local,
            binding,
            field,
            offset,
            value,
            mask,
            callback,
        } => {
            // Passive RAL `record_write` with a per-register write callback
            // registered on the binding: mirror update wrapped in the
            // recursion-depth guard, then the callback fires with the
            // observed value. Mirrors v1's `try_emit_record_write`
            // (`<binding>_cb_depth` / `HARC_RAL_CB_MAX_DEPTH`); the FATAL
            // message uses the const-decoded `at addr 0x..` to match v1.
            let name = local_cpp_name(cx, *local)?;
            let destination = Expr::RecordField {
                local: *local,
                field: field.clone(),
                path: Vec::new(),
                mid_indices: Vec::new(),
                index: None,
            };
            let v = scalar_sink_expr_cpp(cx, value, scalar_shape_type(cx, &destination))?;
            writeln!(out, "{pad}{{").ok();
            let p1 = INDENT.repeat(depth + 1);
            let p2 = INDENT.repeat(depth + 2);
            writeln!(
                out,
                "{p1}if ({binding}_cb_depth >= HARC_RAL_CB_MAX_DEPTH) {{ \
                 sim_log_line(\"FATAL\", \"RAL record_write callback recursion exceeded \
                 HARC_RAL_CB_MAX_DEPTH (%u) on binding `{binding}` at addr 0x%llx\", \
                 (unsigned)HARC_RAL_CB_MAX_DEPTH, (unsigned long long){offset}ull); \
                 ctx.errors++; _fatal = true; }} else {{"
            )
            .ok();
            writeln!(out, "{p2}{binding}_cb_depth++;").ok();
            writeln!(out, "{p2}uint64_t _rec_data = (uint64_t)({v});").ok();
            // Mask only the mirror store; the callback sees the raw value
            // (v1 / runtime `RecordWrite` parity — see `RecordWriteCb` doc).
            writeln!(out, "{p2}{name}.{field} = _rec_data & 0x{mask:x}ull;").ok();
            if let Some(fid) = callback {
                let cb_name = test_hook_cpp_name(cx, *fid)?;
                writeln!(out, "{p2}{cb_name}(_rec_data);").ok();
            }
            writeln!(out, "{p2}{binding}_cb_depth--;").ok();
            writeln!(out, "{p1}}}").ok();
            writeln!(out, "{pad}}}").ok();
        }
        Stmt::TbFieldWrite { field, value } => {
            let destination = Expr::TbField(field.clone());
            let e = scalar_sink_expr_cpp(cx, value, scalar_shape_type(cx, &destination))?;
            let receiver = required_testbench_receiver(cx, "testbench field write")?;
            writeln!(out, "{pad}{receiver}.{field} = {e};").ok();
        }
        // `_tb.mem[i] = v` (and `_tb.mem[i][j] = v` for a nested Vec) —
        // element write of a fixed-vector testbench field. Mirrors v1's
        // subscript assignment on the `std::array` member.
        Stmt::TbFieldVecElementWrite {
            field,
            index,
            inner_index,
            value,
        } => {
            let idx = expr_cpp(cx, index)?;
            let destination = Expr::TbFieldVecElement {
                field: field.clone(),
                index: Box::new(index.clone()),
                inner_index: inner_index.clone().map(Box::new),
            };
            let value = scalar_sink_expr_cpp(cx, value, scalar_shape_type(cx, &destination))?;
            let mut member = format!(
                "{}.{field}[{idx}]",
                required_testbench_receiver(cx, "testbench vector write")?
            );
            if let Some(inner) = inner_index {
                member = format!("{member}[{}]", expr_cpp(cx, inner)?);
            }
            writeln!(out, "{pad}{member} = {value};").ok();
        }
        Stmt::TbQueuePush { field, value } => {
            let destination = owner_tb(cx)
                .and_then(|testbench| {
                    testbench
                        .queue_fields
                        .iter()
                        .find(|queue| queue.name == *field)
                })
                .map(|queue| queue.elem.ir_type());
            let e = scalar_sink_expr_cpp(cx, value, destination)?;
            let receiver = required_testbench_receiver(cx, "testbench queue push")?;
            writeln!(out, "{pad}{receiver}.{field}.push({e});").ok();
        }
        Stmt::TbQueuePop { field, dest } => {
            let name = &names[dest.index()];
            let receiver = required_testbench_receiver(cx, "testbench queue pop")?;
            writeln!(out, "{pad}{name} = {receiver}.{field}.pop();").ok();
        }
        Stmt::TransactorStateWrite {
            instance,
            field,
            value,
        } => {
            let recv = super::expr::resolve_state_instance(cx, instance)?;
            let destination = Expr::TransactorState {
                instance: instance.clone(),
                field: field.clone(),
            };
            let e = scalar_sink_expr_cpp(cx, value, scalar_shape_type(cx, &destination))?;
            writeln!(out, "{pad}{recv}.{field} = {e};").ok();
        }
        // `last.addr = addr` on a bound-to target transactor whole-record
        // state field — a nested member of the value-record struct on the
        // per-instance struct (`<instance>.<field>.<path…> = <value>`).
        Stmt::TransactorStateRecordFieldWrite {
            instance,
            field,
            path,
            mid_indices,
            index,
            value,
        } => {
            // Through the same receiver resolver the READ side and the
            // scalar write already use. Written raw, an empty instance —
            // which is what a transactor's own method body carries for a
            // self-reference — emitted a leading-dot `.cur.tag = 5;`.
            let recv = super::expr::resolve_state_instance(cx, instance)?;
            let destination = Expr::TransactorStateRecordField {
                instance: instance.clone(),
                field: field.clone(),
                path: path.clone(),
                mid_indices: mid_indices.clone(),
                index: index.clone().map(Box::new),
            };
            let e = scalar_sink_expr_cpp(cx, value, scalar_shape_type(cx, &destination))?;
            let recv = format!("{recv}.{field}");
            let dst = if path.is_empty() {
                let Some(index) = index.as_ref() else {
                    return Err(EmitError(format!(
                        "tbir: fixed-vector state write `{field}` lacks an index"
                    )));
                };
                let mut access = recv;
                for (_, mid) in mid_indices {
                    access.push_str(&format!("[{}]", expr_cpp(cx, mid)?));
                }
                format!("{access}[{}]", expr_cpp(cx, index)?)
            } else {
                super::expr::record_access_cpp(
                    cx,
                    &recv,
                    &path[0],
                    &path[1..],
                    mid_indices,
                    index.as_ref(),
                )?
            };
            writeln!(out, "{pad}{dst} = {e};").ok();
        }
        // `pending.push(value)` on a bound-to target transactor `queue<T>`
        // state field — a `harc_rt::HarcQueue<T>` member of the per-
        // instance struct. Mirrors `ComponentQueuePush`.
        Stmt::TransactorStateQueuePush {
            instance,
            field,
            value,
        } => {
            let recv = super::expr::resolve_state_instance(cx, instance)?;
            let destination = super::expr::state_transactor(cx, instance)
                .and_then(|transactor| {
                    transactor
                        .state_fields
                        .iter()
                        .find(|state| state.name == *field)
                })
                .and_then(|state| match &state.kind {
                    crate::ir::StateFieldKind::Queue { elem } => Some(elem.ir_type()),
                    _ => None,
                });
            let e = scalar_sink_expr_cpp(cx, value, destination)?;
            writeln!(out, "{pad}{recv}.{field}.push({e});").ok();
        }
        // `let v = pending.pop()` — pop the state-queue front into a local.
        Stmt::TransactorStateQueuePop {
            instance,
            field,
            dest,
        } => {
            let recv = super::expr::resolve_state_instance(cx, instance)?;
            let name = &names[dest.index()];
            writeln!(out, "{pad}{name} = {recv}.{field}.pop();").ok();
        }
        Stmt::DutWrite(p, e) if matches!(p.access, crate::ir::PortAccess::Force) => {
            // `dut.<force_probe> = expr` → the two-store drv+en pair the
            // bound SV stub picks up to procedurally force the target
            // (docs/probe-signals.md §4.1; v1's `emit_signal_assignment`
            // force-probe arm). The read-side mangled accessor is the
            // base; `_drv`/`_en` derive by suffix.
            let base = super::expr::port_signal(cx, p)?;
            if let Some(words) = wide_words_over_128(e) {
                let req = words
                    .iter()
                    .rposition(|word| *word != 0)
                    .map_or(1, |i| i + 1);
                let list = words
                    .iter()
                    .map(|word| format!("0x{word:x}u"))
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(
                    out,
                    "{pad}harc_rt::harc_assign_words_checked<{req}>({base}_drv, {{{list}}});"
                )
                .ok();
            } else {
                let val = match super::expr::port_value_type(cx, p)? {
                    Some(destination) => scalar_assignment_expr_cpp(cx, e, &destination)?,
                    None => expr_cpp(cx, e)?,
                };
                writeln!(out, "{pad}harc_rt::harc_assign({base}_drv, {val});").ok();
            }
            writeln!(out, "{pad}{base}_en = 1;").ok();
        }
        Stmt::DutWrite(p, e) => {
            let sig = port_signal(cx, p)?;
            match &p.lane {
                // Packed multi-lane port: bit-deposit through the
                // runtime helper; unpacked-array port: raw subscript
                // (v1's emit_signal_assignment split). The lane index is
                // a constant or a runtime expression — v1 re-renders an
                // arbitrary `&Expr` here.
                Some(lane) => {
                    let idx = lane_index_cpp(cx, lane)?;
                    let e = match super::expr::port_value_type(cx, p)? {
                        Some(destination) => scalar_assignment_expr_cpp(cx, e, &destination)?,
                        None => expr_cpp(cx, e)?,
                    };
                    match lane_width(cx, p) {
                        Some(w) => {
                            writeln!(
                                out,
                                "{pad}harc_rt::harc_vec_lane_write<{w}>({sig}, \
                                 (std::size_t)({idx}), {e});"
                            )
                            .ok();
                        }
                        None => {
                            writeln!(out, "{pad}{sig}[{idx}] = {e};").ok();
                        }
                    }
                }
                None => {
                    // > 128-bit literal into a wide signal: word-list
                    // path through the checked helper, parameterized
                    // by the words the value actually needs (v1).
                    if let Some(words) = wide_words_over_128(e) {
                        let req = words.iter().rposition(|w| *w != 0).map_or(1, |i| i + 1);
                        let list = words
                            .iter()
                            .map(|w| format!("0x{w:x}u"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        writeln!(
                            out,
                            "{pad}harc_rt::harc_assign_words_checked<{req}>({sig}, {{{list}}});"
                        )
                        .ok();
                    } else {
                        let e = match super::expr::port_value_type(cx, p)? {
                            Some(destination) => scalar_assignment_expr_cpp(cx, e, &destination)?,
                            None => expr_cpp(cx, e)?,
                        };
                        writeln!(out, "{pad}harc_rt::harc_assign({sig}, {e});").ok();
                    }
                }
            }
        }
        Stmt::DutRead(l, p) => {
            let name = &names[l.index()];
            if matches!(cx.func.locals[l.index()].ty, IrType::PortSnapshot) {
                let sampled = if snapshot_preserves_port_shape(cx.func, *l) {
                    format!("harc_rt::harc_port_snapshot({})", port_signal(cx, p)?)
                } else {
                    port_read(cx, p)?
                };
                writeln!(out, "{pad}{name} = {sampled};").ok();
            } else {
                let destination = super::expr::local_ir_type(cx, *l).ok_or_else(|| {
                    EmitError(format!(
                        "tbir: DUT read targets missing local %{} in {}",
                        l.0, cx.func.name
                    ))
                })?;
                let value = scalar_assignment_expr_cpp(cx, &Expr::Port(p.clone()), destination)?;
                writeln!(out, "{pad}{name} = {value};").ok();
            }
        }
        Stmt::ProbeRelease(p) => {
            // `release dut.<force_probe>` → clear the enable wire so the
            // bound SV stub releases its procedural force (v1's `release`
            // → `<mangled>_en = 0`). Lowering guaranteed `access == Force`.
            let base = super::expr::port_signal(cx, p)?;
            writeln!(out, "{pad}{base}_en = 0;").ok();
        }
        Stmt::Log { level, args } => {
            let (sev, file) = match level {
                LogLevel::Debug => ("DEBUG", None),
                LogLevel::Info => ("INFO", None),
                LogLevel::Warn => ("WARN", None),
                LogLevel::Error => ("ERROR", None),
                LogLevel::Fatal => ("FATAL", None),
                LogLevel::File { path, level } => (
                    match level {
                        FileLogLevel::Debug => "DEBUG",
                        FileLogLevel::Info => "INFO",
                        FileLogLevel::Warn => "WARN",
                        FileLogLevel::Error => "ERROR",
                        FileLogLevel::Fatal => "FATAL",
                    },
                    Some(path.as_str()),
                ),
            };
            emit_log_call(out, cx, sev, file, args, depth)?;
            // Spec §7.7 test-result semantics (mirrors v1's emit_log).
            match sev {
                "ERROR" => {
                    writeln!(out, "{pad}ctx.errors++;").ok();
                }
                "FATAL" => {
                    writeln!(out, "{pad}ctx.errors++; _fatal = true;").ok();
                }
                _ => {}
            }
        }
        Stmt::AssertCheck { cond, on_fail } => {
            let cond = truthy_expr_cpp(cx, cond)?;
            writeln!(out, "{pad}if (!({cond})) {{").ok();
            emit_log_call(out, cx, "FAIL", None, on_fail, depth + 1)?;
            writeln!(out, "{pad}{INDENT}ctx.errors++;").ok();
            writeln!(out, "{pad}}}").ok();
        }
        // Same guard shape as `AssertCheck`, minus the error bump — an
        // assumption bounds the inputs, it does not fail the test.
        Stmt::AssumeCheck { cond, on_fail } => {
            let cond = truthy_expr_cpp(cx, cond)?;
            writeln!(out, "{pad}if (!({cond})) {{").ok();
            emit_log_call(out, cx, "ASSUME", None, on_fail, depth + 1)?;
            writeln!(out, "{pad}}}").ok();
        }
        Stmt::CovReport(inst) => {
            let testbench = cx.bindings.testbench_receiver.unwrap_or("_tb");
            writeln!(out, "{pad}{testbench}.{}.report(log_ctx);", inst.tb_field).ok();
        }
        Stmt::PropertyCheck(p) => {
            let schema = prog.property_checks.get(p.index()).ok_or_else(|| {
                EmitError(format!(
                    "tbir: {} references missing property check p{}",
                    func.name, p.0
                ))
            })?;
            emit_property_check(out, cx, *p, schema, depth)?;
        }
        Stmt::CoverCheck(c) => {
            let schema = prog.cover_checks.get(c.index()).ok_or_else(|| {
                EmitError(format!(
                    "tbir: {} references missing cover check c{}",
                    func.name, c.0
                ))
            })?;
            emit_cover_check(out, cx, *c, schema, depth)?;
        }
        // v1's shape exactly: a subscriber is a closure pushed onto the
        // channel vector, and `emit` calls each subscriber synchronously
        // in subscription order. The subscriber BODY is a separate
        // test-scope lambda (like a cycle handler's) so the pushed
        // closure outlives the block that registered it.
        Stmt::EventSubscribe { event, handler, .. } => {
            let body = test_hook_cpp_name(cx, *handler)?;
            let (chan, ty) = match event {
                crate::ir::EventChannelRef::Local(event) => (
                    names[event.index()].clone(),
                    event_payload_cty(prog, cx, *event)?,
                ),
                crate::ir::EventChannelRef::Component {
                    base,
                    event,
                    payload,
                    ..
                } => (
                    format!("{}.{event}", comp_base_cpp_subst_cx(cx, base)?),
                    super::runtime::event_payload_cty(payload, records),
                ),
            };
            if cx.bindings.durable_callbacks {
                let capture = persistent_callback_capture(cx);
                writeln!(out, "{pad}{chan}.push_back([{capture}]({ty} _p) {{").ok();
                emit_persistent_callback_bindings(out, cx, depth + 1);
                writeln!(out, "{pad}{INDENT}{body}(_p);").ok();
                writeln!(out, "{pad}}});").ok();
            } else {
                writeln!(
                    out,
                    "{pad}{chan}.push_back([&]({ty} _p) {{ {body}(_p); }});"
                )
                .ok();
            }
        }
        Stmt::MethodHookSubscribe {
            target,
            side,
            handler,
            captures,
            ..
        } => {
            let body = prog.functions.get(handler.index()).ok_or_else(|| {
                EmitError(format!(
                    "tbir: {} subscribes method hook to fn{} which does not resolve",
                    func.name, handler.0
                ))
            })?;
            let side = match side {
                crate::ast::HookSide::Pre => "pre",
                crate::ast::HookSide::Post => "post",
            };
            let vector = match target {
                crate::ir::MethodHookTarget::Transactor {
                    field,
                    transactor,
                    method,
                } => {
                    let schema = prog.transactors.get(transactor.index()).ok_or_else(|| {
                        EmitError(format!(
                            "tbir: method hook references missing transactor x{}",
                            transactor.0
                        ))
                    })?;
                    let owner = func.owner.ok_or_else(|| {
                        EmitError(format!(
                            "tbir: transactor method hook in {} has no testbench owner",
                            func.name
                        ))
                    })?;
                    let testbench = prog.testbenches.get(owner.index()).ok_or_else(|| {
                        EmitError(format!(
                            "tbir: transactor method hook in {} references missing testbench tb{}",
                            func.name, owner.0
                        ))
                    })?;
                    let state = testbench
                        .unbound_state_actors
                        .iter()
                        .find(|actor| actor.field == *field && actor.transactor == *transactor)
                        .ok_or_else(|| {
                            EmitError(format!(
                                "tbir: transactor method hook `{field}.{method}` in {} has no typed instance hook state",
                                func.name
                            ))
                        })?;
                    let hook = super::runtime::transactor_hook_field(schema, method, side);
                    format!("{}.{hook}", state.storage)
                }
                crate::ir::MethodHookTarget::Component {
                    base,
                    component,
                    method,
                } => {
                    let schema = prog.components.get(component.index()).ok_or_else(|| {
                        EmitError(format!(
                            "tbir: method hook references missing component c{}",
                            component.0
                        ))
                    })?;
                    let field = super::runtime::component_internal_member_name(
                        schema,
                        &format!("_harc_hook_{method}_{side}"),
                    );
                    format!("{}.{field}", comp_base_cpp_subst_cx(cx, base)?)
                }
            };
            let method_param_count =
                body.params
                    .len()
                    .checked_sub(captures.len())
                    .ok_or_else(|| {
                        EmitError(format!(
                            "tbir: method hook fn{} has fewer parameters than captures",
                            handler.0
                        ))
                    })?;
            let mut decls = Vec::with_capacity(method_param_count);
            let mut args = Vec::with_capacity(body.params.len());
            for (i, local) in body.locals.iter().take(method_param_count).enumerate() {
                let name = format!("_h{i}");
                decls.push(format!("{} {name}", hook_param_cty(prog, &local.ty)?));
                args.push(name);
            }
            for capture in captures {
                let name = names.get(capture.index()).ok_or_else(|| {
                    EmitError(format!(
                        "tbir: method hook captures missing local %{} in {}",
                        capture.0, func.name
                    ))
                })?;
                args.push(name.clone());
            }
            let handler = test_hook_cpp_name(cx, *handler)?;
            if cx.bindings.durable_callbacks {
                let capture = persistent_callback_capture(cx);
                writeln!(
                    out,
                    "{pad}{vector}.push_back([{capture}]({}) {{",
                    decls.join(", ")
                )
                .ok();
                emit_persistent_callback_bindings(out, cx, depth + 1);
                writeln!(out, "{pad}{INDENT}{handler}({});", args.join(", ")).ok();
                writeln!(out, "{pad}}});").ok();
            } else {
                writeln!(
                    out,
                    "{pad}{vector}.push_back([&]({}) {{ {handler}({}); }});",
                    decls.join(", "),
                    args.join(", ")
                )
                .ok();
            }
        }
        Stmt::EventEmit { event, args } => {
            let chan = &names[event.index()];
            let destination = local_ir_type(cx, *event).and_then(|ty| match ty {
                IrType::Event(payload) => Some(event_payload_ir_type(payload.clone())),
                _ => None,
            });
            let mut rendered = Vec::with_capacity(args.len());
            for a in args {
                rendered.push(scalar_sink_expr_cpp(cx, a, destination.clone())?);
            }
            writeln!(
                out,
                "{pad}for (auto& _s : {chan}) _s({});",
                rendered.join(", ")
            )
            .ok();
        }
        Stmt::CycleHandler(h) => {
            let schema = prog.cycle_handlers.get(h.index()).ok_or_else(|| {
                EmitError(format!(
                    "tbir: {} references missing cycle handler h{}",
                    func.name, h.0
                ))
            })?;
            emit_cycle_handler(out, cx, *h, schema, depth)?;
        }
        Stmt::FailDiag { guard, args } => match guard {
            // Per-sub-predicate breakdown: log only if the predicate
            // is STILL false at timeout (v1's "not yet true:" lines).
            // No errors++ — the WaitUntilTimeout terminator already
            // bumped it once on the timeout edge.
            Some(g) => {
                let g = truthy_expr_cpp(cx, g)?;
                writeln!(out, "{pad}if (!({g})) {{").ok();
                emit_log_call(out, cx, "FAIL", None, args, depth + 1)?;
                writeln!(out, "{pad}}}").ok();
            }
            None => emit_log_call(out, cx, "FAIL", None, args, depth)?,
        },
        Stmt::ScoreboardOp {
            sb,
            field,
            op,
            nested_path,
        } => {
            use crate::ir::ScoreboardOp;
            // `None` → testbench field (`_tb.<field>`); `Some(path)` →
            // env-nested data scoreboard, accessed by the run-scope path.
            // A `self`-rooted path is a self-relative sub-scoreboard inside
            // a component body — re-root `self` at the running instance via
            // `self_subst` (the cycle-trigger / on-handler poke form).
            let base = match nested_path {
                Some(p) if p.first().map(String::as_str) == Some("self") => {
                    let root = cx.bindings.self_receiver.ok_or_else(|| {
                        EmitError(format!(
                            "tbir: self-owned scoreboard write in {} has no typed receiver",
                            func.name
                        ))
                    })?;
                    std::iter::once(root.to_string())
                        .chain(p.iter().skip(1).cloned())
                        .collect::<Vec<_>>()
                        .join(".")
                }
                Some(p) => p.join("."),
                None => format!(
                    "{}.{}",
                    cx.bindings.testbench_receiver.ok_or_else(|| {
                        EmitError(format!(
                            "tbir: testbench scoreboard write `{field}` in {} has no typed receiver",
                            func.name
                        ))
                    })?,
                    field
                ),
            };
            match op {
                ScoreboardOp::QueuePush { queue, value } => {
                    let elem_ty = prog
                        .scoreboards
                        .get(sb.index())
                        .and_then(|schema| schema.fields.iter().find(|f| f.name == *queue))
                        .and_then(|field| match &field.kind {
                            crate::ir::ScoreboardFieldKind::Queue {
                                elem: crate::ir::QueueElem::Scalar { ty },
                            } => Some(ty),
                            _ => None,
                        });
                    let e = match elem_ty {
                        Some(ty) => scalar_assignment_expr_cpp(cx, value, ty)?,
                        None => expr_cpp(cx, value)?,
                    };
                    writeln!(out, "{pad}{base}.{queue}.push({e});").ok();
                }
                ScoreboardOp::QueuePop { queue, dest } => {
                    let name = &names[dest.index()];
                    writeln!(out, "{pad}{name} = {base}.{queue}.pop();").ok();
                }
                ScoreboardOp::ScalarWrite { scalar, value } => {
                    let kind = prog
                        .scoreboards
                        .get(sb.index())
                        .and_then(|schema| schema.fields.iter().find(|f| f.name == *scalar))
                        .map(|field| &field.kind)
                        .ok_or_else(|| {
                            EmitError(format!(
                                "tbir: scoreboard {} field `{scalar}` has no value schema",
                                sb.0
                            ))
                        })?;
                    let e = match kind {
                        crate::ir::ScoreboardFieldKind::Scalar { ty, .. } => {
                            scalar_assignment_expr_cpp(cx, value, ty)?
                        }
                        crate::ir::ScoreboardFieldKind::Record { .. } => expr_cpp(cx, value)?,
                        crate::ir::ScoreboardFieldKind::Queue { .. } => {
                            return Err(EmitError(format!(
                                "tbir: scoreboard {} field `{scalar}` is a queue",
                                sb.0
                            )))
                        }
                        crate::ir::ScoreboardFieldKind::List { .. } => {
                            return Err(EmitError(format!(
                                "tbir: scoreboard {} field `{scalar}` is a list",
                                sb.0
                            )))
                        }
                    };
                    writeln!(out, "{pad}{base}.{scalar} = {e};").ok();
                }
            }
        }
        // Composite-component scalar field write — `self.count = ...`
        // inside a method body, or `env.sb.errors = ...` from the test.
        Stmt::ComponentFieldWrite { base, field, value } => {
            let destination = Expr::ComponentField {
                base: base.clone(),
                field: field.clone(),
            };
            let e = scalar_sink_expr_cpp(cx, value, scalar_shape_type(cx, &destination))?;
            // cx-aware so a `self.<field>` write re-roots at the running
            // instance under `cx.self_subst` (the `--mt` active-driver worker
            // coroutine has no `self` parameter). Byte-identical to the bare
            // `comp_base_cpp` when `self_subst` is `None` (every prior site).
            writeln!(
                out,
                "{pad}{}.{field} = {e};",
                comp_base_cpp_subst_cx(cx, base)?
            )
            .ok();
        }
        Stmt::ComponentVecElementWrite {
            base,
            field,
            index_pos,
            index,
            inner_index,
            value,
        } => {
            let destination = Expr::ComponentVecElement {
                base: base.clone(),
                field: field.clone(),
                index_pos: *index_pos,
                index: Box::new(index.clone()),
                inner_index: inner_index.clone().map(Box::new),
            };
            let value = scalar_sink_expr_cpp(cx, value, scalar_shape_type(cx, &destination))?;
            let index = expr_cpp(cx, index)?;
            let mut field = super::expr::indexed_member_cpp(field, *index_pos, &index);
            if let Some(inner) = inner_index {
                field = format!("{field}[{}]", expr_cpp(cx, inner)?);
            }
            writeln!(
                out,
                "{pad}{}.{field} = {value};",
                comp_base_cpp_subst_cx(cx, base)?
            )
            .ok();
        }
        // `emit observed(v)` inside a method body: fan the args out to
        // every subscriber registered on `self.<event>`, then bump the
        // component's `_last_out_cycle` heartbeat (v1's emit lowering).
        Stmt::ComponentEmit {
            base,
            subpath,
            event,
            args,
        } => {
            let (_, component) = component_subpath_schema(cx, base, subpath)?;
            let destination = component.field(event).and_then(|field| match &field.kind {
                crate::ir::ComponentFieldKind::Event { payload } => {
                    Some(event_payload_ir_type(payload.clone()))
                }
                _ => None,
            });
            let mut rendered = Vec::with_capacity(args.len());
            for a in args {
                rendered.push(scalar_sink_expr_cpp(cx, a, destination.clone())?);
            }
            let csv = rendered.join(", ");
            let recv = std::iter::once(comp_base_cpp_subst_cx(cx, base)?)
                .chain(subpath.iter().cloned())
                .collect::<Vec<_>>()
                .join(".");
            let heartbeat = super::runtime::component_heartbeat_field(
                component,
                crate::ir::passes::runtime_cells::ComponentHeartbeat::Output,
            );
            writeln!(out, "{pad}for (auto& _s : {recv}.{event}) _s({csv});").ok();
            writeln!(out, "{pad}{recv}.{heartbeat} = (uint64_t)cycle_count;").ok();
        }
        // `env.source.publish(3)` — a free `<Comp>_<method>(receiver,
        // args)` lambda call (v1's `emit_component_method` shape).
        Stmt::ComponentCall {
            base,
            component,
            method,
            function,
            args,
            dest,
        } => {
            let comp = prog.components.get(component.index()).ok_or_else(|| {
                EmitError(format!(
                    "tbir: ComponentCall in {} references missing component c{}",
                    func.name, component.0
                ))
            })?;
            let Some(method_schema) = comp
                .method(method)
                .filter(|schema| schema.function == *function)
            else {
                return Err(EmitError(format!(
                    "tbir: ComponentCall `{}.{method}` in {} carries stale callable fn{}",
                    comp.name, func.name, function.0
                )));
            };
            let mut rendered = Vec::with_capacity(args.len() + 2);
            if let Some(context) = cx.bindings.run_context {
                rendered.push(context.to_string());
            }
            rendered.push(comp_base_cpp_subst_cx(cx, base)?);
            let concrete_binding = component_bound_bus_binding(cx, base, *component)?;
            rendered.extend(testbench_bus_adapter_args(cx, *function, concrete_binding)?);
            for (index, a) in args.iter().enumerate() {
                rendered.push(scalar_sink_expr_cpp(
                    cx,
                    a,
                    method_schema.param_tys.get(index).cloned(),
                )?);
            }
            let invoke = format!("{}_{method}({})", comp.name, rendered.join(", "));
            match dest {
                Some(d) => {
                    writeln!(out, "{pad}{} = {invoke};", &names[d.index()]).ok();
                }
                None => {
                    writeln!(out, "{pad}{invoke};").ok();
                }
            }
        }
        Stmt::TestbenchCall {
            function,
            args,
            dut_args,
            dest,
        } => {
            let callee = prog.functions.get(function.index()).ok_or_else(|| {
                EmitError(format!(
                    "tbir: TestbenchCall in {} references missing function fn{}",
                    func.name, function.0
                ))
            })?;
            let FunctionKind::TestbenchMethod { testbench, .. } = callee.kind else {
                return Err(EmitError(format!(
                    "tbir: TestbenchCall in {} targets non-testbench function fn{}",
                    func.name, function.0
                )));
            };
            let schema = prog.testbench_types.get(testbench.index()).ok_or_else(|| {
                EmitError(format!(
                    "tbir: TestbenchCall in {} references missing testbench type tbt{}",
                    func.name, testbench.0
                ))
            })?;
            let method = schema
                .methods
                .iter()
                .find(|method| method.function == *function)
                .ok_or_else(|| {
                    EmitError(format!(
                        "tbir: TestbenchCall in {} has no owner entry for fn{}",
                        func.name, function.0
                    ))
                })?;
            let mut rendered = Vec::with_capacity(args.len() + 2);
            if let Some(context) = cx.bindings.run_context {
                rendered.push(context.to_string());
            }
            rendered.push(
                cx.bindings
                    .testbench_receiver
                    .ok_or_else(|| {
                        EmitError(format!(
                            "tbir: TestbenchCall in {} has no typed testbench receiver",
                            func.name
                        ))
                    })?
                    .to_string(),
            );
            if cx.bindings.run_context.is_some() {
                for (field, component) in &schema.component_fields {
                    let receiver = cx
                        .bindings
                        .testbench_components
                        .and_then(|bindings| {
                            bindings.iter().find(|binding| {
                                binding.field == *field && binding.component == *component
                            })
                        })
                        .map(|binding| binding.receiver.clone())
                        .ok_or_else(|| {
                            EmitError(format!(
                                "tbir: TestbenchCall in {} has no typed binding for component field `{field}` c{}",
                                func.name, component.0
                            ))
                        })?;
                    rendered.push(receiver);
                }
                for (field, transactor) in testbench_method_transactor_state_fields(prog, callee)? {
                    let receiver = cx
                        .bindings
                        .testbench_transactor_states
                        .and_then(|bindings| {
                            bindings.iter().find(|binding| {
                                binding.field == field && binding.transactor == transactor
                            })
                        })
                        .map(|binding| binding.receiver.clone())
                        .ok_or_else(|| {
                            EmitError(format!(
                                "tbir: TestbenchCall in {} has no typed state binding for transactor field `{field}` x{}",
                                func.name, transactor.0
                            ))
                        })?;
                    rendered.push(receiver);
                }
            }
            rendered.extend(testbench_bus_adapter_args(cx, *function, None)?);
            for (index, arg) in args.iter().enumerate() {
                if dut_args.contains(&index) {
                    let receiver = match arg {
                        Expr::Literal {
                            value: 0,
                            ty: IrType::Unknown,
                        } => super::expr::dut_receiver(cx)?.to_string(),
                        Expr::Local(local) => super::expr::local_cpp_name(cx, *local)?,
                        _ => {
                            return Err(EmitError(format!(
                            "tbir: TestbenchCall in {} has a non-canonical typed DUT argument {}",
                            func.name,
                            index + 1
                        )))
                        }
                    };
                    rendered.push(receiver);
                } else {
                    rendered.push(scalar_sink_expr_cpp(
                        cx,
                        arg,
                        callee
                            .params
                            .get(index)
                            .map(|parameter| parameter.ty.clone()),
                    )?);
                }
            }
            let invoke = format!("{}_{}({})", schema.name, method.name, rendered.join(", "));
            match dest {
                Some(dest) => {
                    writeln!(out, "{pad}{} = {invoke};", &names[dest.index()]).ok();
                }
                None => {
                    writeln!(out, "{pad}{invoke};").ok();
                }
            }
        }
        // `<recv>.<queue>.push(value)` on a composite-component `queue<T>`
        // field — `self.errors.push(err)` inside a method body, or
        // `checker.sb.errors.push(e)` from the test. Mirrors v1's
        // `HarcQueue::push`.
        Stmt::ComponentQueuePush { base, queue, value } => {
            let destination = component_base_schema(cx, base)?
                .field(queue)
                .and_then(|field| match &field.kind {
                    crate::ir::ComponentFieldKind::Queue { elem } => Some(elem.ir_type()),
                    _ => None,
                });
            let e = scalar_sink_expr_cpp(cx, value, destination)?;
            writeln!(
                out,
                "{pad}{}.{queue}.push({e});",
                comp_base_cpp_subst_cx(cx, base)?
            )
            .ok();
        }
        // `let v = <recv>.<queue>.pop()` — pop the queue front into a local.
        Stmt::ComponentQueuePop { base, queue, dest } => {
            let name = &names[dest.index()];
            writeln!(
                out,
                "{pad}{name} = {}.{queue}.pop();",
                comp_base_cpp_subst_cx(cx, base)?
            )
            .ok();
        }
        // `<dst>.<field> = <src>` — whole sub-component value copy. Both
        // bases use the context-aware renderer because method parameters and
        // self-relative paths are legal on this statement.
        Stmt::ComponentSubAssign { dst, field, src } => {
            let parent = component_base_schema(cx, dst)?;
            let component = parent
                .field(field)
                .and_then(|schema| match schema.kind {
                    crate::ir::ComponentFieldKind::Sub { component, .. } => Some(component),
                    _ => None,
                })
                .and_then(|component| prog.components.get(component.index()))
                .ok_or_else(|| {
                    EmitError(format!(
                        "tbir: component copy destination field `{field}` in {} does not resolve",
                        func.name
                    ))
                })?;
            let copy = super::runtime::component_copy_method_name(component);
            writeln!(
                out,
                "{pad}{}.{field}.{copy}({});",
                comp_base_cpp_subst_cx(cx, dst)?,
                comp_base_cpp_subst_cx(cx, src)?
            )
            .ok();
        }
        // `<dst> = <src>` — whole component value copy into a direct
        // testbench component binding.
        Stmt::ComponentAssign { dst, src } => {
            let component = component_base_schema(cx, dst)?;
            let copy = super::runtime::component_copy_method_name(component);
            writeln!(
                out,
                "{pad}{}.{copy}({});",
                comp_base_cpp_subst_cx(cx, dst)?,
                comp_base_cpp_subst_cx(cx, src)?
            )
            .ok();
        }
        // `yield t` — append a record value to the sequence accumulator
        // (v1's `_result.push_back(t)`).
        Stmt::SeqPush { seq, value } => {
            let name = &names[seq.index()];
            let destination = local_ir_type(cx, *seq).and_then(|ty| match ty {
                IrType::RecordSeq(record) => Some(IrType::Record(*record)),
                IrType::Seq(element) => Some((**element).clone()),
                _ => None,
            });
            let v = scalar_sink_expr_cpp(cx, value, destination)?;
            writeln!(out, "{pad}{name}.push_back({v});").ok();
        }
        Stmt::TlmFork(desc) => emit_tlm_fork(out, cx, bindings, desc, depth)?,
        Stmt::TlmJoinAll(pending) => emit_tlm_join_all(out, cx, records, bindings, pending, depth)?,
    }
    Ok(())
}

fn component_bound_bus_binding<'a>(
    cx: &'a ECx<'a>,
    base: &crate::ir::ComponentBase,
    component: crate::ir::ComponentId,
) -> Result<Option<&'a BusBindingSchema>, EmitError> {
    use crate::ir::ComponentBase;

    if matches!(base, ComponentBase::SelfField)
        || matches!(base, ComponentBase::Path(path) if path.first().map(String::as_str) == Some("self"))
    {
        return Ok(cx.bindings.bound_bus);
    }
    let ComponentBase::Path(path) = base else {
        return Ok(None);
    };
    let Some(field) = path.first() else {
        return Err(EmitError(format!(
            "tbir: empty component path in {}",
            cx.func.name
        )));
    };
    let Some(testbench) = super::expr::owner_tb(cx) else {
        return Ok(None);
    };
    Ok(testbench
        .bound_bus_instances
        .iter()
        .find(|instance| {
            instance.field == *field
                && instance.owner == crate::ir::BoundBusOwner::Component(component)
        })
        .and_then(|instance| testbench.bus_binding(instance.binding)))
}

pub(super) fn testbench_bus_adapter_args(
    cx: &ECx<'_>,
    function: FunctionId,
    concrete_binding: Option<&BusBindingSchema>,
) -> Result<Vec<String>, EmitError> {
    let Some(adapters) = cx.bindings.bus_adapters else {
        return Ok(Vec::new());
    };
    let Some(adapter) = adapters
        .callables
        .iter()
        .find(|adapter| adapter.function == function)
    else {
        return Ok(Vec::new());
    };
    let bindings = cx.bindings.testbench_bus_bindings.ok_or_else(|| {
        EmitError(format!(
            "tbir: call to fn{} from {} has no concrete testbench bus bindings",
            function.0, cx.func.name
        ))
    })?;
    render_bus_adapter_args(
        cx.prog.ok_or_else(|| {
            EmitError(format!(
                "tbir: call to fn{} from {} has no program type catalog",
                function.0, cx.func.name
            ))
        })?,
        adapter,
        adapters.current,
        bindings,
        concrete_binding,
        super::expr::dut_receiver(cx)?,
        &cx.func.name,
    )
}

pub(super) fn transactor_call_context_args(
    cx: &ECx<'_>,
    bus_field: &str,
    transactor: crate::ir::TransactorId,
    function: FunctionId,
) -> Result<Vec<String>, EmitError> {
    let prog = cx.prog.ok_or_else(|| {
        EmitError(format!(
            "tbir: transactor call `{bus_field}` in {} has no program type catalog",
            cx.func.name
        ))
    })?;
    let schema = prog.transactors.get(transactor.index()).ok_or_else(|| {
        EmitError(format!(
            "tbir: transactor call `{bus_field}` in {} references missing transactor x{}",
            cx.func.name, transactor.0
        ))
    })?;
    let owner = super::expr::owner_tb(cx).ok_or_else(|| {
        EmitError(format!(
            "tbir: transactor call `{bus_field}` in {} has no owning testbench",
            cx.func.name
        ))
    })?;
    let mut rendered = Vec::new();
    if let Some(context) = cx.bindings.run_context {
        rendered.push(context.to_string());
    }
    if uses_state_receiver(schema) {
        let explicit = cx
            .bindings
            .testbench_transactor_states
            .and_then(|bindings| {
                bindings
                    .iter()
                    .find(|binding| binding.field == bus_field && binding.transactor == transactor)
            })
            .map(|binding| binding.receiver.clone());
        let receiver = explicit
            .or_else(|| {
                owner
                    .unbound_state_actors
                    .iter()
                    .find(|actor| actor.field == bus_field && actor.transactor == transactor)
                    .map(|actor| {
                        if cx.bindings.durable_callbacks {
                            format!("_harc_run_state.{}", actor.storage)
                        } else {
                            actor.storage.clone()
                        }
                    })
            })
            .ok_or_else(|| {
                EmitError(format!(
                    "tbir: stateful transactor call `{bus_field}` in {} has no typed state receiver",
                    cx.func.name
                ))
            })?;
        rendered.push(receiver);
    }
    if cx.bindings.run_context.is_some() && schema.bound_bus.is_some() {
        let bound = owner
            .bound_bus_instances
            .iter()
            .find(|instance| {
                instance.field == bus_field
                    && instance.owner == crate::ir::BoundBusOwner::Transactor(transactor)
            })
            .and_then(|instance| owner.bus_binding(instance.binding))
            .ok_or_else(|| {
                EmitError(format!(
                    "tbir: common transactor call `{bus_field}` in {} has no explicit bound-bus adapter",
                    cx.func.name
                ))
            })?;
        rendered.extend(testbench_bus_adapter_args(cx, function, Some(bound))?);
    }
    Ok(rendered)
}

pub(super) fn component_callable_bus_adapter_args(
    prog: &TbProgram,
    function: FunctionId,
    all_adapters: &[TestbenchBusAdapterPlan],
    bound_bus: Option<&BusBindingSchema>,
    dut_receiver: &str,
    caller: &str,
) -> Result<Vec<String>, EmitError> {
    let Some(adapter) = all_adapters
        .iter()
        .find(|adapter| adapter.function == function)
    else {
        return Ok(Vec::new());
    };
    render_bus_adapter_args(prog, adapter, None, &[], bound_bus, dut_receiver, caller)
}

fn render_bus_adapter_args(
    prog: &TbProgram,
    adapter: &TestbenchBusAdapterPlan,
    current: Option<&TestbenchBusAdapterPlan>,
    bindings: &[BusBindingSchema],
    concrete_binding: Option<&BusBindingSchema>,
    dut_receiver: &str,
    caller: &str,
) -> Result<Vec<String>, EmitError> {
    let mut rendered = Vec::with_capacity(adapter.signals.len());
    for signal in &adapter.signals {
        if let Some(current) = current.and_then(|current| {
            current
                .signals
                .iter()
                .find(|candidate| candidate.matches(&signal.field, &signal.channel, &signal.signal))
        }) {
            rendered.push(current.symbol.clone());
            continue;
        }
        let binding = concrete_binding
            .or_else(|| {
                bindings
                    .iter()
                    .find(|binding| binding.field == signal.field)
            })
            .ok_or_else(|| {
                EmitError(format!(
                    "tbir: call to fn{} from {} has no binding for logical bus field `{}`",
                    adapter.function.0, caller, signal.field
                ))
            })?;
        let physical = if signal.channel.is_empty() {
            format!("{}_{}", binding.field, signal.signal)
        } else {
            binding.wire_name(&signal.channel, &signal.signal)
        };
        let wire = format!("{dut_receiver}->{physical}");
        let ty = callable_value_cty(prog, &signal.ty)?;
        let read = typed_wire_read_expr(prog, &wire, &signal.ty)?;
        let write = typed_wire_write_stmt(prog, &wire, &signal.ty, "value")?;
        rendered.push(format!(
            "harc_rt::HarcBusSignalRef<{ty}>{{[&]() -> {ty} {{ return {read}; }}, [&](const {ty}& value) {{ {write} }}}}"
        ));
    }
    Ok(rendered)
}

fn typed_wire_read_expr(prog: &TbProgram, wire: &str, ty: &IrType) -> Result<String, EmitError> {
    Ok(match ty {
        IrType::Bool => format!("(harc_rt::harc_read({wire}) != 0)"),
        IrType::UInt(Some(width)) if *width <= 64 => {
            format!("static_cast<uint64_t>(harc_rt::harc_read({wire}))")
        }
        IrType::SInt(Some(width)) if *width <= 64 => format!(
            "static_cast<int64_t>(harc_rt::harc_sext_u128(static_cast<_harc_u128>(harc_rt::harc_read({wire})), {width}, 64))"
        ),
        IrType::UInt(Some(width)) | IrType::SInt(Some(width)) if *width <= 128 => {
            format!("static_cast<_harc_u128>(harc_rt::harc_read({wire}))")
        }
        IrType::UInt(Some(width)) | IrType::SInt(Some(width)) => format!(
            "harc_rt::harc_wide_trunc<{}>(harc_rt::harc_read({wire}), {width})",
            width.div_ceil(32)
        ),
        IrType::Record(record) => format!(
            "harc_unpack_{}({wire})",
            prog.records
                .get(record.index())
                .ok_or_else(|| EmitError(format!(
                    "tbir: typed wire access references missing record r{}",
                    record.0
                )))?
                .name
        ),
        other => {
            return Err(EmitError(format!(
                "tbir: typed wire access cannot represent signal type {other:?}"
            )))
        }
    })
}

fn typed_wire_write_stmt(
    prog: &TbProgram,
    wire: &str,
    ty: &IrType,
    value: &str,
) -> Result<String, EmitError> {
    Ok(match ty {
        IrType::Record(record) => format!(
            "harc_drive_{}({wire}, {value});",
            prog.records
                .get(record.index())
                .ok_or_else(|| EmitError(format!(
                    "tbir: typed wire access references missing record r{}",
                    record.0
                )))?
                .name
        ),
        _ => format!("harc_rt::harc_assign({wire}, {value});"),
    })
}

/// Function-scope exact C++ carriers used by ordered message capture.
/// Definitions are discovered from the unique Assign/DutRead that initializes
/// each internal `PortSnapshot` local. Declaring after ordinary locals and in
/// LocalId order makes every name referenced by `decltype(rhs)` visible while
/// keeping the carrier alive across loop-switch case edges.
fn declare_port_snapshots(out: &mut String, cx: &ECx<'_>, depth: usize) -> Result<(), EmitError> {
    let pad = INDENT.repeat(depth);
    for (index, local) in cx.func.locals.iter().enumerate() {
        if !matches!(local.ty, IrType::PortSnapshot) {
            continue;
        }
        let id = LocalId(index as u32);
        let mut init = None;
        for stmt in cx.func.blocks.iter().flat_map(|block| &block.stmts) {
            match stmt {
                Stmt::DutRead(dest, port) if *dest == id => {
                    let candidate = if snapshot_preserves_port_shape(cx.func, id) {
                        format!("harc_rt::harc_port_snapshot({})", port_signal(cx, port)?)
                    } else {
                        port_read(cx, port)?
                    };
                    if init.replace(candidate).is_some() {
                        return Err(EmitError(format!(
                            "tbir: ordered snapshot local `{}` has multiple definitions",
                            cx.names[index]
                        )));
                    }
                }
                Stmt::Assign(dest, expr) if *dest == id => {
                    let candidate = expr_cpp(cx, expr)?;
                    if init.replace(candidate).is_some() {
                        return Err(EmitError(format!(
                            "tbir: ordered snapshot local `{}` has multiple definitions",
                            cx.names[index]
                        )));
                    }
                }
                _ => {}
            }
        }
        let init = init.ok_or_else(|| {
            EmitError(format!(
                "tbir: ordered snapshot local `{}` has no defining read/assignment",
                cx.names[index]
            ))
        })?;
        writeln!(
            out,
            "{pad}decltype({init}) {}{{}}; (void){};",
            cx.names[index], cx.names[index]
        )
        .ok();
    }
    Ok(())
}

fn snapshot_preserves_port_shape(func: &TbFunction, snapshot: LocalId) -> bool {
    func.blocks
        .iter()
        .flat_map(|block| &block.stmts)
        .any(|stmt| match stmt {
            Stmt::Assign(_, expr) => expr_uses_snapshot_lane(expr, snapshot),
            _ => false,
        })
}

fn expr_uses_snapshot_lane(expr: &Expr, snapshot: LocalId) -> bool {
    let mut found = false;
    crate::ir::visit::walk_expr(expr, &mut |expr| {
        if matches!(
            expr,
            Expr::PortSnapshotLane {
                snapshot: used, ..
            } if *used == snapshot
        ) {
            found = true;
        }
    });
    found
}

/// Expand one `fork bus.<method>(args)` request issue (v1's
/// `try_emit_bus_tlm_fork`, coroutine path): the dest local was hoisted +
/// zero-init at the function head, so this emits ONLY the request side
/// (drive arg wires + optional req_tag, raise `req_valid`, budget-wait
/// `req_ready`, tick, drop `req_valid`). The response is drained at the
/// matching `Stmt::TlmJoinAll`. The `request` trace event carries the OOO
/// tag when present, matching v1's payload so `harc trace-diff` is clean.
fn emit_tlm_fork(
    out: &mut String,
    cx: &ECx<'_>,
    bindings: &[BusBindingSchema],
    desc: &crate::ir::TlmForkDesc,
    depth: usize,
) -> Result<(), EmitError> {
    let pad = INDENT.repeat(depth);
    let advance = tlm_cycle_advance(cx)?;
    let binding = resolve_tlm_binding(bindings, cx, &desc.bus_field, &desc.method, &desc.target)?;
    let wire = |sig: &str| tlm_wire(cx, binding, &desc.bus_field, &desc.method, sig);
    writeln!(out, "{pad}// fork bus.{} tlm_method issue", desc.method).ok();
    let schema = binding
        .methods
        .iter()
        .find(|m| m.name == desc.method)
        .ok_or_else(|| {
            EmitError(format!(
                "tbir: fork `{}.{}` arity drift in {}",
                desc.bus_field, desc.method, cx.func.name
            ))
        })?;
    if schema.args.len() != desc.args.len() {
        return Err(EmitError(format!(
            "tbir: fork `{}.{}` arity drift ({} schema vs {} call args) in {}",
            desc.bus_field,
            desc.method,
            schema.args.len(),
            desc.args.len(),
            cx.func.name
        )));
    }
    for (arg_name, arg) in schema.args.iter().zip(desc.args.iter()) {
        let v = expr_cpp(cx, arg)?;
        writeln!(out, "{pad}harc_rt::harc_assign({}, {v});", wire(arg_name)).ok();
    }
    if let Some(tag) = desc.tag {
        writeln!(out, "{pad}{} = {tag};", wire("req_tag")).ok();
    }
    let tag_arg = desc.tag.map(|t| format!("(int64_t){t}"));
    emit_tlm_trace(
        out,
        &pad,
        cx.trace_component,
        &desc.bus_field,
        &desc.method,
        "request",
        "initiator",
        tag_arg.as_deref(),
    );
    writeln!(out, "{pad}{} = 1;", wire("req_valid")).ok();
    if desc.tag.is_some() {
        // Present the request (valid + tag + payload) for exactly the
        // acceptance cycle, then deassert. An out-of-order target accepts
        // combinationally — `req_ready` is high while the addressed tag's slot
        // is idle and drops the same posedge the request is latched. We must
        // NOT spin on `req_ready` before advancing: the DUT mirror has not
        // re-evaluated with the just-written `req_tag`, so `req_ready` still
        // reflects the *previous* tag (a now-busy slot) — a stale 0 that would
        // hold `req_valid` asserted through a `valid && !ready` window and drop
        // it while the slot is busy, tripping the DUT's `_auto_tlm_*_req_stable`
        // handshake assertion. Advancing exactly one cycle spans the accept
        // edge with the new tag/payload held stable; deasserting the next cycle
        // keeps the request out of any stalled window. (Mirrors v1's
        // `try_emit_bus_tlm_fork`; response drained at join_all.)
        writeln!(out, "{pad}{advance};").ok();
    } else {
        // Blocking forks still need the legacy ready-wait: unlike tagged OOO
        // lanes, a blocking target may legitimately hold req_ready low for
        // multiple cycles before accepting the request.
        writeln!(
            out,
            "{pad}{}",
            crate::codegen::bounded_handshake_wait(
                &wire("req_ready"),
                crate::codegen::TLM_WAIT_BOUND,
                &advance,
                &format!("TLM {}.{} fork request", desc.bus_field, desc.method),
            )
        )
        .ok();
        writeln!(out, "{pad}{advance};").ok();
    }
    writeln!(out, "{pad}{} = 0;", wire("req_valid")).ok();
    Ok(())
}

/// Drain the pending forks at a `join_all` (v1's `emit_tlm_join_all`):
/// all-untagged forks route by issue order (FIFO), all-tagged forks by
/// `rsp_tag` match. An empty list is a no-op. Lowering already rejected a
/// mixed tagged/untagged set, so a single `tagged` test suffices.
fn emit_tlm_join_all(
    out: &mut String,
    cx: &ECx<'_>,
    records: &[RecordSchema],
    bindings: &[BusBindingSchema],
    pending: &[crate::ir::TlmForkDesc],
    depth: usize,
) -> Result<(), EmitError> {
    let pad = INDENT.repeat(depth);
    if pending.is_empty() {
        writeln!(out, "{pad}// join_all: no pending forked TLM calls").ok();
        return Ok(());
    }
    let tagged = pending.iter().any(|p| p.tag.is_some());
    if tagged {
        emit_tagged_tlm_join_all(out, cx, records, bindings, pending, depth)
    } else {
        emit_ordered_tlm_join_all(out, cx, records, bindings, pending, depth)
    }
}

/// Issue-order FIFO drain for `blocking` forks (v1's
/// `emit_ordered_tlm_join_all`): for each pending fork in issue order,
/// raise `rsp_ready`, budget-wait `rsp_valid` (64-cycle), capture
/// `rsp_data` into the dest, tick, drop `rsp_ready`.
fn emit_ordered_tlm_join_all(
    out: &mut String,
    cx: &ECx<'_>,
    records: &[RecordSchema],
    bindings: &[BusBindingSchema],
    pending: &[crate::ir::TlmForkDesc],
    depth: usize,
) -> Result<(), EmitError> {
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let names = cx.names;
    let advance = tlm_cycle_advance(cx)?;
    for p in pending {
        let binding = resolve_tlm_binding(bindings, cx, &p.bus_field, &p.method, &p.target)?;
        let wire = |sig: &str| tlm_wire(cx, binding, &p.bus_field, &p.method, sig);
        writeln!(out, "{pad}// join_all bus.{} response", p.method).ok();
        writeln!(out, "{pad}{} = 1;", wire("rsp_ready")).ok();
        writeln!(out, "{pad}{{",).ok();
        writeln!(out, "{pad1}bool _rsp_ok = true;").ok();
        writeln!(
            out,
            "{pad1}{}",
            crate::codegen::bounded_handshake_wait_into(
                &wire("rsp_valid"),
                crate::codegen::TLM_JOIN_DRAIN_BOUND,
                &advance,
                &format!("TLM {}.{} fork response", p.bus_field, p.method),
                "_rsp_ok",
            )
        )
        .ok();
        writeln!(out, "{pad1}if (_rsp_ok) {{").ok();
        if let (Some(dest), true) = (p.dest, p.has_ret) {
            let capture = tlm_capture_expr(
                cx,
                records,
                dest,
                &wire("rsp_data"),
                super::expr::bus_adapter_signal(cx, &p.bus_field, &p.method, "rsp_data").is_some(),
            )?;
            writeln!(out, "{pad2}{} = {capture};", names[dest.index()]).ok();
        }
        emit_tlm_trace(
            out,
            &pad2,
            cx.trace_component,
            &p.bus_field,
            &p.method,
            "response",
            "initiator",
            None,
        );
        writeln!(out, "{pad1}}}").ok();
        writeln!(out, "{pad}}}").ok();
        writeln!(out, "{pad}{advance};").ok();
        writeln!(out, "{pad}{} = 0;", wire("rsp_ready")).ok();
    }
    Ok(())
}

/// Multi-lane tag-routed drain for `out_of_order tags N` forks (v1's
/// `emit_tagged_tlm_join_all`): poll every lane each tick, accepting any
/// response whose `rsp_tag` matches a not-yet-seen fork's tag (so tag 1
/// can land before tag 0). Captures into each dest, ticks once per poll
/// round, and bounds the wait with a 256-cycle budget.
fn emit_tagged_tlm_join_all(
    out: &mut String,
    cx: &ECx<'_>,
    records: &[RecordSchema],
    bindings: &[BusBindingSchema],
    pending: &[crate::ir::TlmForkDesc],
    depth: usize,
) -> Result<(), EmitError> {
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let pad3 = INDENT.repeat(depth + 3);
    let names = cx.names;
    let advance = tlm_cycle_advance(cx)?;
    writeln!(out, "{pad}{{").ok();
    writeln!(out, "{pad1}int _tlm_pending = {};", pending.len()).ok();
    for idx in 0..pending.len() {
        writeln!(out, "{pad1}bool _tlm_seen_{idx} = false;").ok();
    }
    writeln!(out, "{pad1}int _tlm_budget = 256;").ok();
    writeln!(out, "{pad1}while (_tlm_pending > 0 && _tlm_budget > 0) {{").ok();
    for p in pending {
        let binding = resolve_tlm_binding(bindings, cx, &p.bus_field, &p.method, &p.target)?;
        writeln!(
            out,
            "{pad2}{} = 0;",
            tlm_wire(cx, binding, &p.bus_field, &p.method, "rsp_ready")
        )
        .ok();
    }
    writeln!(out, "{pad2}bool _tlm_accept = false;").ok();
    for (idx, p) in pending.iter().enumerate() {
        let tag = p.tag.ok_or_else(|| {
            EmitError(format!(
                "tbir: tagged join_all in {} carries an untagged fork \
                 (lowering should have rejected the mix)",
                cx.func.name
            ))
        })?;
        let binding = resolve_tlm_binding(bindings, cx, &p.bus_field, &p.method, &p.target)?;
        let wire = |sig: &str| tlm_wire(cx, binding, &p.bus_field, &p.method, sig);
        writeln!(
            out,
            "{pad2}if (!_tlm_seen_{idx} && {} && {} == {tag}) {{",
            wire("rsp_valid"),
            wire("rsp_tag")
        )
        .ok();
        if let (Some(dest), true) = (p.dest, p.has_ret) {
            let capture = tlm_capture_expr(
                cx,
                records,
                dest,
                &wire("rsp_data"),
                super::expr::bus_adapter_signal(cx, &p.bus_field, &p.method, "rsp_data").is_some(),
            )?;
            writeln!(out, "{pad3}{} = {capture};", names[dest.index()]).ok();
        }
        emit_tlm_trace(
            out,
            &pad3,
            cx.trace_component,
            &p.bus_field,
            &p.method,
            "response",
            "initiator",
            Some(&format!("(int64_t){tag}")),
        );
        writeln!(out, "{pad3}{} = 1;", wire("rsp_ready")).ok();
        writeln!(out, "{pad3}_tlm_seen_{idx} = true;").ok();
        writeln!(out, "{pad3}_tlm_pending--;").ok();
        writeln!(out, "{pad3}_tlm_accept = true;").ok();
        writeln!(out, "{pad2}}}").ok();
    }
    writeln!(out, "{pad2}{advance};").ok();
    writeln!(out, "{pad2}if (!_tlm_accept) _tlm_budget--;").ok();
    writeln!(out, "{pad1}}}").ok();
    writeln!(out, "{pad1}if (_tlm_pending > 0) {{").ok();
    writeln!(
        out,
        "{pad2}sim_log_line(\"FAIL\", \"TLM fork join_all timed out after 256 cycles waiting for a matching response tag\");"
    )
    .ok();
    writeln!(out, "{pad2}ctx.errors++;").ok();
    writeln!(out, "{pad1}}}").ok();
    for p in pending {
        let binding = resolve_tlm_binding(bindings, cx, &p.bus_field, &p.method, &p.target)?;
        writeln!(
            out,
            "{pad1}{} = 0;",
            tlm_wire(cx, binding, &p.bus_field, &p.method, "rsp_ready")
        )
        .ok();
    }
    writeln!(out, "{pad}}}").ok();
    Ok(())
}

/// The C++ expression that captures a TLM response pin (`rsp_data`)
/// into a value-returning call's `dest` local. A record-typed dest is
/// bit-unpacked through the record's generated `harc_unpack_<R>` helper
/// (v1's `record_unpack_expr`); any other type is a plain scalar read.
fn tlm_capture_expr(
    cx: &ECx<'_>,
    _records: &[RecordSchema],
    dest: crate::ir::LocalId,
    raw_wire: &str,
    typed_adapter: bool,
) -> Result<String, EmitError> {
    let ty = cx
        .func
        .locals
        .get(dest.index())
        .map(|local| &local.ty)
        .ok_or_else(|| {
            EmitError(format!(
                "tbir: TLM response in {} references missing local %{}",
                cx.func.name, dest.0
            ))
        })?;
    if typed_adapter {
        return Ok(format!("{raw_wire}.harc_read()"));
    }
    typed_wire_read_expr(
        cx.prog.ok_or_else(|| {
            EmitError(format!(
                "tbir: TLM response in {} has no program type catalog",
                cx.func.name
            ))
        })?,
        raw_wire,
        ty,
    )
}

/// Resolve a bus binding for a fork/join wire emission, with the same
/// "verifier should have rejected it" hard-error contract as
/// `emit_transactor_call`.
fn resolve_tlm_binding<'a>(
    bindings: &'a [BusBindingSchema],
    cx: &ECx<'a>,
    bus_field: &str,
    method: &str,
    target: &crate::ir::TransactorMethodTarget,
) -> Result<&'a BusBindingSchema, EmitError> {
    match target {
        crate::ir::TransactorMethodTarget::ConcreteBusBinding { binding, field } => {
            if field != bus_field {
                return Err(EmitError(format!(
                    "tbir: fork/join `{bus_field}.{method}` in {} carries concrete binding field `{field}`",
                    cx.func.name
                )));
            }
            let resolved = bindings.get(binding.index()).ok_or_else(|| {
                EmitError(format!(
                    "tbir: unresolved fork/join binding bb{} for `{bus_field}.{method}` in {}",
                    binding.0, cx.func.name
                ))
            })?;
            if resolved.field != bus_field {
                return Err(EmitError(format!(
                    "tbir: fork/join `{bus_field}.{method}` in {} names bb{} owned by `{}`",
                    cx.func.name, binding.0, resolved.field
                )));
            }
            Ok(resolved)
        }
        crate::ir::TransactorMethodTarget::TestbenchBusField {
            testbench,
            field,
            bus,
        } => {
            if field != bus_field
                || !matches!(
                    cx.func.kind,
                    FunctionKind::TestbenchMethod {
                        testbench: owner,
                        ..
                    } if owner == *testbench
                )
            {
                return Err(EmitError(format!(
                    "tbir: fork/join `{bus_field}.{method}` in {} carries a mismatched reusable testbench binding",
                    cx.func.name
                )));
            }
            let binding = bindings
                .iter()
                .find(|binding| binding.field == *field)
                .ok_or_else(|| {
                    EmitError(format!(
                        "tbir: fork/join `{bus_field}.{method}` in {} has no typed binding for reusable testbench field `{field}`",
                        cx.func.name
                    ))
                })?;
            if binding.bus != *bus {
                return Err(EmitError(format!(
                    "tbir: fork/join `{bus_field}.{method}` in {} resolves `{field}` as bus `{}`, expected `{bus}`",
                    cx.func.name, binding.bus
                )));
            }
            Ok(binding)
        }
        crate::ir::TransactorMethodTarget::BoundBus => bindings
            .iter()
            .find(|binding| binding.field == bus_field)
            .or(cx.bindings.bound_bus)
            .ok_or_else(|| {
                EmitError(format!(
                    "tbir: bound-bus fork/join `{bus_field}.{method}` in {} has no callable adapter",
                    cx.func.name
                ))
            }),
        crate::ir::TransactorMethodTarget::Callable { .. } => Err(EmitError(format!(
            "tbir: callable edge `{bus_field}.{method}` in {} reached the bus protocol renderer",
            cx.func.name
        ))),
    }
}

fn tlm_wire(
    cx: &ECx<'_>,
    binding: &BusBindingSchema,
    bus_field: &str,
    method: &str,
    signal: &str,
) -> String {
    super::expr::bus_adapter_signal(cx, bus_field, method, signal)
        .map(str::to_string)
        .unwrap_or_else(|| format!("dut->{}", binding.wire_name(method, signal)))
}

/// One `trace.tlm_call(...)` line, with the OOO tag appended when present
/// (mirrors v1's `emit_tlm_call_trace_event`). `component` is empty at
/// test-run scope, or the responder-instance name when the fork/join is
/// emitted inside a bound-target responder body (nested fork-forwarding)
/// — matching v1's `current_component_instance` so the trace diffs clean.
#[allow(clippy::too_many_arguments)]
fn emit_tlm_trace(
    out: &mut String,
    pad: &str,
    component: &str,
    bus: &str,
    method: &str,
    phase: &str,
    direction: &str,
    tag: Option<&str>,
) {
    write!(
        out,
        "{pad}trace.tlm_call(cycle_count, \"{}\", \"{}\", \"{}\", \"{phase}\", \"{direction}\"",
        escape_c(component),
        escape_c(bus),
        escape_c(method)
    )
    .ok();
    if let Some(t) = tag {
        write!(out, ", {t}").ok();
    }
    writeln!(out, ");").ok();
}

/// Expand one sanctioned bus-bound `TransactorMethod` call edge into v1's
/// blocking req/rsp wire protocol (cpp_tb `try_emit_bus_tlm_method`,
/// coroutine path): drive arg wires, raise `req_valid`, budget-wait
/// for `req_ready`, tick, drop `req_valid`; raise `rsp_ready`,
/// budget-wait for `rsp_valid`, capture `rsp_data` (value-returning
/// methods), tick, drop `rsp_ready`. Trace events bracket the wire
/// activity with the same `tlm_call` payloads v1 records (component
/// context is empty — test-run scope), so `harc trace-diff` sees
/// identical request/response edges.
#[allow(clippy::too_many_arguments)]
fn emit_transactor_call(
    out: &mut String,
    cx: &ECx<'_>,
    records: &[RecordSchema],
    bindings: &[BusBindingSchema],
    dest: crate::ir::LocalId,
    bus_field: &str,
    method: &str,
    target: &crate::ir::TransactorMethodTarget,
    args: &[Expr],
    depth: usize,
) -> Result<(), EmitError> {
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let func = cx.func;
    let names = cx.names;
    let advance = tlm_cycle_advance(cx)?;
    let binding = resolve_tlm_binding(bindings, cx, bus_field, method, target)?;
    let schema = binding
        .methods
        .iter()
        .find(|m| m.name == method)
        .ok_or_else(|| {
            EmitError(format!(
                "tbir: unresolved transactor call `{bus_field}.{method}` in {} — \
             verifier should have rejected it",
                func.name
            ))
        })?;
    if schema.args.len() != args.len() {
        return Err(EmitError(format!(
            "tbir: `{bus_field}.{method}` arity drift ({} schema vs {} call args) in {}",
            schema.args.len(),
            args.len(),
            func.name
        )));
    }
    // `bind ... with { method.sig: "port" }` remaps override the
    // `<field>_<method>_<sig>` flat-name convention (mirrors v1's
    // `bus_signal_name`).
    let wire = |sig: &str| tlm_wire(cx, binding, bus_field, method, sig);
    let budget_wait = |out: &mut String, sig: &str, label: &str| {
        writeln!(
            out,
            "{pad}{}",
            crate::codegen::bounded_handshake_wait(
                &wire(sig),
                crate::codegen::TLM_WAIT_BOUND,
                &advance,
                label,
            )
        )
        .ok();
    };
    let trace_event = |out: &mut String, phase: &str| {
        writeln!(
            out,
            "{pad}trace.tlm_call(cycle_count, \"{}\", \"{}\", \"{}\", \"{phase}\", \"initiator\");",
            escape_c(cx.trace_component),
            escape_c(bus_field),
            escape_c(method)
        )
        .ok();
    };

    writeln!(out, "{pad}// bus.{method} tlm_method").ok();
    for (arg_name, arg) in schema.args.iter().zip(args.iter()) {
        let v = expr_cpp(cx, arg)?;
        writeln!(out, "{pad}harc_rt::harc_assign({}, {v});", wire(arg_name)).ok();
    }
    trace_event(out, "request");
    writeln!(out, "{pad}{} = 1;", wire("req_valid")).ok();
    budget_wait(
        out,
        "req_ready",
        &format!("TLM {bus_field}.{method} request"),
    );
    writeln!(out, "{pad}{advance};").ok();
    writeln!(out, "{pad}{} = 0;", wire("req_valid")).ok();
    writeln!(out, "{pad}{} = 1;", wire("rsp_ready")).ok();
    writeln!(out, "{pad}{{").ok();
    writeln!(out, "{pad1}bool _rsp_ok = true;").ok();
    writeln!(
        out,
        "{pad1}{}",
        crate::codegen::bounded_handshake_wait_into(
            &wire("rsp_valid"),
            crate::codegen::TLM_WAIT_BOUND,
            &advance,
            &format!("TLM {bus_field}.{method} response"),
            "_rsp_ok",
        )
    )
    .ok();
    writeln!(out, "{pad1}if (_rsp_ok) {{").ok();
    writeln!(
        out,
        "{pad2}trace.tlm_call(cycle_count, \"{}\", \"{}\", \"{}\", \"response\", \"initiator\");",
        escape_c(cx.trace_component),
        escape_c(bus_field),
        escape_c(method)
    )
    .ok();
    if schema.has_ret {
        // Capture BEFORE the trailing tick — rsp_data is valid in the
        // same cycle as rsp_valid (mirrors v1; for result-less or
        // discarded calls v1 skips the capture but still completes the
        // rsp handshake). A record-typed return is bit-unpacked from the
        // lowered response pin (v1's `record_unpack_expr`).
        let capture = tlm_capture_expr(
            cx,
            records,
            dest,
            &wire("rsp_data"),
            super::expr::bus_adapter_signal(cx, bus_field, method, "rsp_data").is_some(),
        )?;
        writeln!(out, "{pad2}{} = {capture};", names[dest.index()]).ok();
    }
    writeln!(out, "{pad1}}}").ok();
    writeln!(out, "{pad}}}").ok();
    writeln!(out, "{pad}{advance};").ok();
    writeln!(out, "{pad}{} = 0;", wire("rsp_ready")).ok();
    Ok(())
}

fn tlm_cycle_advance(cx: &ECx<'_>) -> Result<String, EmitError> {
    if let Some(slot) = cx.bindings.actor_slot {
        return Ok(format!("co_await harc_rt::wait_cycles({slot}, 1)"));
    }
    match cx.func.kind {
        FunctionKind::TestBody { .. } => Ok("co_await harc_rt::wait_cycles(_slot, 1)".to_string()),
        FunctionKind::TestbenchMethod { .. }
        | FunctionKind::ComponentMethod { .. }
        | FunctionKind::TransactorBody { .. }
        | FunctionKind::TestHook { .. } => Ok("tick()".to_string()),
        ref kind => Err(EmitError(format!(
            "tbir: TLM call in fn{} `{}` has no cycle-advance binding for {kind:?}",
            cx.func.id.0, cx.func.name
        ))),
    }
}

/// Hoisted local declarations for a loop-switch body, skipping the
/// first `skip` locals (function parameters — they arrive as C++
/// parameters and must not be re-declared). Record-typed locals hoist
/// as default-constructed structs; the `RecordInit` at the source
/// `let` site re-runs the field defaults (v1 declares at the let
/// site, so loop iterations re-default-construct).
///
/// Names of record locals that are shared test-scope host state in the
/// self-contained layout. Typed testbench record fields use the explicit
/// receiver binding; callback-bearing register mirrors retain their local
/// compatibility mapping until their common-runtime ownership is modeled.
pub(super) fn shared_record_names(prog: &TbProgram, func: &TbFunction) -> HashSet<String> {
    let mut out = HashSet::new();
    let owner = func
        .owner
        .and_then(|owner| prog.testbenches.get(owner.index()))
        .or_else(|| {
            let FunctionKind::TestbenchMethod { testbench, .. } = func.kind else {
                return None;
            };
            prog.testbenches
                .iter()
                .find(|schema| schema.type_id == testbench)
        });
    if let Some(tb) = owner {
        for (field, _) in &tb.record_fields {
            out.insert(field.clone());
        }
        for b in &tb.regblock_bindings {
            out.insert(b.field.clone());
        }
    }
    out
}

fn declare_locals(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
    names: &[String],
    skip: usize,
    depth: usize,
    dut_access: Option<&crate::ir::passes::dut_access::DutAccessPlan>,
) -> Result<(), EmitError> {
    declare_locals_except(
        out,
        prog,
        func,
        names,
        skip,
        &HashSet::new(),
        depth,
        dut_access,
    )
}

fn declare_locals_except(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
    names: &[String],
    skip: usize,
    excluded: &HashSet<LocalId>,
    depth: usize,
    dut_access: Option<&crate::ir::passes::dut_access::DutAccessPlan>,
) -> Result<(), EmitError> {
    let pad = INDENT.repeat(depth);
    let shared = shared_record_names(prog, func);
    for (index, (l, n)) in func.locals.iter().zip(names).enumerate().skip(skip) {
        if excluded.contains(&LocalId(index as u32)) {
            continue;
        }
        // Shared test-scope record state is declared once by the enclosing
        // test; skip its per-function declaration so references resolve to
        // the captured object.
        if func
            .testbench_record_locals
            .iter()
            .any(|binding| binding.local.index() == index)
            || shared.contains(n)
        {
            continue;
        }
        if matches!(l.ty, IrType::PortSnapshot) {
            continue;
        }
        let ty = if matches!(l.ty, IrType::Unknown) {
            dut_access
                .and_then(|plan| plan.inferred_local_type(func.id, LocalId(index as u32)))
                .unwrap_or(&l.ty)
        } else {
            &l.ty
        };
        match ty {
            IrType::Record(r) => {
                let rec = prog.records.get(r.index()).ok_or_else(|| {
                    EmitError(format!(
                        "tbir: local `{n}` in {} references missing record r{}",
                        func.name, r.0
                    ))
                })?;
                writeln!(out, "{pad}{} {n}{{}}; (void){n};", rec.name).ok();
            }
            // A transaction-sequence local — `std::vector<Record>`, v1's
            // tseq accumulator / call-result shape.
            IrType::RecordSeq(r) => {
                let rec = prog.records.get(r.index()).ok_or_else(|| {
                    EmitError(format!(
                        "tbir: seq local `{n}` in {} references missing record r{}",
                        func.name, r.0
                    ))
                })?;
                writeln!(out, "{pad}std::vector<{}> {n}{{}}; (void){n};", rec.name).ok();
            }
            IrType::FixedVec { elem, len } => {
                let cty = super::aggregate_value_cty(
                    &IrType::FixedVec {
                        elem: elem.clone(),
                        len: *len,
                    },
                    &prog.records,
                );
                writeln!(out, "{pad}{cty} {n}{{}}; (void){n};").ok();
            }
            // A scalar-element transaction-sequence local —
            // `std::vector<T>` over the scalar C++ type. v1's tseq scalar
            // accumulator / call-result shape.
            IrType::Seq(scalar) => {
                let cty = super::field_scalar_cty(scalar);
                writeln!(out, "{pad}std::vector<{cty}> {n}{{}}; (void){n};").ok();
            }
            IrType::Component(c) => {
                let component = prog.components.get(c.index()).ok_or_else(|| {
                    EmitError(format!(
                        "tbir: local `{n}` in {} references missing component c{}",
                        func.name, c.0
                    ))
                })?;
                writeln!(out, "{pad}{} {n}{{}}; (void){n};", component.name).ok();
            }
            // A test-scope event channel — v1's subscriber vector.
            IrType::Event(payload) => {
                let cty = match payload {
                    crate::ir::EventPayload::Scalar { .. } => super::field_scalar_cty(
                        &payload.scalar_ir_type().expect("a scalar payload types"),
                    ),
                    crate::ir::EventPayload::Record(r) => prog
                        .records
                        .get(r.index())
                        .ok_or_else(|| {
                            EmitError(format!(
                                "tbir: event local `{n}` in {} references missing record r{}",
                                func.name, r.0
                            ))
                        })?
                        .name
                        .clone(),
                    crate::ir::EventPayload::FixedVec { .. } => {
                        super::aggregate_value_cty(&payload.value_ir_type(), &prog.records)
                    }
                };
                writeln!(
                    out,
                    "{pad}std::vector<std::function<void({cty})>> {n}; (void){n};"
                )
                .ok();
            }
            // Scalar local. Wide (>64-bit) `uint`/`sint` locals — e.g. a
            // wide method param hoisted as the first N locals — take v1's
            // `_harc_u128` storage; everything else widens to uint64_t.
            ty => {
                let cty = super::local_scalar_cty(ty);
                let init = if matches!(ty, IrType::String) {
                    "nullptr"
                } else {
                    "0"
                };
                writeln!(out, "{pad}{cty} {n} = {init}; (void){n};").ok();
            }
        }
    }
    Ok(())
}

fn flow_hook_capture_name(func: &TbFunction, local: LocalId) -> String {
    format!("__harc_hook_capture_fn{}_l{}", func.id.0, local.0)
}

/// Whether a transactor type's methods use the per-instance state-receiver
/// ABI (#494 P1b): the shared body takes a leading `<Type>_state&
/// self_state` param and each call site passes its instance's struct.
///
/// Every stateful transactor method receives explicit per-instance state.
/// Bound and unbound callables therefore share the same receiver contract;
/// no source instance name is written into a type-owned function body.
pub(super) fn uses_state_receiver(schema: &TransactorSchema) -> bool {
    schema.requires_runtime_receiver()
}

pub(super) fn declare_method_slot(
    out: &mut String,
    prog: &TbProgram,
    transactor: crate::ir::TransactorId,
    schema: &TransactorSchema,
    m: &TransactorMethodSchema,
    depth: usize,
) -> Result<(), EmitError> {
    let func = prog.function(m.function);
    let ret_ty = match func.ret.and_then(|ret| func.locals.get(ret.index())) {
        Some(local) => match &local.ty {
            IrType::Record(r) => prog.records[r.index()].name.clone(),
            IrType::RecordSeq(r) => format!("std::vector<{}>", prog.records[r.index()].name),
            IrType::Seq(scalar) => format!("std::vector<{}>", super::field_scalar_cty(scalar)),
            ty @ IrType::FixedVec { .. } => super::aggregate_value_cty(ty, &prog.records),
            ty => super::local_scalar_cty(ty).to_string(),
        },
        None => "void".to_string(),
    };
    let mut param_tys: Vec<String> = Vec::new();
    // State-receiver ABI (#494 P1b): an unbound stateful transactor's
    // method takes its per-instance state struct by reference as the
    // leading param, so one shared body serves any number of instances.
    if uses_state_receiver(schema) {
        param_tys.push(format!(
            "{}&",
            super::runtime::unbound_state_struct_ref(prog, transactor)
        ));
    }
    param_tys.extend((0..func.params.len()).map(|i| match func.locals[i].ty {
        IrType::Record(r) => prog.records[r.index()].name.clone(),
        IrType::RecordSeq(r) => format!("std::vector<{}>", prog.records[r.index()].name),
        IrType::Seq(ref scalar) => format!("std::vector<{}>", super::field_scalar_cty(scalar)),
        ref ty @ IrType::FixedVec { .. } => super::aggregate_value_cty(ty, &prog.records),
        ref ty => super::local_scalar_cty(ty).to_string(),
    }));
    let params = param_tys.join(", ");
    let pad = INDENT.repeat(depth);
    writeln!(
        out,
        "{pad}std::function<{ret_ty}({params})> {}_{};",
        schema.emission_name(),
        m.name
    )
    .ok();
    Ok(())
}

/// Emit one transactor method body as a `[&]`-capturing lambda named
/// `<Transactor>_<method>` — the same naming and synchronous-call
/// contract as v1's hookable lambdas, with the body as a loop-switch.
///
/// Key delta from `emit_function`: this is a PLAIN function body, not
/// a coroutine. v1 hookables run synchronously — their waits advance
/// the clock directly instead of yielding to the scheduler — so the
/// suspension terminators emit v1's sync shapes (`for (...) tick();`,
/// `while (!(pred)) tick();`) and `Return` is a real `return`.
///
/// Direct transactor instance storage and heartbeat predicates are emitted
/// by the surrounding test scaffold; this routine owns the method lambda and
/// its method-hook fan-out loops.
pub(super) fn emit_method(
    out: &mut String,
    prog: &TbProgram,
    _owner: crate::ir::TestbenchId,
    transactor: crate::ir::TransactorId,
    schema: &TransactorSchema,
    m: &TransactorMethodSchema,
    bound_bus: Option<&BusBindingSchema>,
    randomize_snippets: &[String],
    depth: usize,
) -> Result<(), EmitError> {
    let func = prog.function(m.function);
    let names = cpp_local_names(func);
    // Method bodies poke the DUT directly but carry no `--sv` packed-
    // lane table of their own (no fixture combines transactor methods
    // with packed Vec lanes); an empty table falls back to raw
    // subscripts, matching v1 for that case.
    let empty_lanes = HashMap::new();
    // Method bodies access the bound DUT (`schema.dut_type`); probes are
    // never declared on a transactor DUT (lowering rejects it), but pass
    // the real type so any `PortAccess` would mangle correctly.
    // State-receiver ABI (#494 P1b): an unbound stateful transactor's
    // shared method body reads/writes its per-instance state through
    // `self_state`, the struct the caller passes by reference.
    // Empty-instance state nodes in the body resolve against it. A bound-to
    // initiator BFM keeps its baked-in instance name (non-empty), so no
    // receiver.
    let has_state = uses_state_receiver(schema);
    let cx = ECx {
        prog: Some(prog),
        func,
        names: &names,
        lanes: &empty_lanes,
        bindings: CallableRenderBindings {
            bound_bus,
            ..CallableRenderBindings::default()
        },
        dut_type: &schema.dut_type,
        dut_access: None,
        trace_component: "",
        state_receiver: has_state.then_some("self_state"),
        temporal_widths: &[],
        temporal_cell_prefix: None,
        common_contextual_tseqs: None,
    };
    let nparams = func.params.len();
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let pad3 = INDENT.repeat(depth + 3);

    let ret_ty = match func.ret.and_then(|ret| func.locals.get(ret.index())) {
        Some(local) => match &local.ty {
            IrType::Record(r) => prog.records[r.index()].name.clone(),
            IrType::RecordSeq(r) => format!("std::vector<{}>", prog.records[r.index()].name),
            IrType::Seq(scalar) => format!("std::vector<{}>", super::field_scalar_cty(scalar)),
            ty @ IrType::FixedVec { .. } => super::aggregate_value_cty(ty, &prog.records),
            ty => super::local_scalar_cty(ty).to_string(),
        },
        None => "void".to_string(),
    };
    // A record-typed param (`send(t: RegOp)`) is taken by value as the
    // record struct — the body binds it and reads its fields, mirroring
    // v1's by-value struct param. A scalar param widens to uint64_t, or
    // to v1's `_harc_u128` wide-value type for a >64-bit `uint`/`sint`
    // (the wide-value method ABI — the body moves it to a wide DUT port).
    let mut param_list: Vec<String> = Vec::new();
    if has_state {
        param_list.push(format!(
            "{}& self_state",
            super::runtime::unbound_state_struct_ref(prog, transactor)
        ));
    }
    param_list.extend(
        names[..nparams]
            .iter()
            .enumerate()
            .map(|(i, n)| match func.locals[i].ty {
                IrType::Record(r) => format!("{} {n}", prog.records[r.index()].name),
                IrType::RecordSeq(r) => {
                    format!("std::vector<{}> {n}", prog.records[r.index()].name)
                }
                IrType::Seq(ref scalar) => {
                    format!("std::vector<{}> {n}", super::field_scalar_cty(scalar))
                }
                ref ty @ IrType::FixedVec { .. } => {
                    format!("{} {n}", super::aggregate_value_cty(ty, &prog.records))
                }
                ref ty => format!("{} {n}", super::local_scalar_cty(ty)),
            }),
    );
    let params = param_list.join(", ");
    writeln!(
        out,
        "{pad}{}_{} = [&]({params}) -> {ret_ty} {{",
        schema.emission_name(),
        m.name
    )
    .ok();
    declare_locals(out, prog, func, &names, nparams, depth + 1, cx.dut_access)?;
    declare_port_snapshots(out, &cx, depth + 1)?;
    let hook_args = names[..nparams].join(", ");
    let has_coverage_hooks = !m.cov_hook_subs.is_empty();
    let has_user_hooks = m.hookable;
    if has_coverage_hooks {
        let field = super::runtime::transactor_coverage_hook_field(schema, &m.name, "pre");
        writeln!(
            out,
            "{pad1}for (auto& _h : self_state.{field}) _h({hook_args});"
        )
        .ok();
    }
    if has_user_hooks {
        let field = super::runtime::transactor_hook_field(schema, &m.name, "pre");
        writeln!(
            out,
            "{pad1}for (auto& _h : self_state.{field}) _h({hook_args});"
        )
        .ok();
    }
    writeln!(out, "{pad1}int __bb = {};", func.entry.0).ok();
    writeln!(out, "{pad1}while (true) {{").ok();
    writeln!(out, "{pad2}switch (__bb) {{").ok();
    for (bi, block) in func.blocks.iter().enumerate() {
        writeln!(out, "{pad2}case {bi}: {{").ok();
        for s in &block.stmts {
            // Method bodies have no testbench bus-binding scope — a
            // bus-bound call edge in here is a lowering bug and errors
            // inside `emit_stmt` (empty binding table). They CAN init a
            // record local (`let c : Completion` in an `on` handler), so
            // the record table must be visible for `RecordInit`.
            emit_stmt(out, prog, &cx, &prog.records, &[], s, depth + 3)?;
        }
        match &block.terminator {
            Terminator::Jump(b) => {
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::Branch(c, t, f) => {
                let cond = truthy_expr_cpp(&cx, c)?;
                writeln!(
                    out,
                    "{pad3}if ({cond}) {{ __bb = {}; }} else {{ __bb = {}; }}",
                    t.0, f.0
                )
                .ok();
            }
            Terminator::WaitCycles(n, None, b) => {
                // v1's synchronous wait: one tick() per cycle.
                let n = bounded_count_expr_cpp(&cx, n, i64::MAX as u64)?;
                writeln!(
                    out,
                    "{pad3}for (int64_t _w = 0; _w < (int64_t)({n}); _w++) tick();"
                )
                .ok();
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::WaitUntil { preds, mode, succ } => {
                // v1's synchronous polling loop.
                let cond = preds_cpp(&cx, preds, *mode)?;
                writeln!(out, "{pad3}while (!({cond})) tick();").ok();
                writeln!(out, "{pad3}__bb = {};", succ.0).ok();
            }
            Terminator::WaitTimePs(ps, b) => {
                // Wall-clock settle inside a synchronous method body:
                // v1 advances absolute simulation time inline, and these
                // lambdas capture `now_ps` / `eval_clocks_until` by reference.
                writeln!(out, "{pad3}eval_clocks_until(now_ps + {ps});").ok();
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::Return => {
                // Hook-vector fan-out for hook-triggered covergroups
                // sampled on this method's `post` boundary.
                if has_coverage_hooks && func.implicit_returns.contains(&BlockId(bi as u32)) {
                    let field =
                        super::runtime::transactor_coverage_hook_field(schema, &m.name, "post");
                    writeln!(
                        out,
                        "{pad3}for (auto& _h : self_state.{field}) _h({hook_args});"
                    )
                    .ok();
                }
                if has_user_hooks && func.implicit_returns.contains(&BlockId(bi as u32)) {
                    let field = super::runtime::transactor_hook_field(schema, &m.name, "post");
                    writeln!(
                        out,
                        "{pad3}for (auto& _h : self_state.{field}) _h({hook_args});"
                    )
                    .ok();
                }
                match func.ret {
                    Some(r) => {
                        writeln!(out, "{pad3}return {};", names[r.index()]).ok();
                    }
                    None => {
                        writeln!(out, "{pad3}return;").ok();
                    }
                }
            }
            Terminator::Randomize {
                target,
                constraints,
                succ,
            } => {
                // Same Z3-solve splice as the test/tseq paths: a method
                // body's `randomize(t)` lowers to a `ConstraintSite` in
                // the shared table, so its snippet is already built by
                // `cpp_tb::emit_randomize_snippets`. Method-body sites
                // carry no problem-id (the constraint-IR problem table
                // only catalogs test/tseq spans, in BOTH codegens), so
                // the snippet uses v1's nullptr-descriptor solve path —
                // identical bytes to v1's method-body emission.
                let run_context = cx.bindings.run_context.unwrap_or("ctx");
                let state_receiver =
                    randomize_state_receiver(func, *constraints, None, run_context)?;
                let snippet = randomize_snippet_for(
                    prog,
                    func,
                    &names,
                    *target,
                    *constraints,
                    randomize_snippets,
                    run_context,
                    &state_receiver,
                    cx.bindings.self_receiver,
                )?
                .ok_or_else(|| {
                    EmitError(format!(
                        "tbir: Randomize in method {} references missing constraint \
                                 snippet c{}",
                        func.name, constraints.0
                    ))
                })?;
                out.push_str(&snippet);
                writeln!(out, "{pad3}__bb = {};", succ.0).ok();
            }
            Terminator::WaitUntilTimeout {
                preds,
                mode,
                cycles,
                on_fire,
                on_timeout,
            } => {
                // A method body has no scheduler to defer to, so the
                // timed wait keeps v1's explicit polling loop (spec
                // §7.4's "synchronous context" shape) rather than the
                // coroutine `wait_until_timeout` awaiter: budget read
                // once, `tick()` per cycle, bounded by elapsed cycles.
                // The error bump rides the timeout edge, exactly as on
                // the coroutine path, so the `on_timeout` block carries
                // only the diagnostic text.
                let cond = preds_cpp(&cx, preds, *mode)?;
                let n = bounded_count_expr_cpp(&cx, cycles, i64::MAX as u64)?;
                writeln!(out, "{pad3}int64_t _wu_budget = (int64_t)({n});").ok();
                writeln!(out, "{pad3}int64_t _wu_start = (int64_t)cycle_count;").ok();
                writeln!(
                    out,
                    "{pad3}while (!({cond}) && ((int64_t)cycle_count - _wu_start) < _wu_budget) \
                     tick();"
                )
                .ok();
                writeln!(
                    out,
                    "{pad3}if ({cond}) {{ __bb = {}; }} else {{ ctx.errors++; __bb = {}; }}",
                    on_fire.0, on_timeout.0
                )
                .ok();
            }
            other @ (Terminator::WaitCycles(_, Some(_), _)
            | Terminator::WaitCyclesSync(_, _)
            | Terminator::TbLifecycleCall { .. }
            | Terminator::Fatal(_)) => {
                // Lowering rejects these inside method bodies (or, for
                // Fatal, never produces the terminator at all).
                return Err(EmitError(format!(
                    "tbir: transactor method `{}` contains terminator {other:?} — \
                     lowering gate failed",
                    func.name
                )));
            }
        }
        writeln!(out, "{pad3}break;").ok();
        writeln!(out, "{pad2}}}").ok();
    }
    match func.ret {
        // Value-initialization works for scalar, record, and fixed-vector
        // returns; a literal zero cannot initialize `std::array`.
        Some(_) => writeln!(out, "{pad2}default: return {{}};").ok(),
        None => writeln!(out, "{pad2}default: return;").ok(),
    };
    writeln!(out, "{pad2}}}").ok(); // switch
    writeln!(out, "{pad1}}}").ok(); // while
    writeln!(out, "{pad}}};").ok(); // lambda
    Ok(())
}

/// Emit one composite-component method as a free `<Comp>_<method>(
/// <Comp>& self, args...)` lambda (v1's `emit_component_method` shape).
/// The body addresses fields self-relatively (`self.<field>`) and `emit`
/// fans out over `self.<event>`; it is a loop-switch over the lowered
/// CFG like a transactor method. The `[&]` capture sees `cycle_count`
/// (for the emit heartbeat bump) and sibling method lambdas (file order
/// places sub-component methods before the env's).
pub(super) fn emit_component_method(
    out: &mut String,
    prog: &TbProgram,
    owner: crate::ir::TestbenchId,
    component: crate::ir::ComponentId,
    comp: &crate::ir::ComponentSchema,
    m: &crate::ir::ComponentMethodSchema,
    bound_bus: Option<&BusBindingSchema>,
    dut_type: &str,
    dut_access: Option<&crate::ir::passes::dut_access::DutAccessPlan>,
    dut_lane_widths: &HashMap<String, u32>,
    randomize_snippets: &[String],
    depth: usize,
) -> Result<(), EmitError> {
    let lambda = format!("{}_{}", comp.name, m.name);
    emit_component_fn_lambda(
        out,
        prog,
        &comp.name,
        "self",
        m.function,
        &lambda,
        dut_type,
        dut_access,
        Some(dut_lane_widths),
        Some(CallableHookCtx::Component {
            component: comp,
            method_name: &m.name,
            has_instance_hook_vector: !m.cov_hook_subs.is_empty(),
            has_method_hook_vector: super::has_component_method_hook_subscription(
                prog, owner, component, &m.name,
            ),
        }),
        randomize_snippets,
        None,
        &[],
        bound_bus,
        None,
        None,
        None,
        None,
        depth,
        ComponentFunctionEmission::LocalLambda,
        None,
    )
}

fn testbench_method_schema<'a>(
    prog: &'a TbProgram,
    function: &TbFunction,
) -> Result<
    (
        &'a crate::ir::TestbenchTypeSchema,
        &'a crate::ir::TestbenchMethodSchema,
    ),
    EmitError,
> {
    let FunctionKind::TestbenchMethod {
        testbench, method, ..
    } = function.kind
    else {
        return Err(EmitError(format!(
            "tbir: fn{} `{}` is not a testbench method",
            function.id.0, function.name
        )));
    };
    let schema = prog.testbench_types.get(testbench.index()).ok_or_else(|| {
        EmitError(format!(
            "tbir: testbench method fn{} references missing type tbt{}",
            function.id.0, testbench.0
        ))
    })?;
    let method = schema
        .methods
        .get(method.index())
        .filter(|method| method.function == function.id)
        .ok_or_else(|| {
            EmitError(format!(
                "tbir: testbench type `{}` does not own method fn{}",
                schema.name, function.id.0
            ))
        })?;
    Ok((schema, method))
}

fn testbench_component_param_name(field: &str) -> String {
    format!("_harc_tb_component_{field}")
}

fn testbench_transactor_state_param_name(field: &str) -> String {
    format!("_harc_tb_transactor_state_{field}")
}

fn collect_testbench_method_transactor_fields(
    prog: &TbProgram,
    testbench: crate::ir::TestbenchTypeId,
    function: FunctionId,
    visited: &mut HashSet<FunctionId>,
    fields: &mut BTreeSet<String>,
) -> Result<(), EmitError> {
    if !visited.insert(function) {
        return Ok(());
    }
    let body = prog.functions.get(function.index()).ok_or_else(|| {
        EmitError(format!(
            "tbir: testbench method field analysis references missing fn{}",
            function.0
        ))
    })?;
    if !matches!(
        body.kind,
        FunctionKind::TestbenchMethod {
            testbench: owner,
            ..
        } if owner == testbench
    ) {
        return Err(EmitError(format!(
            "tbir: fn{} `{}` is not a method of testbench type tbt{}",
            body.id.0, body.name, testbench.0
        )));
    }
    for block in &body.blocks {
        for stmt in &block.stmts {
            match stmt {
                Stmt::TransactorStateWrite { instance, .. }
                | Stmt::TransactorStateRecordFieldWrite { instance, .. }
                | Stmt::TransactorStateQueuePush { instance, .. }
                | Stmt::TransactorStateQueuePop { instance, .. } => {
                    if !instance.is_empty() {
                        fields.insert(instance.clone());
                    }
                }
                Stmt::TestbenchCall {
                    function: callee, ..
                } => collect_testbench_method_transactor_fields(
                    prog, testbench, *callee, visited, fields,
                )?,
                _ => {}
            }
        }
    }
    for_each_function_expr(body, |expr| match expr {
        Expr::TransactorState { instance, .. }
        | Expr::TransactorStateRecordField { instance, .. }
        | Expr::TransactorStateQueueQuery { instance, .. } => {
            if !instance.is_empty() {
                fields.insert(instance.clone());
            }
        }
        Expr::TransactorIdle { field, .. } => {
            fields.insert(field.clone());
        }
        Expr::Call(
            CallTarget::TransactorMethod {
                bus_field,
                target: crate::ir::TransactorMethodTarget::Callable { transactor, .. },
                ..
            },
            _,
        ) if prog
            .transactors
            .get(transactor.index())
            .is_some_and(uses_state_receiver) =>
        {
            fields.insert(bus_field.clone());
        }
        _ => {}
    });
    Ok(())
}

pub(super) fn testbench_method_transactor_state_fields(
    prog: &TbProgram,
    function: &TbFunction,
) -> Result<Vec<(String, crate::ir::TransactorId)>, EmitError> {
    let FunctionKind::TestbenchMethod { testbench, .. } = function.kind else {
        return Err(EmitError(format!(
            "tbir: fn{} `{}` is not a testbench method",
            function.id.0, function.name
        )));
    };
    let mut required = BTreeSet::new();
    collect_testbench_method_transactor_fields(
        prog,
        testbench,
        function.id,
        &mut HashSet::new(),
        &mut required,
    )?;
    if required.is_empty() {
        return Ok(Vec::new());
    }

    let implementations = prog
        .testbenches
        .iter()
        .filter(|instance| instance.type_id == testbench)
        .collect::<Vec<_>>();
    let Some(first) = implementations.first() else {
        return Ok(Vec::new());
    };
    let mut fields = Vec::new();
    for (field, transactor) in &first.transactor_fields {
        if !required.remove(field) {
            continue;
        }
        prog.transactors.get(transactor.index()).ok_or_else(|| {
            EmitError(format!(
                "tbir: testbench `{}` field `{field}` references missing transactor x{}",
                first.name, transactor.0
            ))
        })?;
        fields.push((field.clone(), *transactor));
    }
    if let Some(field) = required.first() {
        return Err(EmitError(format!(
            "tbir: testbench method `{}` references transactor field `{field}` missing from implementation `{}`",
            function.name, first.name
        )));
    }
    for instance in implementations {
        for (field, transactor) in &fields {
            let matches = instance
                .transactor_fields
                .iter()
                .filter(|(candidate, owner)| candidate == field && owner == transactor)
                .count();
            if matches != 1 {
                return Err(EmitError(format!(
                    "tbir: testbench `{}` must provide exactly one transactor field `{field}` x{} required by method `{}`; found {matches}",
                    instance.name, transactor.0, function.name
                )));
            }
            let state_matches = instance
                .unbound_state_actors
                .iter()
                .filter(|actor| actor.field == *field && actor.transactor == *transactor)
                .count();
            if state_matches != 1 {
                return Err(EmitError(format!(
                    "tbir: testbench `{}` must provide exactly one state binding for transactor field `{field}` x{} required by method `{}`; found {state_matches}",
                    instance.name, transactor.0, function.name
                )));
            }
        }
    }
    Ok(fields)
}

pub(super) fn testbench_method_emit_order(
    prog: &TbProgram,
    testbench: crate::ir::TestbenchTypeId,
) -> Result<Vec<crate::ir::FunctionId>, EmitError> {
    if prog
        .testbench_types
        .get(testbench.index())
        .is_none_or(|schema| schema.methods.is_empty())
    {
        return Ok(Vec::new());
    }
    crate::ir::passes::callable_placement::analyze(prog)
        .and_then(|catalog| catalog.testbench_method_order(testbench))
        .map_err(|error| EmitError(format!("tbir: {error}")))
}

pub(super) fn component_method_emit_order(
    prog: &TbProgram,
) -> Result<Vec<crate::ir::FunctionId>, EmitError> {
    if !prog.functions.iter().any(|function| {
        matches!(
            function.kind,
            FunctionKind::ComponentMethod {
                method_name: Some(_),
                ..
            }
        )
    }) {
        return Ok(Vec::new());
    }
    crate::ir::passes::callable_placement::analyze(prog)
        .and_then(|catalog| catalog.component_method_order())
        .map_err(|error| EmitError(format!("tbir: {error}")))
}

pub(super) fn emit_testbench_method(
    out: &mut String,
    prog: &TbProgram,
    function: &TbFunction,
    owner: crate::ir::TestbenchId,
    bus_bindings: &[BusBindingSchema],
    dut_type: &str,
    dut_access: Option<&crate::ir::passes::dut_access::DutAccessPlan>,
    dut_lane_widths: &HashMap<String, u32>,
    randomize_snippets: &[String],
    depth: usize,
) -> Result<(), EmitError> {
    let (schema, method) = testbench_method_schema(prog, function)?;
    testbench_method_transactor_state_fields(prog, function)?;
    emit_component_fn_lambda(
        out,
        prog,
        &schema.name,
        "_tb",
        function.id,
        &format!("{}_{}", schema.name, method.name),
        dut_type,
        dut_access,
        Some(dut_lane_widths),
        None,
        randomize_snippets,
        Some(owner),
        bus_bindings,
        None,
        Some(&method.module_param_types),
        None,
        None,
        None,
        depth,
        ComponentFunctionEmission::LocalLambda,
        None,
    )
}

pub(super) fn emit_common_testbench_method_declaration(
    out: &mut String,
    prog: &TbProgram,
    function: &TbFunction,
    bus_adapter: Option<&TestbenchBusAdapterPlan>,
) -> Result<(), EmitError> {
    let (schema, method) = testbench_method_schema(prog, function)?;
    let transactor_fields = testbench_method_transactor_state_fields(prog, function)?;
    let component_param_names = schema
        .component_fields
        .iter()
        .map(|(field, _)| testbench_component_param_name(field))
        .collect::<Vec<_>>();
    let transactor_param_names = transactor_fields
        .iter()
        .map(|(field, _)| testbench_transactor_state_param_name(field))
        .collect::<Vec<_>>();
    let names = cpp_local_names_with_reserved(
        function,
        component_param_names
            .iter()
            .chain(&transactor_param_names)
            .map(String::as_str),
    );
    let ret_ty = callable_return_cty(prog, function)?;
    let params = callable_param_list(
        prog,
        &schema.name,
        "_tb",
        function,
        &names,
        true,
        Some(&method.module_param_types),
        Some(&schema.component_fields),
        Some(&transactor_fields),
        bus_adapter,
    )?;
    writeln!(out, "{ret_ty} {}_{}({params});", schema.name, method.name).ok();
    Ok(())
}

pub(super) fn emit_common_testbench_method_function(
    out: &mut String,
    prog: &TbProgram,
    function: &TbFunction,
    bus_bindings: &[BusBindingSchema],
    bus_adapter: Option<&TestbenchBusAdapterPlan>,
    all_bus_adapters: &[TestbenchBusAdapterPlan],
    dut_type: &str,
    dut_access: &crate::ir::passes::dut_access::DutAccessPlan,
    randomize_snippets: &[String],
    contextual_tseqs: &BTreeSet<FunctionId>,
) -> Result<(), EmitError> {
    let (schema, method) = testbench_method_schema(prog, function)?;
    let transactor_fields = testbench_method_transactor_state_fields(prog, function)?;
    emit_component_fn_lambda(
        out,
        prog,
        &schema.name,
        "_tb",
        function.id,
        &format!("{}_{}", schema.name, method.name),
        dut_type,
        Some(dut_access),
        None,
        None,
        randomize_snippets,
        None,
        bus_bindings,
        None,
        Some(&method.module_param_types),
        Some(&schema.component_fields),
        Some(&transactor_fields),
        Some(BusAdapterRenderBindings {
            current: bus_adapter,
            callables: all_bus_adapters,
        }),
        0,
        ComponentFunctionEmission::CommonFunction,
        Some(contextual_tseqs),
    )
}

pub(super) fn emit_common_transactor_method_declaration(
    out: &mut String,
    prog: &TbProgram,
    transactor: crate::ir::TransactorId,
    schema: &TransactorSchema,
    method: &TransactorMethodSchema,
    bus_adapter: Option<&TestbenchBusAdapterPlan>,
) -> Result<(), EmitError> {
    let function = prog.function(method.function);
    let names = cpp_local_names(function);
    let ret_ty = callable_return_cty(prog, function)?;
    let receiver_type = uses_state_receiver(schema)
        .then(|| super::runtime::unbound_state_struct_ref(prog, transactor))
        .unwrap_or_default();
    let receiver_name = (!receiver_type.is_empty())
        .then_some("self_state")
        .unwrap_or("");
    let params = callable_param_list(
        prog,
        &receiver_type,
        receiver_name,
        function,
        &names,
        true,
        None,
        None,
        None,
        bus_adapter,
    )?;
    writeln!(out, "{ret_ty} {}_{}({params});", schema.name, method.name).ok();
    Ok(())
}

pub(super) fn emit_common_transactor_method_function(
    out: &mut String,
    prog: &TbProgram,
    transactor: crate::ir::TransactorId,
    schema: &TransactorSchema,
    method: &TransactorMethodSchema,
    bus_bindings: &[BusBindingSchema],
    bus_adapter: Option<&TestbenchBusAdapterPlan>,
    all_bus_adapters: &[TestbenchBusAdapterPlan],
    dut_type: &str,
    dut_access: &crate::ir::passes::dut_access::DutAccessPlan,
    randomize_snippets: &[String],
    contextual_tseqs: &BTreeSet<FunctionId>,
) -> Result<(), EmitError> {
    let receiver_type = uses_state_receiver(schema)
        .then(|| super::runtime::unbound_state_struct_ref(prog, transactor))
        .unwrap_or_default();
    let receiver_name = (!receiver_type.is_empty())
        .then_some("self_state")
        .unwrap_or("");
    emit_component_fn_lambda(
        out,
        prog,
        &receiver_type,
        receiver_name,
        method.function,
        &format!("{}_{}", schema.name, method.name),
        dut_type,
        Some(dut_access),
        None,
        Some(CallableHookCtx::Transactor { schema, method }),
        randomize_snippets,
        None,
        bus_bindings,
        None,
        None,
        None,
        None,
        Some(BusAdapterRenderBindings {
            current: bus_adapter,
            callables: all_bus_adapters,
        }),
        0,
        ComponentFunctionEmission::CommonFunction,
        Some(contextual_tseqs),
    )
}

pub(super) fn emit_common_component_method_declaration(
    out: &mut String,
    prog: &TbProgram,
    comp: &crate::ir::ComponentSchema,
    method: &crate::ir::ComponentMethodSchema,
    bus_adapter: Option<&TestbenchBusAdapterPlan>,
) -> Result<(), EmitError> {
    let function = prog.function(method.function);
    let names = cpp_local_names(function);
    let ret_ty = callable_return_cty(prog, function)?;
    let params = callable_param_list(
        prog,
        &comp.name,
        "self",
        function,
        &names,
        true,
        None,
        None,
        None,
        bus_adapter,
    )?;
    writeln!(out, "{ret_ty} {}_{}({params});", comp.name, method.name).ok();
    Ok(())
}

pub(super) fn emit_common_component_method_function(
    out: &mut String,
    prog: &TbProgram,
    comp: &crate::ir::ComponentSchema,
    method: &crate::ir::ComponentMethodSchema,
    bus_bindings: &[BusBindingSchema],
    bus_adapter: Option<&TestbenchBusAdapterPlan>,
    all_bus_adapters: &[TestbenchBusAdapterPlan],
    dut_type: &str,
    dut_access: &crate::ir::passes::dut_access::DutAccessPlan,
    randomize_snippets: &[String],
    contextual_tseqs: &BTreeSet<FunctionId>,
) -> Result<(), EmitError> {
    emit_component_fn_lambda(
        out,
        prog,
        &comp.name,
        "self",
        method.function,
        &format!("{}_{}", comp.name, method.name),
        dut_type,
        Some(dut_access),
        None,
        Some(CallableHookCtx::Component {
            component: comp,
            method_name: &method.name,
            has_instance_hook_vector: !method.cov_hook_subs.is_empty(),
            has_method_hook_vector: method.hookable,
        }),
        randomize_snippets,
        None,
        bus_bindings,
        None,
        None,
        None,
        None,
        Some(BusAdapterRenderBindings {
            current: bus_adapter,
            callables: all_bus_adapters,
        }),
        0,
        ComponentFunctionEmission::CommonFunction,
        Some(contextual_tseqs),
    )
}

pub(super) fn emit_common_component_lifecycle_declaration(
    out: &mut String,
    prog: &TbProgram,
    comp: &crate::ir::ComponentSchema,
    function: &TbFunction,
    symbol: &str,
    bus_adapter: Option<&TestbenchBusAdapterPlan>,
) -> Result<(), EmitError> {
    let names = cpp_local_names(function);
    let ret_ty = callable_return_cty(prog, function)?;
    let params = callable_param_list(
        prog,
        &comp.name,
        "self",
        function,
        &names,
        true,
        None,
        None,
        None,
        bus_adapter,
    )?;
    writeln!(out, "{ret_ty} {symbol}({params});").ok();
    Ok(())
}

pub(super) fn emit_common_component_lifecycle_function(
    out: &mut String,
    prog: &TbProgram,
    comp: &crate::ir::ComponentSchema,
    function: &TbFunction,
    symbol: &str,
    bus_bindings: &[BusBindingSchema],
    bus_adapter: Option<&TestbenchBusAdapterPlan>,
    all_bus_adapters: &[TestbenchBusAdapterPlan],
    dut_type: &str,
    dut_access: &crate::ir::passes::dut_access::DutAccessPlan,
    randomize_snippets: &[String],
    contextual_tseqs: &BTreeSet<FunctionId>,
) -> Result<(), EmitError> {
    emit_component_fn_lambda(
        out,
        prog,
        &comp.name,
        "self",
        function.id,
        symbol,
        dut_type,
        Some(dut_access),
        None,
        None,
        randomize_snippets,
        None,
        bus_bindings,
        None,
        None,
        None,
        None,
        Some(BusAdapterRenderBindings {
            current: bus_adapter,
            callables: all_bus_adapters,
        }),
        0,
        ComponentFunctionEmission::CommonFunction,
        Some(contextual_tseqs),
    )
}

/// Emit one `on <ev>(arg)` handler body as a free
/// `<Comp>_on_h<fid>(<Comp>& self, uint64_t arg)` lambda — same loop-
/// switch shape as a component method, but the lambda name is derived
/// from the handler's FunctionId so it never collides with a user method.
pub(super) fn emit_component_on_handler(
    out: &mut String,
    prog: &TbProgram,
    comp: &crate::ir::ComponentSchema,
    oh: &crate::ir::OnHandlerSchema,
    bound_bus: Option<&BusBindingSchema>,
    dut_type: &str,
    dut_access: Option<&crate::ir::passes::dut_access::DutAccessPlan>,
    dut_lane_widths: &HashMap<String, u32>,
    randomize_snippets: &[String],
    depth: usize,
) -> Result<(), EmitError> {
    let lambda = on_handler_lambda_name(comp, oh);
    emit_component_fn_lambda(
        out,
        prog,
        &comp.name,
        "self",
        oh.function,
        &lambda,
        dut_type,
        dut_access,
        Some(dut_lane_widths),
        None,
        randomize_snippets,
        None,
        &[],
        bound_bus,
        None,
        None,
        None,
        None,
        depth,
        ComponentFunctionEmission::LocalLambda,
        None,
    )
}

/// The free-lambda name for an on-handler (`<Comp>_on_h<fid>`).
pub(super) fn on_handler_lambda_name(
    comp: &crate::ir::ComponentSchema,
    oh: &crate::ir::OnHandlerSchema,
) -> String {
    format!("{}_on_h{}", comp.name, oh.function.0)
}

/// Emit one `on <N> cycles` periodic-handler body as a free
/// `<Comp>_periodic_h<fid>(<Comp>& self)` lambda (zero params besides
/// `self`), mirroring the on-handler lambda shape.
pub(super) fn emit_component_periodic_handler(
    out: &mut String,
    prog: &TbProgram,
    comp: &crate::ir::ComponentSchema,
    ph: &crate::ir::PeriodicHandlerSchema,
    bound_bus: Option<&BusBindingSchema>,
    dut_type: &str,
    dut_access: Option<&crate::ir::passes::dut_access::DutAccessPlan>,
    dut_lane_widths: &HashMap<String, u32>,
    randomize_snippets: &[String],
    depth: usize,
) -> Result<(), EmitError> {
    let lambda = periodic_handler_lambda_name(comp, ph);
    emit_component_fn_lambda(
        out,
        prog,
        &comp.name,
        "self",
        ph.function,
        &lambda,
        dut_type,
        dut_access,
        Some(dut_lane_widths),
        None,
        randomize_snippets,
        None,
        &[],
        bound_bus,
        None,
        None,
        None,
        None,
        depth,
        ComponentFunctionEmission::LocalLambda,
        None,
    )
}

/// The free-lambda name for a periodic handler (`<Comp>_periodic_h<fid>`).
pub(super) fn periodic_handler_lambda_name(
    comp: &crate::ir::ComponentSchema,
    ph: &crate::ir::PeriodicHandlerSchema,
) -> String {
    format!("{}_periodic_h{}", comp.name, ph.function.0)
}

/// Emit a component's cycle-trigger handler body as a free
/// `<Comp>_cycle_h<fid>(<Comp>& self)` lambda (zero params besides
/// `self`), mirroring the periodic-handler lambda shape. The trigger
/// predicate + edge gating live in the per-instance `_checkers` closure
/// (see `mod::emit_lifecycle_checkers`).
pub(super) fn emit_component_cycle_handler(
    out: &mut String,
    prog: &TbProgram,
    comp: &crate::ir::ComponentSchema,
    ch: &crate::ir::CycleTriggerHandlerSchema,
    bound_bus: Option<&BusBindingSchema>,
    dut_type: &str,
    dut_access: Option<&crate::ir::passes::dut_access::DutAccessPlan>,
    dut_lane_widths: &HashMap<String, u32>,
    randomize_snippets: &[String],
    depth: usize,
) -> Result<(), EmitError> {
    let lambda = cycle_handler_lambda_name(comp, ch);
    emit_component_fn_lambda(
        out,
        prog,
        &comp.name,
        "self",
        ch.function,
        &lambda,
        dut_type,
        dut_access,
        Some(dut_lane_widths),
        None,
        randomize_snippets,
        None,
        &[],
        bound_bus,
        None,
        None,
        None,
        None,
        depth,
        ComponentFunctionEmission::LocalLambda,
        None,
    )
}

/// The free-lambda name for a cycle-trigger handler (`<Comp>_cycle_h<fid>`).
pub(super) fn cycle_handler_lambda_name(
    comp: &crate::ir::ComponentSchema,
    ch: &crate::ir::CycleTriggerHandlerSchema,
) -> String {
    format!("{}_cycle_h{}", comp.name, ch.function.0)
}

/// Emit a component's `watchdog` body as a free
/// `<Comp>_watchdog<fid>(<Comp>& self)` lambda (zero params besides
/// `self`). Only the user body runs here; the idle check + period gating
/// are emitted in the per-instance `_checkers` closure (see
/// `mod::emit_lifecycle_checkers`), so a field-backed max_idle/period read
/// stays self-relative there too.
pub(super) fn emit_component_watchdog(
    out: &mut String,
    prog: &TbProgram,
    comp: &crate::ir::ComponentSchema,
    w: &crate::ir::WatchdogSchema,
    bound_bus: Option<&BusBindingSchema>,
    dut_type: &str,
    dut_access: Option<&crate::ir::passes::dut_access::DutAccessPlan>,
    dut_lane_widths: &HashMap<String, u32>,
    randomize_snippets: &[String],
    depth: usize,
) -> Result<(), EmitError> {
    let lambda = watchdog_lambda_name(comp, w);
    emit_component_fn_lambda(
        out,
        prog,
        &comp.name,
        "self",
        w.function,
        &lambda,
        dut_type,
        dut_access,
        Some(dut_lane_widths),
        None,
        randomize_snippets,
        None,
        &[],
        bound_bus,
        None,
        None,
        None,
        None,
        depth,
        ComponentFunctionEmission::LocalLambda,
        None,
    )
}

/// The free-lambda name for a watchdog body (`<Comp>_watchdog<fid>`).
pub(super) fn watchdog_lambda_name(
    comp: &crate::ir::ComponentSchema,
    w: &crate::ir::WatchdogSchema,
) -> String {
    format!("{}_watchdog{}", comp.name, w.function.0)
}

/// Render a component lifecycle predicate in per-instance scope. Wide
/// scalar predicates need an explicit zero test.
pub(super) fn clause_predicate_cpp(
    prog: &TbProgram,
    function: crate::ir::FunctionId,
    instance: &str,
    dut_type: &str,
    dut_access: Option<&crate::ir::passes::dut_access::DutAccessPlan>,
    dut_lane_widths: &HashMap<String, u32>,
    bound_bus: Option<&BusBindingSchema>,
    e: &Expr,
) -> Result<String, EmitError> {
    let func = prog.function(function);
    let names = cpp_local_names(func);
    let cx = ECx {
        prog: Some(prog),
        func,
        names: &names,
        lanes: dut_lane_widths,
        bindings: CallableRenderBindings {
            run_context: None,
            dut_receiver: Some(if dut_access.is_some() {
                "ctx.dut"
            } else {
                "dut"
            }),
            self_receiver: Some(instance),
            testbench_owner: None,
            testbench_receiver: None,
            bound_bus,
            testbench_bus_bindings: None,
            bus_adapters: None,
            clocks: None,
            actor_slot: None,
            testbench_components: None,
            testbench_transactor_states: None,
            runtime_cells: None,
            durable_callbacks: false,
        },
        dut_type,
        dut_access,
        trace_component: "",
        state_receiver: None,
        temporal_widths: &[],
        temporal_cell_prefix: None,
        common_contextual_tseqs: None,
    };
    truthy_expr_cpp(&cx, e)
}

/// Numeric counterpart for component periodic/watchdog timing clauses.
pub(super) fn clause_count_cpp(
    prog: &TbProgram,
    function: crate::ir::FunctionId,
    instance: &str,
    dut_type: &str,
    dut_access: Option<&crate::ir::passes::dut_access::DutAccessPlan>,
    dut_lane_widths: &HashMap<String, u32>,
    bound_bus: Option<&BusBindingSchema>,
    e: &Expr,
) -> Result<String, EmitError> {
    let func = prog.function(function);
    let names = cpp_local_names(func);
    let cx = ECx {
        prog: Some(prog),
        func,
        names: &names,
        lanes: dut_lane_widths,
        bindings: CallableRenderBindings {
            run_context: None,
            dut_receiver: Some(if dut_access.is_some() {
                "ctx.dut"
            } else {
                "dut"
            }),
            self_receiver: Some(instance),
            testbench_owner: None,
            testbench_receiver: None,
            bound_bus,
            testbench_bus_bindings: None,
            bus_adapters: None,
            clocks: None,
            actor_slot: None,
            testbench_components: None,
            testbench_transactor_states: None,
            runtime_cells: None,
            durable_callbacks: false,
        },
        dut_type,
        dut_access,
        trace_component: "",
        state_receiver: None,
        temporal_widths: &[],
        temporal_cell_prefix: None,
        common_contextual_tseqs: None,
    };
    bounded_count_expr_cpp(&cx, e, i64::MAX as u64)
}

/// Render a testbench-scoped cycle-trigger predicate (issue #494 P2b) as
/// standalone C++ text. This uses the TEST-scope ECx (`self_subst: None`)
/// and a real `dut_type`
/// so `dut.<sig>` reads route to the shared `dut` handle and `_tb.<field>`
/// reads resolve to the captured host struct — matching `emit_test_hook`.
pub(super) fn tb_service_expr_cpp(
    prog: &TbProgram,
    function: crate::ir::FunctionId,
    dut_type: &str,
    dut_access: Option<&crate::ir::passes::dut_access::DutAccessPlan>,
    dut_lane_widths: &HashMap<String, u32>,
    e: &Expr,
) -> Result<String, EmitError> {
    let func = prog.function(function);
    let names = cpp_local_names(func);
    let owner = func.owner.ok_or_else(|| {
        EmitError(format!(
            "tbir: testbench service fn{} `{}` has no owning testbench",
            func.id.0, func.name
        ))
    })?;
    let clocks = prog
        .tests
        .iter()
        .find(|test| test.testbench == owner)
        .map(|test| test.clocks.as_slice());
    let cx = ECx {
        prog: Some(prog),
        func,
        names: &names,
        lanes: dut_lane_widths,
        bindings: CallableRenderBindings {
            dut_receiver: Some(if dut_access.is_some() {
                "ctx.dut"
            } else {
                "dut"
            }),
            testbench_owner: Some(owner),
            testbench_receiver: Some("_tb"),
            clocks,
            ..CallableRenderBindings::default()
        },
        dut_type,
        dut_access,
        trace_component: "",
        state_receiver: None,
        temporal_widths: &[],
        temporal_cell_prefix: None,
        common_contextual_tseqs: None,
    };
    truthy_expr_cpp(&cx, e)
}

/// Shared lambda emission for component methods and on-handlers: a free
/// `<lambda>(<Comp>& self, args...)` loop-switch over the lowered CFG.
enum CallableHookCtx<'a> {
    Component {
        component: &'a crate::ir::ComponentSchema,
        method_name: &'a str,
        has_instance_hook_vector: bool,
        has_method_hook_vector: bool,
    },
    Transactor {
        schema: &'a TransactorSchema,
        method: &'a TransactorMethodSchema,
    },
}

impl CallableHookCtx<'_> {
    fn coverage_vector(&self, receiver: &str, side: &str) -> Option<String> {
        match self {
            Self::Component {
                method_name,
                has_instance_hook_vector,
                ..
            } => has_instance_hook_vector
                .then(|| format!("{receiver}._harc_cov_{method_name}_{side}")),
            Self::Transactor { schema, method } => (!method.cov_hook_subs.is_empty()).then(|| {
                format!(
                    "{receiver}.{}",
                    super::runtime::transactor_coverage_hook_field(schema, &method.name, side)
                )
            }),
        }
    }

    fn method_vector(&self, receiver: &str, side: &str) -> Option<String> {
        match self {
            Self::Component {
                component,
                method_name,
                has_method_hook_vector,
                ..
            } => has_method_hook_vector.then(|| {
                format!(
                    "{receiver}.{}",
                    super::runtime::component_internal_member_name(
                        component,
                        &format!("_harc_hook_{method_name}_{side}"),
                    )
                )
            }),
            Self::Transactor { schema, method } => method.hookable.then(|| {
                format!(
                    "{receiver}.{}",
                    super::runtime::transactor_hook_field(schema, &method.name, side)
                )
            }),
        }
    }
}

#[derive(Clone, Copy)]
enum ComponentFunctionEmission {
    LocalLambda,
    CommonFunction,
}

fn callable_return_cty(prog: &TbProgram, func: &TbFunction) -> Result<String, EmitError> {
    match func.ret.map(|ret| &func.locals[ret.index()].ty) {
        Some(ty) => callable_value_cty(prog, ty),
        None => Ok("void".to_string()),
    }
}

fn callable_param_list(
    prog: &TbProgram,
    receiver_type: &str,
    receiver_name: &str,
    func: &TbFunction,
    names: &[String],
    with_context: bool,
    module_param_types: Option<&[Option<String>]>,
    testbench_components: Option<&[(String, crate::ir::ComponentId)]>,
    testbench_transactors: Option<&[(String, crate::ir::TransactorId)]>,
    bus_adapter: Option<&TestbenchBusAdapterPlan>,
) -> Result<String, EmitError> {
    let mut params = Vec::with_capacity(func.params.len() + 2);
    if with_context {
        params.push("HarcTestContext& ctx".to_string());
    }
    if !receiver_type.is_empty() {
        params.push(format!("{receiver_type}& {receiver_name}"));
    }
    for (field, component) in testbench_components.into_iter().flatten() {
        let schema = prog.components.get(component.index()).ok_or_else(|| {
            EmitError(format!(
                "tbir: testbench component field `{field}` references missing component c{}",
                component.0
            ))
        })?;
        params.push(format!(
            "{}& {}",
            schema.name,
            testbench_component_param_name(field)
        ));
    }
    for (field, transactor) in testbench_transactors.into_iter().flatten() {
        prog.transactors.get(transactor.index()).ok_or_else(|| {
            EmitError(format!(
                "tbir: testbench transactor field `{field}` references missing transactor x{}",
                transactor.0
            ))
        })?;
        params.push(format!(
            "{}& {}",
            super::runtime::unbound_state_struct_ref(prog, *transactor),
            testbench_transactor_state_param_name(field)
        ));
    }
    for signal in bus_adapter.into_iter().flat_map(|adapter| &adapter.signals) {
        let ty = callable_value_cty(prog, &signal.ty)?;
        params.push(format!("harc_rt::HarcBusSignalRef<{ty}> {}", signal.symbol));
    }
    for (i, name) in names[..func.params.len()].iter().enumerate() {
        let ty = module_param_types
            .and_then(|types| types.get(i))
            .and_then(Option::as_deref)
            .map(|module| format!("V{module}*"))
            .map(Ok)
            .unwrap_or_else(|| callable_value_cty(prog, &func.locals[i].ty))?;
        params.push(format!("{ty} {name}"));
    }
    Ok(params.join(", "))
}

fn emit_component_fn_lambda(
    out: &mut String,
    prog: &TbProgram,
    receiver_type: &str,
    receiver_name: &str,
    function: crate::ir::FunctionId,
    lambda: &str,
    dut_type: &str,
    dut_access: Option<&crate::ir::passes::dut_access::DutAccessPlan>,
    dut_lane_widths: Option<&HashMap<String, u32>>,
    hook_ctx: Option<CallableHookCtx<'_>>,
    randomize_snippets: &[String],
    testbench_owner: Option<crate::ir::TestbenchId>,
    bus_bindings: &[BusBindingSchema],
    bound_bus: Option<&BusBindingSchema>,
    module_param_types: Option<&[Option<String>]>,
    testbench_component_fields: Option<&[(String, crate::ir::ComponentId)]>,
    testbench_transactor_fields: Option<&[(String, crate::ir::TransactorId)]>,
    bus_adapters: Option<BusAdapterRenderBindings<'_>>,
    depth: usize,
    emission: ComponentFunctionEmission,
    common_contextual_tseqs: Option<&BTreeSet<FunctionId>>,
) -> Result<(), EmitError> {
    let func = prog.function(function);
    let testbench_component_bindings = testbench_component_fields
        .into_iter()
        .flatten()
        .map(
            |(field, component)| super::expr::TestbenchComponentRenderBinding {
                field: field.clone(),
                component: *component,
                receiver: testbench_component_param_name(field),
            },
        )
        .collect::<Vec<_>>();
    let testbench_transactor_state_bindings = if let Some(fields) = testbench_transactor_fields {
        fields
            .iter()
            .map(
                |(field, transactor)| super::expr::TestbenchTransactorStateRenderBinding {
                    field: field.clone(),
                    transactor: *transactor,
                    receiver: testbench_transactor_state_param_name(field),
                },
            )
            .collect::<Vec<_>>()
    } else if matches!(func.kind, FunctionKind::TestbenchMethod { .. }) {
        testbench_owner
            .and_then(|owner| prog.testbenches.get(owner.index()))
            .map(|testbench| {
                testbench
                    .unbound_state_actors
                    .iter()
                    .map(|actor| super::expr::TestbenchTransactorStateRenderBinding {
                        field: actor.field.clone(),
                        transactor: actor.transactor,
                        receiver: actor.storage.clone(),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let names = cpp_local_names_with_reserved(
        func,
        testbench_component_bindings
            .iter()
            .map(|binding| binding.receiver.as_str())
            .chain(
                testbench_transactor_state_bindings
                    .iter()
                    .map(|binding| binding.receiver.as_str()),
            ),
    );
    let empty_lanes = HashMap::new();
    let clock_bindings = testbench_owner.and_then(|owner| {
        prog.tests
            .iter()
            .find(|test| test.testbench == owner)
            .map(|test| test.clocks.as_slice())
    });
    // Component method/on-handler bodies share the test's bound DUT and may
    // read, force, or release suite-cataloged probes.
    let cx = ECx {
        prog: Some(prog),
        func,
        names: &names,
        lanes: dut_lane_widths.unwrap_or(&empty_lanes),
        bindings: CallableRenderBindings {
            run_context: matches!(emission, ComponentFunctionEmission::CommonFunction)
                .then_some("ctx"),
            dut_receiver: Some(match emission {
                ComponentFunctionEmission::LocalLambda => "dut",
                ComponentFunctionEmission::CommonFunction => "ctx.dut",
            }),
            self_receiver: (!receiver_name.is_empty()).then_some(receiver_name),
            testbench_owner,
            testbench_receiver: matches!(func.kind, FunctionKind::TestbenchMethod { .. })
                .then_some(receiver_name),
            bound_bus,
            testbench_bus_bindings: Some(bus_bindings),
            bus_adapters,
            clocks: clock_bindings,
            actor_slot: None,
            testbench_components: (!testbench_component_bindings.is_empty())
                .then_some(testbench_component_bindings.as_slice()),
            testbench_transactor_states: (!testbench_transactor_state_bindings.is_empty())
                .then_some(testbench_transactor_state_bindings.as_slice()),
            runtime_cells: None,
            durable_callbacks: false,
        },
        dut_type,
        dut_access,
        trace_component: "",
        state_receiver: matches!(hook_ctx.as_ref(), Some(CallableHookCtx::Transactor { schema, .. }) if uses_state_receiver(schema))
            .then_some(receiver_name),
        temporal_widths: &[],
        temporal_cell_prefix: None,
        common_contextual_tseqs,
    };
    let nparams = func.params.len();
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let pad3 = INDENT.repeat(depth + 3);
    let tick_call = match emission {
        ComponentFunctionEmission::LocalLambda => "tick()",
        ComponentFunctionEmission::CommonFunction => "harc_tseq_tick(ctx)",
    };

    // A record-returning method (`function predict_read(...) ->
    // ReadResponse`) returns the record struct by value; a scalar return
    // takes the same width-aware storage every other scalar does; no
    // return is `void`.
    //
    // `local_scalar_cty`, not a hardcoded `uint64_t`: past 64 bits a
    // scalar lives in `_harc_u128` or `harc_rt::HarcWide<N>`, and a
    // `-> uint<256>` getter narrowed to the low word here even after
    // its field was stored wide (issue #642). The `__ret` local now
    // carries the declared return type, so this reads the same
    // `IrType` the params below do and the two cannot drift.
    let ret_ty = callable_return_cty(prog, func)?;
    // The receiver `self`, then one parameter per declared param — a
    // record param (`on in_ev(t)` with `event<TinyTxn>`) is taken by
    // value as the record struct; a component-typed param (`observe(addr,
    // model: ProtocolModel)`) by value as the component struct; every
    // other param widens to uint64_t.
    let params = callable_param_list(
        prog,
        receiver_type,
        receiver_name,
        func,
        &names,
        matches!(emission, ComponentFunctionEmission::CommonFunction),
        module_param_types,
        testbench_component_fields,
        testbench_transactor_fields,
        bus_adapters.and_then(|bindings| bindings.current),
    )?;
    match emission {
        ComponentFunctionEmission::LocalLambda => {
            writeln!(out, "{pad}auto {lambda} = [&]({params}) -> {ret_ty} {{").ok();
        }
        ComponentFunctionEmission::CommonFunction => {
            writeln!(out, "{pad}{ret_ty} {lambda}({params}) {{").ok();
            writeln!(out, "{pad1}auto* dut = ctx.dut;").ok();
            writeln!(out, "{pad1}auto& errors = ctx.errors;").ok();
            writeln!(out, "{pad1}auto& _fatal = ctx.fatal;").ok();
            writeln!(out, "{pad1}auto& cycle_count = ctx.cycle_count;").ok();
            writeln!(out, "{pad1}auto& trace = ctx.trace;").ok();
            writeln!(out, "{pad1}auto& log_ctx = ctx.log_ctx;").ok();
            writeln!(
                out,
                "{pad1}auto& _auto_cov_reports = ctx._auto_cov_reports;"
            )
            .ok();
            writeln!(out, "{pad1}auto& harc_rng = ctx.rng;").ok();
            writeln!(out, "{pad1}auto tick = [&]() {{ harc_tseq_tick(ctx); }};").ok();
            writeln!(
                out,
                "{pad1}auto sim_logf_line = [&](FILE* f, const char* sev, const char* fmt, ...) {{"
            )
            .ok();
            writeln!(
                out,
                "{pad2}HARC_RT_LOG_FILE_ONLY_PRINTF(f, cycle_count, sev, fmt);"
            )
            .ok();
            writeln!(out, "{pad1}}};").ok();
            writeln!(
                out,
                "{pad1}auto sim_log_line = [&](const char* sev, const char* fmt, ...) {{"
            )
            .ok();
            writeln!(out, "{pad2}va_list ap;").ok();
            writeln!(out, "{pad2}va_start(ap, fmt);").ok();
            writeln!(
                out,
                "{pad2}harc_rt::log::harc_log_vline(log_ctx.sim_log, &trace, cycle_count, sev, fmt, ap);"
            )
            .ok();
            writeln!(out, "{pad2}va_end(ap);").ok();
            writeln!(out, "{pad1}}};").ok();
        }
    }
    declare_locals(out, prog, func, &names, nparams, depth + 1, cx.dut_access)?;
    declare_port_snapshots(out, &cx, depth + 1)?;
    let hook_args = names[..nparams].join(", ");
    let coverage_pre = hook_ctx
        .as_ref()
        .and_then(|ctx| ctx.coverage_vector(receiver_name, "pre"));
    let method_pre = hook_ctx
        .as_ref()
        .and_then(|ctx| ctx.method_vector(receiver_name, "pre"));
    let coverage_post = hook_ctx
        .as_ref()
        .and_then(|ctx| ctx.coverage_vector(receiver_name, "post"));
    let method_post = hook_ctx
        .as_ref()
        .and_then(|ctx| ctx.method_vector(receiver_name, "post"));
    if let Some(vector) = &coverage_pre {
        writeln!(out, "{pad1}for (auto& _h : {vector}) _h({hook_args});").ok();
    }
    if let Some(vector) = &method_pre {
        writeln!(out, "{pad1}for (auto& _h : {vector}) _h({hook_args});").ok();
    }
    writeln!(out, "{pad1}int __bb = {};", func.entry.0).ok();
    writeln!(out, "{pad1}while (true) {{").ok();
    writeln!(out, "{pad2}switch (__bb) {{").ok();
    for (bi, block) in func.blocks.iter().enumerate() {
        writeln!(out, "{pad2}case {bi}: {{").ok();
        for s in &block.stmts {
            // Record table visible so an `on`-handler body that inits a
            // record local (`let c : Completion`) resolves `RecordInit`.
            emit_stmt(out, prog, &cx, &prog.records, bus_bindings, s, depth + 3)?;
        }
        match &block.terminator {
            Terminator::Jump(b) => {
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::Branch(c, t, f) => {
                let cond = truthy_expr_cpp(&cx, c)?;
                writeln!(
                    out,
                    "{pad3}if ({cond}) {{ __bb = {}; }} else {{ __bb = {}; }}",
                    t.0, f.0
                )
                .ok();
            }
            Terminator::WaitCycles(n, None, b) | Terminator::WaitCyclesSync(n, b) => {
                let n = bounded_count_expr_cpp(&cx, n, i64::MAX as u64)?;
                writeln!(
                    out,
                    "{pad3}for (int64_t _w = 0; _w < (int64_t)({n}); _w++) {tick_call};"
                )
                .ok();
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::WaitCycles(n, Some(clock), b) => {
                if matches!(emission, ComponentFunctionEmission::CommonFunction) {
                    let n = bounded_count_expr_cpp(&cx, n, i64::MAX as u64)?;
                    writeln!(
                        out,
                        "{pad3}harc_wait_clock_cycles(ctx, \"{}\", (long long)({n}));",
                        super::expr::escape_c(&clock.name)
                    )
                    .ok();
                } else {
                    let clocks = cx.bindings.clocks.ok_or_else(|| {
                        EmitError(format!(
                            "tbir: callable `{}` has clock-qualified wait on `{}` without typed clock bindings",
                            func.name, clock.name
                        ))
                    })?;
                    let index = clocks
                        .iter()
                        .position(|candidate| candidate.name == clock.name)
                        .ok_or_else(|| {
                            EmitError(format!(
                                "tbir: callable `{}` waits on clock `{}` absent from its owning test",
                                func.name, clock.name
                            ))
                        })?;
                    let n = bounded_count_expr_cpp(&cx, n, i64::MAX as u64)?;
                    emit_local_qualified_clock_wait(out, &pad3, index, &n);
                }
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::WaitUntil { preds, mode, succ } => {
                let cond = preds_cpp(&cx, preds, *mode)?;
                writeln!(out, "{pad3}while (!({cond})) {tick_call};").ok();
                writeln!(out, "{pad3}__bb = {};", succ.0).ok();
            }
            Terminator::WaitTimePs(ps, b) => {
                match emission {
                    ComponentFunctionEmission::LocalLambda => {
                        writeln!(out, "{pad3}eval_clocks_until(now_ps + {ps});").ok();
                    }
                    ComponentFunctionEmission::CommonFunction => {
                        writeln!(out, "{pad3}harc_eval_clocks_until(ctx, ctx.now_ps + {ps});").ok();
                    }
                }
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::Return => match func.ret {
                Some(r) => {
                    if func.implicit_returns.contains(&BlockId(bi as u32)) {
                        if let Some(vector) = &coverage_post {
                            writeln!(out, "{pad3}for (auto& _h : {vector}) _h({hook_args});").ok();
                        }
                        if let Some(vector) = &method_post {
                            writeln!(out, "{pad3}for (auto& _h : {vector}) _h({hook_args});").ok();
                        }
                    }
                    writeln!(out, "{pad3}return {};", names[r.index()]).ok();
                }
                None => {
                    if func.implicit_returns.contains(&BlockId(bi as u32)) {
                        if let Some(vector) = &coverage_post {
                            writeln!(out, "{pad3}for (auto& _h : {vector}) _h({hook_args});").ok();
                        }
                        if let Some(vector) = &method_post {
                            writeln!(out, "{pad3}for (auto& _h : {vector}) _h({hook_args});").ok();
                        }
                    }
                    writeln!(out, "{pad3}return;").ok();
                }
            },
            Terminator::Randomize {
                target,
                constraints,
                succ,
            } => {
                // Same Z3-solve splice as the test/tseq/transactor-method
                // paths: a component method or `on`-handler body that
                // calls `randomize(t)` lowers to a shared `ConstraintSite`,
                // so its snippet is already built by
                // `cpp_tb::emit_randomize_snippets`. Method-body sites
                // carry no problem-id in either codegen, so the snippet
                // uses v1's nullptr-descriptor solve path — identical
                // bytes to v1's component-body emission.
                let run_context = cx.bindings.run_context.unwrap_or("ctx");
                let state_receiver =
                    randomize_state_receiver(func, *constraints, None, run_context)?;
                let snippet = randomize_snippet_for(
                    prog,
                    func,
                    &names,
                    *target,
                    *constraints,
                    randomize_snippets,
                    run_context,
                    &state_receiver,
                    cx.bindings.self_receiver,
                )?
                .ok_or_else(|| {
                    EmitError(format!(
                        "tbir: Randomize in component body {} references missing \
                                 constraint snippet c{}",
                        func.name, constraints.0
                    ))
                })?;
                out.push_str(&snippet);
                writeln!(out, "{pad3}__bb = {};", succ.0).ok();
            }
            // Same synchronous poll loop as a transactor method body —
            // a component method / `on` handler lambda has no scheduler
            // to defer to either, so both sync body emitters render the
            // timed wait identically. (Lowering no longer gates this
            // terminator out of method bodies, so the arm has to exist in
            // BOTH of them or a component-method timed wait falls into
            // the internal error below.)
            Terminator::WaitUntilTimeout {
                preds,
                mode,
                cycles,
                on_fire,
                on_timeout,
            } => {
                let cond = preds_cpp(&cx, preds, *mode)?;
                let n = bounded_count_expr_cpp(&cx, cycles, i64::MAX as u64)?;
                writeln!(out, "{pad3}int64_t _wu_budget = (int64_t)({n});").ok();
                writeln!(out, "{pad3}int64_t _wu_start = (int64_t)cycle_count;").ok();
                writeln!(
                    out,
                    "{pad3}while (!({cond}) && ((int64_t)cycle_count - _wu_start) < _wu_budget) \
                     {tick_call};"
                )
                .ok();
                writeln!(
                    out,
                    "{pad3}if ({cond}) {{ __bb = {}; }} else {{ ctx.errors++; __bb = {}; }}",
                    on_fire.0, on_timeout.0
                )
                .ok();
            }
            other @ (Terminator::TbLifecycleCall { .. } | Terminator::Fatal(_)) => {
                return Err(EmitError(format!(
                    "tbir: component method `{}` contains terminator {other:?} — \
                     lowering gate failed",
                    func.name
                )));
            }
        }
        writeln!(out, "{pad3}break;").ok();
        writeln!(out, "{pad2}}}").ok();
    }
    // The unreachable switch default returns a value-initialized result —
    // `{}` covers both a record struct and a scalar (zero), matching the
    // `default: return 0;` for scalars while compiling for a record return.
    match func.ret {
        Some(_) => writeln!(out, "{pad2}default: return {{}};").ok(),
        None => writeln!(out, "{pad2}default: return;").ok(),
    };
    writeln!(out, "{pad2}}}").ok(); // switch
    writeln!(out, "{pad1}}}").ok(); // while
    match emission {
        ComponentFunctionEmission::LocalLambda => writeln!(out, "{pad}}};").ok(),
        ComponentFunctionEmission::CommonFunction => writeln!(out, "{pad}}}").ok(),
    };
    Ok(())
}

/// Emit one bound-to target-side TLM responder as a background-coroutine
/// actor (one `ThreadSlot` per target method), mirroring v1's
/// `emit_bound_tlm_target_actors` blocking path. Each actor:
///   1. holds `req_ready=0, rsp_valid=0`, then loops;
///   2. raises `req_ready`, awaits `req_valid && req_ready`, captures the
///      request args off the payload wires;
///   3. ticks, drops `req_ready`, traces the `request` edge;
///   4. runs the lowered responder body (a coroutine loop-switch — its
///      waits `co_await` the scheduler) capturing the return value;
///   5. drives `rsp_data` (value methods), traces `response`, raises
///      `rsp_valid`, awaits `rsp_ready`, ticks, drops `rsp_valid`.
/// Trace payloads match v1 exactly (`tlm_call(cycle, instance, "bus",
/// method, phase, "target")`) so the semantic trace diffs clean.
fn register_target_actor_slot(
    out: &mut String,
    mt: bool,
    actor_threads: &mut Vec<(String, String)>,
    context_storage: Option<&str>,
    sched_var: &str,
    slot_var: &str,
    depth: usize,
) {
    if let Some(context) = context_storage {
        crate::codegen::tbir::runtime::register_context_actor_slot(
            out,
            mt,
            actor_threads,
            context,
            sched_var,
            slot_var,
            depth,
        );
    } else {
        crate::codegen::tbir::runtime::register_actor_slot(
            out,
            mt,
            actor_threads,
            sched_var,
            slot_var,
            depth,
        );
    }
}

pub(super) fn emit_target_actor(
    out: &mut String,
    prog: &TbProgram,
    actor: &crate::ir::TargetTlmActorSchema,
    bindings: &[BusBindingSchema],
    mt: bool,
    actor_threads: &mut Vec<(String, String)>,
    context_storage: Option<&str>,
    depth: usize,
) -> Result<(), EmitError> {
    let schema = prog.transactor(actor.transactor);
    let instance = &actor.instance;
    let bus_field = &actor.bus_field;
    // The serving binding carries any `bind ... with { ... }` remap
    // table; absent one (no test-scope binding), the responder falls
    // back to the `<field>_<method>_<sig>` convention.
    let binding = bindings.iter().find(|b| b.field == *bus_field);
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);

    for tm in &schema.target_methods {
        if !actor.active && matches!(tm.activation, crate::ir::Activation::ActiveOnly) {
            continue;
        }
        let method = &tm.name;
        let func = prog.function(tm.function);
        let names = cpp_local_names(func);
        let input_heartbeat = crate::codegen::tbir::runtime::transactor_heartbeat_field(
            schema,
            crate::ir::passes::runtime_cells::ComponentHeartbeat::Input,
        );
        let output_heartbeat = crate::codegen::tbir::runtime::transactor_heartbeat_field(
            schema,
            crate::ir::passes::runtime_cells::ComponentHeartbeat::Output,
        );
        let empty_lanes = HashMap::new();
        // Target-TLM responder bodies are wire-protocol only; no probes.
        // A downstream forwarded `back.read(...)` initiator trace event
        // carries the responder-instance name in its `component` field
        // (v1's `current_component_instance`), so set the trace context.
        let cx = ECx {
            prog: Some(prog),
            func,
            names: &names,
            lanes: &empty_lanes,
            bindings: CallableRenderBindings {
                actor_slot: Some("_slot"),
                ..CallableRenderBindings::default()
            },
            dut_type: "",
            dut_access: None,
            trace_component: instance,
            // Target responder functions are shared by every binding of the
            // transactor type. Resolve their empty-instance state nodes
            // against this actor's concrete storage object.
            state_receiver: Some(instance),
            temporal_widths: &[],
            temporal_cell_prefix: None,
            common_contextual_tseqs: None,
        };
        let wire = |sig: &str| match binding {
            Some(b) => format!("dut->{}", b.wire_name(method, sig)),
            None => format!("dut->{bus_field}_{method}_{sig}"),
        };
        let slot_var = format!("_{instance}_{method}_target_slot");
        let sched_var = format!("_{instance}_{method}_target_sched");
        let trace_event = |out: &mut String, phase: &str, d: usize| {
            writeln!(
                out,
                "{}trace.tlm_call(cycle_count, \"{}\", \"bus\", \"{}\", \"{phase}\", \"target\");",
                INDENT.repeat(d),
                escape_c(instance),
                escape_c(method)
            )
            .ok();
        };

        // `out_of_order tags N` responder: emit the multi-lane topology
        // (per-tag dispatcher + N lane coroutines + arbiter routing by
        // hidden tag wires), mirroring v1's
        // `emit_bound_tagged_tlm_target_actors`. The blocking path below
        // is the single in-order responder coroutine.
        if let Some(tag_count) = tm.ooo_tags {
            emit_tagged_target_actors(
                out,
                prog,
                &cx,
                func,
                &names,
                tm,
                instance,
                &wire,
                &input_heartbeat,
                &output_heartbeat,
                tag_count as usize,
                bindings,
                mt,
                actor_threads,
                context_storage,
                depth,
            )?;
            continue;
        }

        register_target_actor_slot(
            out,
            mt,
            actor_threads,
            context_storage,
            &sched_var,
            &slot_var,
            depth,
        );
        writeln!(
            out,
            "{pad}auto {slot_var}_lambda = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{"
        )
        .ok();
        writeln!(out, "{pad1}{} = 0;", wire("req_ready")).ok();
        writeln!(out, "{pad1}{} = 0;", wire("rsp_valid")).ok();
        writeln!(out, "{pad1}while (true) {{").ok();
        writeln!(out, "{pad2}{} = 1;", wire("req_ready")).ok();
        writeln!(
            out,
            "{pad2}co_await harc_rt::wait_until(_slot, [&]{{ return {} && {}; }});",
            wire("req_valid"),
            wire("req_ready")
        )
        .ok();
        // Capture request args into the body's parameter locals.
        let nparams = func.params.len();
        for (i, arg) in tm.args.iter().enumerate() {
            let local = &names[i];
            let value_type = tm.param_tys.get(i).ok_or_else(|| {
                EmitError(format!(
                    "tbir: target responder `{}` has no type for request argument `{arg}`",
                    func.name
                ))
            })?;
            let cty = callable_value_cty(prog, value_type)?;
            let capture = typed_wire_read_expr(prog, &wire(arg), value_type)?;
            writeln!(out, "{pad2}{cty} {local} = {capture};",).ok();
        }
        writeln!(out, "{pad2}co_await harc_rt::wait_cycles(_slot, 1);").ok();
        writeln!(out, "{pad2}{} = 0;", wire("req_ready")).ok();
        writeln!(
            out,
            "{pad2}{instance}.{input_heartbeat} = (uint64_t)cycle_count;"
        )
        .ok();
        trace_event(out, "request", depth + 2);

        // Responder body as a coroutine loop-switch. The args are already
        // declared above; declare the remaining locals (incl. the ret
        // slot) and run the switch. `Return` sets `__done`; the captured
        // ret local survives the loop for the response drive.
        writeln!(
            out,
            "{pad2}{{ // {} (TB-IR responder loop-switch)",
            func.name
        )
        .ok();
        emit_responder_loop_switch(out, prog, &cx, func, &names, nparams, bindings, depth + 3)?;
        // Drive the response payload, trace, handshake.
        if tm.has_ret {
            let ret = func.ret.ok_or_else(|| {
                EmitError(format!(
                    "tbir: target responder `{}` declares a return but carries no ret slot",
                    func.name
                ))
            })?;
            // A record-typed return is packed onto the lowered response
            // pin through the record's generated `harc_drive_<R>` helper
            // (v1's `record_drive_stmt`); a scalar is a plain assign.
            let ret_type = func
                .locals
                .get(ret.index())
                .map(|local| &local.ty)
                .ok_or_else(|| {
                    EmitError(format!(
                        "tbir: target responder `{}` references missing return local %{}",
                        func.name, ret.0
                    ))
                })?;
            let drive =
                typed_wire_write_stmt(prog, &wire("rsp_data"), ret_type, &names[ret.index()])?;
            writeln!(out, "{pad2}{INDENT}{drive}").ok();
        }
        writeln!(out, "{pad2}}}").ok(); // body scope
        trace_event(out, "response", depth + 2);
        writeln!(out, "{pad2}{} = 1;", wire("rsp_valid")).ok();
        writeln!(
            out,
            "{pad2}if (!{}) co_await harc_rt::wait_until(_slot, [&]{{ return {}; }});",
            wire("rsp_ready"),
            wire("rsp_ready")
        )
        .ok();
        writeln!(out, "{pad2}co_await harc_rt::wait_cycles(_slot, 1);").ok();
        writeln!(out, "{pad2}{} = 0;", wire("rsp_valid")).ok();
        writeln!(
            out,
            "{pad2}{instance}.{output_heartbeat} = (uint64_t)cycle_count;"
        )
        .ok();
        writeln!(out, "{pad1}}}").ok(); // while(true)
        writeln!(out, "{pad1}co_return;").ok();
        writeln!(out, "{pad}}};").ok();
        writeln!(
            out,
            "{pad}{slot_var}.thread = {slot_var}_lambda(&{slot_var});"
        )
        .ok();
    }
    Ok(())
}

/// Emit the responder body as a coroutine loop-switch into the current
/// scope. The request-arg locals (`func.params`) are assumed already
/// declared by the caller; this declares the remaining locals (incl. the
/// `ret` slot, which survives the loop for the caller's response drive),
/// then runs a `while (!__done) switch (__bb)` over the lowered blocks.
/// A `Return` terminator sets `__done`; `wait`/`wait until` terminators
/// `co_await` the scheduler (the responder body is a coroutine). Shared
/// verbatim by the blocking single-responder path and each OOO lane.
#[allow(clippy::too_many_arguments)]
fn emit_responder_loop_switch(
    out: &mut String,
    prog: &TbProgram,
    cx: &ECx<'_>,
    func: &TbFunction,
    names: &[String],
    nparams: usize,
    bindings: &[BusBindingSchema],
    depth: usize,
) -> Result<(), EmitError> {
    declare_locals(out, prog, func, names, nparams, depth, cx.dut_access)?;
    declare_port_snapshots(out, cx, depth)?;
    writeln!(out, "{}int __bb = {};", INDENT.repeat(depth), func.entry.0).ok();
    writeln!(out, "{}bool __done = false;", INDENT.repeat(depth)).ok();
    writeln!(out, "{}while (!__done) {{", INDENT.repeat(depth)).ok();
    writeln!(out, "{}switch (__bb) {{", INDENT.repeat(depth + 1)).ok();
    let pad_case = INDENT.repeat(depth + 1);
    let pad_body = INDENT.repeat(depth + 2);
    for (bi, block) in func.blocks.iter().enumerate() {
        writeln!(out, "{pad_case}case {bi}: {{").ok();
        for s in &block.stmts {
            // The responder body runs in test scope: a downstream
            // blocking TLM call edge (`back.read(...)` — nested
            // forwarding) resolves against the test's bus bindings, so
            // pass them through. `emit_transactor_call` uses `co_await`
            // (the responder body is a coroutine), and an unresolved
            // `back` surfaces as an EmitError there. The records table
            // is needed for `RecordInit` of a record-returning responder
            // (e.g. a burst read building its `rsp` record).
            emit_stmt(out, prog, cx, &prog.records, bindings, s, depth + 2)?;
        }
        match &block.terminator {
            Terminator::Jump(b) => {
                writeln!(out, "{pad_body}__bb = {};", b.0).ok();
            }
            Terminator::Branch(c, t, f) => {
                let cond = truthy_expr_cpp(cx, c)?;
                writeln!(
                    out,
                    "{pad_body}if ({cond}) {{ __bb = {}; }} else {{ __bb = {}; }}",
                    t.0, f.0
                )
                .ok();
            }
            Terminator::WaitCycles(n, None, b) => {
                let n = bounded_count_expr_cpp(cx, n, u32::MAX as u64)?;
                writeln!(
                    out,
                    "{pad_body}co_await harc_rt::wait_cycles(_slot, (uint32_t)({n}));"
                )
                .ok();
                writeln!(out, "{pad_body}__bb = {};", b.0).ok();
            }
            Terminator::WaitUntil { preds, mode, succ } => {
                let cond = preds_cpp(cx, preds, *mode)?;
                writeln!(
                    out,
                    "{pad_body}co_await harc_rt::wait_until(_slot, [&]{{ return {cond}; }});"
                )
                .ok();
                writeln!(out, "{pad_body}__bb = {};", succ.0).ok();
            }
            Terminator::Return => {
                writeln!(out, "{pad_body}__done = true;").ok();
            }
            other => {
                return Err(EmitError(format!(
                    "tbir: target responder `{}` contains terminator {other:?} — \
                     lowering gate failed",
                    func.name
                )));
            }
        }
        writeln!(out, "{pad_body}break;").ok();
        writeln!(out, "{pad_case}}}").ok();
    }
    writeln!(out, "{pad_case}default: __done = true; break;").ok();
    writeln!(out, "{}}}", INDENT.repeat(depth + 1)).ok(); // switch
    writeln!(out, "{}}}", INDENT.repeat(depth)).ok(); // while
    Ok(())
}

/// Re-lower an `active` bound event-driven transactor's `on <ev>(arg)`
/// driver into a queue-fed worker-coroutine actor under `--mt`, mirroring
/// v1's `try_emit_bound_driver_actor` (`cpp_tb.rs:7258-7513`). For each
/// `on <ev>` handler the component declares:
///
///   1. a per-instance `std::deque<Payload> _<inst>_<ev>_q;` stimulus queue;
///   2. a *pusher* subscriber `<inst>.<ev>.push_back([&](auto _t){
///      _q.push_back(_t); });` — `emit <inst>.<ev>(t)` now ENQUEUES from
///      the run coroutine instead of running the body inline;
///   3. an actor `ThreadSlot` (its own `ThreadScheduler` under `--mt` via
///      `register_actor_slot`); and
///   4. a worker coroutine that loops `co_await wait_until(!_q.empty())`,
///      pops the front transaction, bumps `_last_in_cycle`, then runs the
///      handler body as a coroutine loop-switch (waits/bus handshakes lower
///      to `co_await`, so the driver yields each cycle and shares the
///      per-posedge barrier window with the bound monitor).
///
/// The cooperative-default path keeps the synchronous on-handler subscriber
/// (`emit_on_handler_regs`) — this is emitted only when `mt` is true and
/// the instance is an `active` bound driver, so default output is unchanged.
pub(super) fn emit_active_bound_driver_actor(
    out: &mut String,
    prog: &TbProgram,
    component: crate::ir::ComponentId,
    inst_path: &str,
    dut_type: &str,
    bindings: &[BusBindingSchema],
    bound_bus: Option<&BusBindingSchema>,
    actor_threads: &mut Vec<(String, String)>,
    depth: usize,
) -> Result<(), EmitError> {
    let comp = &prog.components[component.index()];
    let input_heartbeat = super::runtime::component_heartbeat_field(
        comp,
        crate::ir::passes::runtime_cells::ComponentHeartbeat::Input,
    );
    let inst_tag = inst_path.replace('.', "_");
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    for oh in &comp.on_handlers {
        let func = prog.function(oh.function);
        let names = cpp_local_names(func);
        let empty_lanes = HashMap::new();
        // Driver bodies poke bound-bus DUT wires through the explicit instance
        // adapter and forward downstream blocking TLM calls; no probes.
        // `self_subst = inst_path` so a `self.<field>` access (`self.last_read
        // = val`) resolves to the running instance (`drv.last_read`) — the
        // worker coroutine has no `self` parameter, unlike the synchronous
        // on-handler lambda.
        let cx = ECx {
            prog: Some(prog),
            func,
            names: &names,
            lanes: &empty_lanes,
            bindings: CallableRenderBindings {
                run_context: None,
                dut_receiver: Some("dut"),
                self_receiver: Some(inst_path),
                testbench_owner: None,
                testbench_receiver: None,
                bound_bus,
                testbench_bus_bindings: None,
                bus_adapters: None,
                clocks: None,
                actor_slot: Some("_slot"),
                testbench_components: None,
                testbench_transactor_states: None,
                runtime_cells: None,
                durable_callbacks: false,
            },
            dut_type,
            dut_access: None,
            trace_component: inst_path,
            state_receiver: None,
            temporal_widths: &[],
            temporal_cell_prefix: None,
            common_contextual_tseqs: None,
        };
        let payload_ty =
            crate::codegen::tbir::runtime::event_payload_cty(&oh.arg_payload, &prog.records);
        let queue_var = format!("_{inst_tag}_{}_q", oh.event);
        let slot_var = format!("_{inst_tag}_{}_drv_slot", oh.event);
        let sched_var = format!("_{inst_tag}_{}_drv_sched", oh.event);
        // 1. Stimulus queue.
        writeln!(out, "{pad}std::deque<{payload_ty}> {queue_var};").ok();
        // 2. Pusher subscriber: `emit <inst>.<ev>(t)` enqueues.
        writeln!(
            out,
            "{pad}{inst_path}.{}.push_back([&](auto _t) {{ {queue_var}.push_back(_t); }});",
            oh.event
        )
        .ok();
        // 3. Actor slot (own ThreadScheduler under --mt).
        crate::codegen::tbir::runtime::register_actor_slot(
            out,
            true,
            actor_threads,
            &sched_var,
            &slot_var,
            depth,
        );
        // 4. Worker coroutine: pop a transaction, bump activity, run body.
        writeln!(
            out,
            "{pad}auto {slot_var}_lambda = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{"
        )
        .ok();
        writeln!(out, "{pad1}while (true) {{").ok();
        writeln!(
            out,
            "{pad2}co_await harc_rt::wait_until(_slot, [&]{{ return !{queue_var}.empty(); }});"
        )
        .ok();
        // The handler's single param local is the dequeued transaction.
        let arg_name = &names[0];
        let arg_decl_ty = match func.locals[0].ty {
            IrType::Record(r) => prog.records[r.index()].name.clone(),
            _ => "uint64_t".to_string(),
        };
        writeln!(out, "{pad2}{arg_decl_ty} {arg_name} = {queue_var}.front();").ok();
        writeln!(out, "{pad2}{queue_var}.pop_front();").ok();
        writeln!(
            out,
            "{pad2}{inst_path}.{input_heartbeat} = (uint64_t)cycle_count;"
        )
        .ok();
        // Body as a coroutine loop-switch: param 0 (the txn) is declared
        // above, so skip it; waits/bus handshakes `co_await` the slot.
        writeln!(
            out,
            "{pad2}{{ // {} (TB-IR active-driver loop-switch)",
            func.name
        )
        .ok();
        emit_responder_loop_switch(out, prog, &cx, func, &names, 1, bindings, depth + 3)?;
        writeln!(out, "{pad2}}}").ok();
        writeln!(out, "{pad1}}}").ok(); // while(true)
        writeln!(out, "{pad1}co_return;").ok();
        writeln!(out, "{pad}}};").ok();
        writeln!(
            out,
            "{pad}{slot_var}.thread = {slot_var}_lambda(&{slot_var});"
        )
        .ok();
    }
    Ok(())
}

/// Emit the `out_of_order tags N` RESPONDER topology for one target
/// method, mirroring v1's `emit_bound_tagged_tlm_target_actors`:
///
///  1. Per-tag shared state arrays (`std::array<…, N>`): `lane_busy`,
///     `lane_req_valid`, `lane_rsp_valid`, one exactly typed array per
///     request arg, and (value methods) an exactly typed `lane_rsp_data`
///     array.
///  2. A combinational `_post_eval_services` closure that drives
///     `req_ready` = "a lane is free for the requested (or any) tag",
///     matching v1's dispatcher accept gate.
///  3. A dispatcher coroutine: awaits an accepted request, latches the
///     args + tag into the lane's slot, marks it busy/pending, traces the
///     `request` edge (with `_tag`), ticks.
///  4. N lane coroutines: each awaits its `lane_req_valid`, binds the
///     latched args, runs the responder body (loop-switch), stores the
///     return into `lane_rsp_data`, raises `lane_rsp_valid`, awaits the
///     arbiter clearing it, drops `lane_busy`.
///  5. An arbiter coroutine: awaits any lane response, selects the
///     highest-index ready lane (v1's order), drives `rsp_data`/`rsp_tag`,
///     traces the `response` edge (with `_sel`), runs the rsp handshake,
///     clears the lane.
///
/// In cooperative mode every coroutine pushes onto the shared `sched`;
/// under `--mt` each (dispatcher, per-lane, arbiter) slot gets its own
/// dedicated `ThreadScheduler` + OS worker thread, mirroring v1's
/// multi-lane split (`cpp_tb.rs:6837-7129`). Trace payloads (incl. the
/// routed tag) match v1 byte-for-byte so `harc trace-diff` stays clean.
#[allow(clippy::too_many_arguments)]
fn emit_tagged_target_actors(
    out: &mut String,
    prog: &TbProgram,
    cx: &ECx<'_>,
    func: &TbFunction,
    names: &[String],
    tm: &crate::ir::TargetTlmMethodSchema,
    instance: &str,
    wire: &dyn Fn(&str) -> String,
    input_heartbeat: &str,
    output_heartbeat: &str,
    tag_count: usize,
    bindings: &[BusBindingSchema],
    mt: bool,
    actor_threads: &mut Vec<(String, String)>,
    context_storage: Option<&str>,
    depth: usize,
) -> Result<(), EmitError> {
    let method = &tm.name;
    let pad = INDENT.repeat(depth);
    let prefix = format!("_{instance}_{method}_target_ooo");
    let lane_busy = format!("{prefix}_lane_busy");
    let lane_req_valid = format!("{prefix}_lane_req_valid");
    let lane_rsp_valid = format!("{prefix}_lane_rsp_valid");
    let lane_rsp_data = format!("{prefix}_lane_rsp_data");
    let dispatcher_slot = format!("{prefix}_dispatcher_slot");
    let dispatcher_sched = format!("{prefix}_dispatcher_sched");
    let arbiter_slot = format!("{prefix}_arbiter_slot");
    let arbiter_sched = format!("{prefix}_arbiter_sched");

    // --- (1) Per-tag shared state arrays. ---
    writeln!(
        out,
        "{pad}std::array<std::atomic<bool>, {tag_count}> {lane_busy}{{}};"
    )
    .ok();
    writeln!(
        out,
        "{pad}std::array<std::atomic<bool>, {tag_count}> {lane_req_valid}{{}};"
    )
    .ok();
    writeln!(
        out,
        "{pad}std::array<std::atomic<bool>, {tag_count}> {lane_rsp_valid}{{}};"
    )
    .ok();
    for (index, arg) in tm.args.iter().enumerate() {
        let value_type = tm.param_tys.get(index).ok_or_else(|| {
            EmitError(format!(
                "tbir: OOO target responder `{}` has no type for request argument `{arg}`",
                func.name
            ))
        })?;
        let cty = callable_value_cty(prog, value_type)?;
        let arr = format!("{prefix}_arg_{arg}");
        writeln!(out, "{pad}std::array<{cty}, {tag_count}> {arr}{{}};").ok();
    }
    if tm.has_ret {
        let ret_type = tm.ret_ty.as_ref().ok_or_else(|| {
            EmitError(format!(
                "tbir: OOO target responder `{}` has no declared return type",
                func.name
            ))
        })?;
        let cty = callable_value_cty(prog, ret_type)?;
        writeln!(
            out,
            "{pad}std::array<{cty}, {tag_count}> {lane_rsp_data}{{}};"
        )
        .ok();
    }

    // --- (2) Combinational dispatcher accept gate (`req_ready`). ---
    writeln!(out, "{pad}_post_eval_services.push_back([&]() {{").ok();
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    writeln!(out, "{pad1}bool _tlm_ready = false;").ok();
    writeln!(out, "{pad1}if ({}) {{", wire("req_valid")).ok();
    writeln!(out, "{pad2}auto _tag = (size_t){};", wire("req_tag")).ok();
    writeln!(
        out,
        "{pad2}_tlm_ready = _tag < {tag_count} && !{lane_busy}[_tag].load() && !{lane_req_valid}[_tag].load();"
    )
    .ok();
    writeln!(out, "{pad1}}} else {{").ok();
    writeln!(
        out,
        "{pad2}for (size_t i = 0; i < {tag_count}; ++i) if (!{lane_busy}[i].load() && !{lane_req_valid}[i].load()) {{ _tlm_ready = true; break; }}"
    )
    .ok();
    writeln!(out, "{pad1}}}").ok();
    writeln!(out, "{pad1}{} = _tlm_ready;", wire("req_ready")).ok();
    writeln!(out, "{pad}}});").ok();

    // --- (3) Dispatcher coroutine. ---
    register_target_actor_slot(
        out,
        mt,
        actor_threads,
        context_storage,
        &dispatcher_sched,
        &dispatcher_slot,
        depth,
    );
    writeln!(
        out,
        "{pad}auto {dispatcher_slot}_lambda = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{"
    )
    .ok();
    writeln!(out, "{pad1}{} = 0;", wire("req_ready")).ok();
    writeln!(out, "{pad1}while (true) {{").ok();
    writeln!(
        out,
        "{pad2}co_await harc_rt::wait_until(_slot, [&]{{ return {} && {}; }});",
        wire("req_valid"),
        wire("req_ready")
    )
    .ok();
    writeln!(out, "{pad2}{{").ok();
    let pad3 = INDENT.repeat(depth + 3);
    writeln!(out, "{pad3}auto _tag = (size_t){};", wire("req_tag")).ok();
    for (index, arg) in tm.args.iter().enumerate() {
        let value_type = tm.param_tys.get(index).ok_or_else(|| {
            EmitError(format!(
                "tbir: OOO target responder `{}` has no type for request argument `{arg}`",
                func.name
            ))
        })?;
        let arr = format!("{prefix}_arg_{arg}");
        let capture = typed_wire_read_expr(prog, &wire(arg), value_type)?;
        writeln!(out, "{pad3}{arr}[_tag] = {capture};",).ok();
    }
    writeln!(out, "{pad3}{lane_busy}[_tag].store(true);").ok();
    writeln!(out, "{pad3}{lane_req_valid}[_tag].store(true);").ok();
    writeln!(
        out,
        "{pad3}{instance}.{input_heartbeat} = (uint64_t)cycle_count;"
    )
    .ok();
    writeln!(
        out,
        "{pad3}trace.tlm_call(cycle_count, \"{}\", \"bus\", \"{}\", \"request\", \"target\", (int64_t)(_tag));",
        escape_c(instance),
        escape_c(method)
    )
    .ok();
    writeln!(out, "{pad2}}}").ok();
    writeln!(out, "{pad2}co_await harc_rt::wait_cycles(_slot, 1);").ok();
    writeln!(out, "{pad1}}}").ok();
    writeln!(out, "{pad1}co_return;").ok();
    writeln!(out, "{pad}}};").ok();
    writeln!(
        out,
        "{pad}{dispatcher_slot}.thread = {dispatcher_slot}_lambda(&{dispatcher_slot});"
    )
    .ok();

    // --- (4) Lane coroutines. ---
    let nparams = func.params.len();
    for lane in 0..tag_count {
        let lane_slot = format!("{prefix}_lane{lane}_slot");
        let lane_sched = format!("{prefix}_lane{lane}_sched");
        register_target_actor_slot(
            out,
            mt,
            actor_threads,
            context_storage,
            &lane_sched,
            &lane_slot,
            depth,
        );
        writeln!(
            out,
            "{pad}auto {lane_slot}_lambda = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{"
        )
        .ok();
        writeln!(out, "{pad1}while (true) {{").ok();
        writeln!(
            out,
            "{pad2}co_await harc_rt::wait_until(_slot, [&]{{ return {lane_req_valid}[{lane}].load(); }});"
        )
        .ok();
        writeln!(out, "{pad2}{lane_req_valid}[{lane}].store(false);").ok();
        // Bind the latched request args into the body's parameter locals.
        for (i, arg) in tm.args.iter().enumerate() {
            let local = &names[i];
            let arr = format!("{prefix}_arg_{arg}");
            let value_type = tm.param_tys.get(i).ok_or_else(|| {
                EmitError(format!(
                    "tbir: OOO target responder `{}` has no type for request argument `{arg}`",
                    func.name
                ))
            })?;
            let cty = callable_value_cty(prog, value_type)?;
            writeln!(out, "{pad2}{cty} {local} = {arr}[{lane}];").ok();
        }
        // Responder body loop-switch (shared with the blocking path).
        writeln!(
            out,
            "{pad2}{{ // {} (TB-IR OOO lane {lane} loop-switch)",
            func.name
        )
        .ok();
        emit_responder_loop_switch(out, prog, cx, func, names, nparams, bindings, depth + 3)?;
        if tm.has_ret {
            let ret = func.ret.ok_or_else(|| {
                EmitError(format!(
                    "tbir: OOO target responder `{}` declares a return but carries no ret slot",
                    func.name
                ))
            })?;
            writeln!(
                out,
                "{pad3}{lane_rsp_data}[{lane}] = {};",
                &names[ret.index()]
            )
            .ok();
        }
        writeln!(out, "{pad2}}}").ok(); // body scope
        writeln!(out, "{pad2}{lane_rsp_valid}[{lane}].store(true);").ok();
        writeln!(
            out,
            "{pad2}co_await harc_rt::wait_until(_slot, [&]{{ return !{lane_rsp_valid}[{lane}].load(); }});"
        )
        .ok();
        writeln!(out, "{pad2}{lane_busy}[{lane}].store(false);").ok();
        writeln!(out, "{pad1}}}").ok();
        writeln!(out, "{pad1}co_return;").ok();
        writeln!(out, "{pad}}};").ok();
        writeln!(
            out,
            "{pad}{lane_slot}.thread = {lane_slot}_lambda(&{lane_slot});"
        )
        .ok();
    }

    // --- (5) Arbiter coroutine. ---
    register_target_actor_slot(
        out,
        mt,
        actor_threads,
        context_storage,
        &arbiter_sched,
        &arbiter_slot,
        depth,
    );
    writeln!(
        out,
        "{pad}auto {arbiter_slot}_lambda = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{"
    )
    .ok();
    writeln!(out, "{pad1}{} = 0;", wire("rsp_valid")).ok();
    writeln!(out, "{pad1}while (true) {{").ok();
    writeln!(
        out,
        "{pad2}co_await harc_rt::wait_until(_slot, [&]{{ for (size_t i = 0; i < {tag_count}; ++i) if ({lane_rsp_valid}[i].load()) return true; return false; }});"
    )
    .ok();
    writeln!(out, "{pad2}int _sel = -1;").ok();
    writeln!(
        out,
        "{pad2}for (int i = {tag_count} - 1; i >= 0; --i) if ({lane_rsp_valid}[(size_t)i].load()) {{ _sel = i; break; }}"
    )
    .ok();
    writeln!(out, "{pad2}if (_sel >= 0) {{").ok();
    if tm.has_ret {
        let ret_type = tm.ret_ty.as_ref().ok_or_else(|| {
            EmitError(format!(
                "tbir: OOO target responder `{}` has no declared return type",
                func.name
            ))
        })?;
        let drive = typed_wire_write_stmt(
            prog,
            &wire("rsp_data"),
            ret_type,
            &format!("{lane_rsp_data}[(size_t)_sel]"),
        )?;
        writeln!(out, "{pad3}{drive}",).ok();
    }
    writeln!(out, "{pad3}{} = _sel;", wire("rsp_tag")).ok();
    writeln!(
        out,
        "{pad3}trace.tlm_call(cycle_count, \"{}\", \"bus\", \"{}\", \"response\", \"target\", (int64_t)(_sel));",
        escape_c(instance),
        escape_c(method)
    )
    .ok();
    writeln!(out, "{pad3}{} = 1;", wire("rsp_valid")).ok();
    writeln!(
        out,
        "{pad3}if (!{}) co_await harc_rt::wait_until(_slot, [&]{{ return {}; }});",
        wire("rsp_ready"),
        wire("rsp_ready")
    )
    .ok();
    writeln!(out, "{pad3}co_await harc_rt::wait_cycles(_slot, 1);").ok();
    writeln!(out, "{pad3}{} = 0;", wire("rsp_valid")).ok();
    writeln!(out, "{pad3}{lane_rsp_valid}[(size_t)_sel].store(false);").ok();
    writeln!(
        out,
        "{pad3}{instance}.{output_heartbeat} = (uint64_t)cycle_count;"
    )
    .ok();
    writeln!(out, "{pad2}}}").ok();
    writeln!(out, "{pad1}}}").ok();
    writeln!(out, "{pad1}co_return;").ok();
    writeln!(out, "{pad}}};").ok();
    writeln!(
        out,
        "{pad}{arbiter_slot}.thread = {arbiter_slot}_lambda(&{arbiter_slot});"
    )
    .ok();
    Ok(())
}

/// Join wait-until sub-predicates the way v1 does: a single predicate
/// emits bare; multiple emit as a parenthesized `&&` chain (`all of`)
/// or `||` chain (`any of`).
fn preds_cpp(cx: &ECx<'_>, preds: &[PredSrc], mode: WaitMode) -> Result<String, EmitError> {
    if preds.is_empty() {
        return Err(EmitError(format!(
            "tbir: wait-until terminator with no predicates in {} — lowering bug",
            cx.func.name
        )));
    }
    if preds.len() == 1 {
        return truthy_expr_cpp(cx, &preds[0].expr);
    }
    let joiner = match mode {
        WaitMode::Single | WaitMode::AllOf => " && ",
        WaitMode::AnyOf => " || ",
    };
    let parts = preds
        .iter()
        .map(|p| truthy_expr_cpp(cx, &p.expr))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(parts
        .iter()
        .map(|p| format!("({p})"))
        .collect::<Vec<_>>()
        .join(joiner))
}

/// `sim_log_line("SEV", "fmt", args...)` or the `sim_logf_line` file
/// variant — identical call shape to v1 so runtime log/trace text
/// matches byte-for-byte.
fn emit_log_call(
    out: &mut String,
    cx: &ECx<'_>,
    sev: &str,
    file: Option<&str>,
    args: &FmtArgs,
    depth: usize,
) -> Result<(), EmitError> {
    let pad = INDENT.repeat(depth);
    match file {
        Some(p) => {
            write!(
                out,
                "{pad}sim_logf_line(log_ctx.file(\"{}\"), \"{sev}\", \"{}\"",
                escape_c(p),
                escape_c(&args.fmt)
            )
            .ok();
        }
        None => {
            write!(
                out,
                "{pad}sim_log_line(\"{sev}\", \"{}\"",
                escape_c(&args.fmt)
            )
            .ok();
        }
    }
    for a in &args.args {
        let rendered = fmt_arg_cpp(cx, a)?;
        write!(out, ", {rendered}").ok();
    }
    writeln!(out, ");").ok();
    Ok(())
}

/// Emit one closure-hook body (`FunctionKind::TestHook`) as a free lambda
/// named by `func.name`. Structurally identical to a transactor method
/// (synchronous loop-switch, `tick()`-based waits) since a hook fires inside
/// synchronous callback dispatch rather than the run coroutine.
pub(super) fn emit_test_hook(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
    dut_type: &str,
    depth: usize,
    render: TestHookRenderBindings<'_>,
) -> Result<(), EmitError> {
    let default_testbench_component_bindings = func
        .owner
        .and_then(|owner| prog.testbenches.get(owner.index()))
        .map(|testbench| {
            testbench
                .component_fields
                .iter()
                .map(|binding| super::expr::TestbenchComponentRenderBinding {
                    field: binding.field.clone(),
                    component: binding.component,
                    receiver: binding.field.clone(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let testbench_component_bindings = render
        .flow
        .testbench_components
        .unwrap_or(default_testbench_component_bindings.as_slice());
    let default_testbench_transactor_state_bindings = func
        .owner
        .and_then(|owner| prog.testbenches.get(owner.index()))
        .map(|testbench| {
            testbench
                .unbound_state_actors
                .iter()
                .map(|actor| super::expr::TestbenchTransactorStateRenderBinding {
                    field: actor.field.clone(),
                    transactor: actor.transactor,
                    receiver: actor.storage.clone(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let testbench_transactor_state_bindings = render
        .flow
        .testbench_transactor_states
        .unwrap_or(default_testbench_transactor_state_bindings.as_slice());
    let names = cpp_local_names_with_reserved(
        func,
        testbench_component_bindings
            .iter()
            .map(|binding| binding.receiver.as_str())
            .chain(
                testbench_transactor_state_bindings
                    .iter()
                    .map(|binding| binding.receiver.as_str()),
            )
            .chain(render.flow.reserved.iter().copied()),
    );
    let empty_lanes = HashMap::new();
    let owner = func.owner.ok_or_else(|| {
        EmitError(format!(
            "tbir: test hook fn{} `{}` has no owning testbench",
            func.id.0, func.name
        ))
    })?;
    let owner_testbench = prog.testbenches.get(owner.index()).ok_or_else(|| {
        EmitError(format!(
            "tbir: test hook fn{} `{}` references missing testbench tb{}",
            func.id.0, func.name, owner.0
        ))
    })?;
    let clocks = render.flow.clocks.or_else(|| {
        prog.tests
            .iter()
            .find(|test| test.testbench == owner)
            .map(|test| test.clocks.as_slice())
    });
    let cx = ECx {
        prog: Some(prog),
        func,
        names: &names,
        lanes: render.flow.dut_lane_widths.unwrap_or(&empty_lanes),
        bindings: CallableRenderBindings {
            run_context: render.flow.run_context,
            dut_receiver: render.flow.dut_receiver,
            testbench_owner: Some(owner),
            testbench_receiver: render.flow.testbench_receiver.or(Some("_tb")),
            clocks,
            testbench_components: (!testbench_component_bindings.is_empty())
                .then_some(testbench_component_bindings),
            testbench_transactor_states: (!testbench_transactor_state_bindings.is_empty())
                .then_some(testbench_transactor_state_bindings),
            testbench_bus_bindings: Some(&owner_testbench.bus_bindings),
            bus_adapters: render.flow.bus_adapters,
            runtime_cells: render.runtime_cells,
            durable_callbacks: render.durable_capture,
            ..CallableRenderBindings::default()
        },
        dut_type,
        dut_access: render.flow.dut_access,
        trace_component: "",
        state_receiver: None,
        temporal_widths: &[],
        temporal_cell_prefix: None,
        common_contextual_tseqs: render.common_contextual_tseqs,
    };
    let nparams = func.params.len();
    let capture_count = super::runtime::test_hook_capture_count(prog, func.id);
    let capture_base = nparams.saturating_sub(capture_count);
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let pad3 = INDENT.repeat(depth + 3);

    // Hooks are void in this subset; a value-returning hook is never
    // produced by lowering (the firing site discards any result).
    let param_ty = |i: usize| -> Result<String, EmitError> {
        hook_param_cty(
            prog,
            local_ir_type(&cx, crate::ir::LocalId(i as u32)).unwrap_or(&func.locals[i].ty),
        )
    };
    let params = names[..nparams]
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let reference = if i >= capture_base { "&" } else { "" };
            Ok(format!("{}{reference} {n}", param_ty(i)?))
        })
        .collect::<Result<Vec<_>, EmitError>>()?
        .join(", ");
    // A per-register `on regs.REG` callback can re-enter `record_write` and
    // call itself. Every callback body therefore binds through its planned
    // run-owned `std::function` slot rather than a deduced `auto` lambda.
    let storage = render
        .runtime_cells
        .ok_or_else(|| {
            EmitError(format!(
                "tbir: test hook fn{} `{}` has no typed runtime-cell binding",
                func.id.0, func.name
            ))
        })?
        .test_hook(func.id)?;
    if render.durable_capture {
        writeln!(
            out,
            "{pad}{storage} = [_harc_callback_state = &_harc_run_state, _harc_callback_context = &ctx]({params}) -> void {{"
        )
        .ok();
        writeln!(out, "{pad1}auto& _harc_run_state = *_harc_callback_state;").ok();
        writeln!(out, "{pad1}auto& ctx = *_harc_callback_context;").ok();
        writeln!(out, "{pad1}auto* dut = ctx.dut;").ok();
        writeln!(out, "{pad1}auto& errors = ctx.errors;").ok();
        writeln!(out, "{pad1}auto& _fatal = ctx.fatal;").ok();
        writeln!(out, "{pad1}auto& cycle_count = ctx.cycle_count;").ok();
        writeln!(out, "{pad1}auto& trace = ctx.trace;").ok();
        writeln!(out, "{pad1}auto& log_ctx = ctx.log_ctx;").ok();
        writeln!(out, "{pad1}auto& _checkers = ctx._checkers;").ok();
        writeln!(
            out,
            "{pad1}auto& _post_eval_services = ctx._post_eval_services;"
        )
        .ok();
        writeln!(
            out,
            "{pad1}auto& _auto_cov_reports = ctx._auto_cov_reports;"
        )
        .ok();
        writeln!(out, "{pad1}auto& harc_rng = ctx.rng;").ok();
        for binding in &owner_testbench.regblock_bindings {
            writeln!(
                out,
                "{pad1}auto& {} = _harc_run_state.{};",
                binding.field, binding.field
            )
            .ok();
            if !binding.callbacks.is_empty() {
                writeln!(
                    out,
                    "{pad1}auto& {}_cb_depth = _harc_run_state.{}_cb_depth;",
                    binding.field, binding.field
                )
                .ok();
            }
        }
        writeln!(
            out,
            "{pad1}auto sim_logf_line = [&](FILE* f, const char* sev, const char* fmt, ...) {{"
        )
        .ok();
        writeln!(
            out,
            "{pad2}HARC_RT_LOG_FILE_ONLY_PRINTF(f, cycle_count, sev, fmt);"
        )
        .ok();
        writeln!(out, "{pad1}}};").ok();
        writeln!(
            out,
            "{pad1}auto sim_log_line = [&](const char* sev, const char* fmt, ...) {{"
        )
        .ok();
        writeln!(out, "{pad2}va_list ap;").ok();
        writeln!(out, "{pad2}va_start(ap, fmt);").ok();
        writeln!(out, "{pad2}harc_rt::log::harc_log_vline(log_ctx.sim_log, &trace, cycle_count, sev, fmt, ap);").ok();
        writeln!(out, "{pad2}va_end(ap);").ok();
        writeln!(out, "{pad1}}};").ok();
    } else {
        writeln!(out, "{pad}{storage} = [&]({params}) -> void {{").ok();
    }
    declare_locals(out, prog, func, &names, nparams, depth + 1, cx.dut_access)?;
    declare_port_snapshots(out, &cx, depth + 1)?;
    writeln!(out, "{pad1}int __bb = {};", func.entry.0).ok();
    writeln!(out, "{pad1}while (true) {{").ok();
    writeln!(out, "{pad2}switch (__bb) {{").ok();
    for (bi, block) in func.blocks.iter().enumerate() {
        writeln!(out, "{pad2}case {bi}: {{").ok();
        for s in &block.stmts {
            emit_stmt(out, prog, &cx, &prog.records, &[], s, depth + 3)?;
        }
        match &block.terminator {
            Terminator::Jump(b) => {
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::Branch(c, t, f) => {
                let cond = truthy_expr_cpp(&cx, c)?;
                writeln!(
                    out,
                    "{pad3}if ({cond}) {{ __bb = {}; }} else {{ __bb = {}; }}",
                    t.0, f.0
                )
                .ok();
            }
            Terminator::WaitCycles(n, None, b) | Terminator::WaitCyclesSync(n, b) => {
                let n = bounded_count_expr_cpp(&cx, n, i64::MAX as u64)?;
                let tick = render
                    .flow
                    .run_context
                    .map(|context| format!("harc_tseq_tick({context})"))
                    .unwrap_or_else(|| "tick()".to_string());
                writeln!(
                    out,
                    "{pad3}for (int64_t _w = 0; _w < (int64_t)({n}); _w++) {tick};"
                )
                .ok();
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::WaitCycles(n, Some(clock), b) => {
                if render.flow.run_context.is_some() {
                    let n = bounded_count_expr_cpp(&cx, n, i64::MAX as u64)?;
                    writeln!(
                        out,
                        "{pad3}harc_wait_clock_cycles(ctx, \"{}\", (long long)({n}));",
                        super::expr::escape_c(&clock.name)
                    )
                    .ok();
                } else {
                    let clocks = clocks.ok_or_else(|| {
                        EmitError(format!(
                            "tbir: test hook `{}` has clock-qualified wait on `{}` without typed clock bindings",
                            func.name, clock.name
                        ))
                    })?;
                    let index = clocks
                        .iter()
                        .position(|candidate| candidate.name == clock.name)
                        .ok_or_else(|| {
                            EmitError(format!(
                                "tbir: test hook `{}` waits on clock `{}` absent from its owning test",
                                func.name, clock.name
                            ))
                        })?;
                    let n = bounded_count_expr_cpp(&cx, n, i64::MAX as u64)?;
                    emit_local_qualified_clock_wait(out, &pad3, index, &n);
                }
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::WaitUntil { preds, mode, succ } => {
                let cond = preds_cpp(&cx, preds, *mode)?;
                let tick = render
                    .flow
                    .run_context
                    .map(|context| format!("harc_tseq_tick({context})"))
                    .unwrap_or_else(|| "tick()".to_string());
                writeln!(out, "{pad3}while (!({cond})) {tick};").ok();
                writeln!(out, "{pad3}__bb = {};", succ.0).ok();
            }
            Terminator::WaitTimePs(ps, b) => {
                if let Some(context) = render.flow.run_context {
                    writeln!(
                        out,
                        "{pad3}harc_eval_clocks_until({context}, {context}.now_ps + {ps});"
                    )
                    .ok();
                } else {
                    writeln!(out, "{pad3}eval_clocks_until(now_ps + {ps});").ok();
                }
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::Return => {
                writeln!(out, "{pad3}return;").ok();
            }
            other => {
                return Err(EmitError(format!(
                    "tbir: closure-hook `{}` contains terminator {other:?} — lowering gate failed",
                    func.name
                )));
            }
        }
        writeln!(out, "{pad3}break;").ok();
        writeln!(out, "{pad2}}}").ok();
    }
    writeln!(out, "{pad2}default: return;").ok();
    writeln!(out, "{pad2}}}").ok(); // switch
    writeln!(out, "{pad1}}}").ok(); // while
    writeln!(out, "{pad}}};").ok(); // lambda
    Ok(())
}

#[cfg(test)]
mod lifecycle_classifier_tests {
    //! #619 M4b: `lifecycle_shareable_kind` must NOT classify a lifecycle
    //! body that reaches a run-coroutine frame local as out-of-line-shareable.
    //! Two classes:
    //!   * a frame-local / suspending CALL — a bus/TLM `TransactorMethod`, a
    //!     `Tseq` generator, or a transactor `TransactorSelfMethod` — which
    //!     emits `co_await`/`_slot` or references a `[&]`-captured run-coroutine
    //!     lambda; and
    //!   * (#619 M4 A3) a non-call HOST-STATE READ whose emitted receiver is a
    //!     run-frame local not reconstructed by the ambient prologue — an
    //!     owned/env scoreboard (`ScoreboardQuery{nested_path: Some}`, bare
    //!     `direct.count`), a bound-to transactor instance (`TransactorState*`),
    //!     or a composite-component read through a `Path`/`SelfField` base.
    //! Neither an out-of-line `static void` function nor a coroutine driven from
    //! the outside can reach those names, so the whole body must fall back to
    //! re-inline. These tests build the offending IR directly (no DUT needed)
    //! and pin the classifier decision.
    use super::*;
    use crate::ast::LifecyclePhase;
    use crate::ir::{
        BasicBlock, BinOp, ComponentBase, CovgroupId, CovgroupInstance, FunctionId, FunctionKind,
        IdleKind, LaneIndex, PortAccess, PortRef, ScoreboardId, TemporalFn, TestbenchId,
        TransactorId, TypedLocal, WidthCastKind,
    };

    fn single_block_lifecycle(stmts: Vec<Stmt>, terminator: Terminator) -> TbFunction {
        TbFunction {
            id: FunctionId(0),
            name: "__tb_lifecycle_Test_Setup".to_string(),
            kind: FunctionKind::TestbenchLifecycle {
                testbench: TestbenchId(0),
                phase: LifecyclePhase::Setup,
            },
            params: vec![],
            locals: vec![TypedLocal {
                name: "r".to_string(),
                ty: IrType::UInt(Some(32)),
            }],
            blocks: vec![BasicBlock { stmts, terminator }],
            entry: BlockId(0),
            owner: Some(TestbenchId(0)),
            ret: None,
            implicit_returns: vec![],
            testbench_record_locals: vec![],
        }
    }

    fn tlm_call() -> Expr {
        // `bus.read(...)` lowers to Stmt::Assign(dest, Expr::Call(TransactorMethod,..)).
        Expr::Call(
            CallTarget::TransactorMethod {
                bus_field: "mem".to_string(),
                method: "read".to_string(),
                target: crate::ir::TransactorMethodTarget::BoundBus,
            },
            vec![],
        )
    }

    #[test]
    fn tlm_call_in_lifecycle_is_not_shareable() {
        let f = single_block_lifecycle(
            vec![Stmt::Assign(LocalId(0), tlm_call())],
            Terminator::Return,
        );
        assert_eq!(
            lifecycle_shareable_kind(&f),
            None,
            "a TLM/transactor-method call in a lifecycle body must fall back to re-inline"
        );
    }

    #[test]
    fn tseq_call_in_lifecycle_is_not_shareable() {
        let call = Expr::Call(
            CallTarget::Tseq {
                function: FunctionId(0),
                name: "Gen".to_string(),
            },
            vec![],
        );
        let f = single_block_lifecycle(vec![Stmt::Assign(LocalId(0), call)], Terminator::Return);
        assert_eq!(lifecycle_shareable_kind(&f), None);
    }

    #[test]
    fn transactor_self_method_call_in_lifecycle_is_not_shareable() {
        // The module docstring names all three frame-reaching call targets;
        // `TransactorMethod` and `Tseq` are pinned above. `TransactorSelfMethod`
        // emits a `<Transactor>_<method>(...)` call INSIDE the enclosing
        // transactor-method lambda (a run-coroutine frame local), so an
        // out-of-line lifecycle body cannot reach it either. It never appears
        // directly in a lifecycle body today (self-method calls are confined to
        // transactor method bodies), but `call_target_reaches_frame` rejects it
        // defensively — this pins that leg of the classifier so a future
        // relaxation cannot silently start sharing such a body.
        let call = Expr::Call(
            CallTarget::TransactorSelfMethod {
                transactor: TransactorId(0),
                transactor_name: "Drv".to_string(),
                method: "step".to_string(),
                function: FunctionId(0),
            },
            vec![],
        );
        let f = single_block_lifecycle(vec![Stmt::Assign(LocalId(0), call)], Terminator::Return);
        assert_eq!(
            lifecycle_shareable_kind(&f),
            None,
            "a transactor self-method call in a lifecycle body must fall back to re-inline"
        );
    }

    #[test]
    fn nested_tlm_call_under_arithmetic_is_not_shareable() {
        // Defensive: even wrapped in a combinator, the TLM call excludes.
        let expr = Expr::Binary(
            BinOp::Add,
            Box::new(tlm_call()),
            Box::new(Expr::Literal {
                value: 1,
                ty: IrType::UInt(Some(32)),
            }),
        );
        let f = single_block_lifecycle(vec![Stmt::Assign(LocalId(0), expr)], Terminator::Return);
        assert_eq!(lifecycle_shareable_kind(&f), None);
    }

    #[test]
    fn tlm_call_carried_by_tb_field_write_is_not_shareable() {
        // The rvalue scan covers non-Assign whitelisted stmts too, so a
        // frame-reaching call hidden in any whitelisted stmt's value excludes.
        let f = single_block_lifecycle(
            vec![Stmt::TbFieldWrite {
                field: "f".to_string(),
                value: tlm_call(),
            }],
            Terminator::Return,
        );
        assert_eq!(lifecycle_shareable_kind(&f), None);
    }

    #[test]
    fn plain_value_lifecycle_is_shareable_plain() {
        let f = single_block_lifecycle(
            vec![Stmt::Assign(
                LocalId(0),
                Expr::Literal {
                    value: 1,
                    ty: IrType::UInt(Some(32)),
                },
            )],
            Terminator::Return,
        );
        assert_eq!(lifecycle_shareable_kind(&f), Some(LifecycleEmit::Plain));
    }

    #[test]
    fn wallclock_and_fatal_terminators_are_not_shareable() {
        let wallclock = single_block_lifecycle(vec![], Terminator::WaitTimePs(10_000, BlockId(0)));
        assert_eq!(
            lifecycle_shareable_kind(&wallclock),
            None,
            "WaitTimePs reaches scheduler-time frame locals and must re-inline"
        );

        // No parser statement currently lowers to Terminator::Fatal:
        // source-level `log(fatal, ...)` is Stmt::Log + Return. Keep the IR
        // terminator's conservative fallback pinned in case it becomes
        // source-reachable later.
        let fatal = single_block_lifecycle(
            vec![Stmt::Log {
                level: LogLevel::Fatal,
                args: crate::ir::FmtArgs {
                    fmt: "boom".to_string(),
                    args: vec![],
                },
            }],
            Terminator::Fatal(crate::ir::FmtArgs {
                fmt: "boom".to_string(),
                args: vec![],
            }),
        );
        assert_eq!(
            lifecycle_shareable_kind(&fatal),
            None,
            "a Fatal terminator must re-inline if it becomes source-reachable"
        );
    }

    #[test]
    fn pure_helper_call_lifecycle_is_shareable_plain() {
        // A pure-helper call resolves to a file-scope symbol → safe to share.
        let call = Expr::Call(
            CallTarget::Helper {
                function: FunctionId(0),
                name: "h".to_string(),
                ret: IrType::UInt(Some(32)),
            },
            vec![],
        );
        let f = single_block_lifecycle(vec![Stmt::Assign(LocalId(0), call)], Terminator::Return);
        assert_eq!(lifecycle_shareable_kind(&f), Some(LifecycleEmit::Plain));
    }

    // ---- #619 M4 A3: frame-local HOST-STATE READS route to re-inline. ----

    fn scoreboard_scalar_read(nested_path: Option<Vec<String>>) -> Expr {
        Expr::ScoreboardQuery {
            sb: ScoreboardId(0),
            field: "direct".to_string(),
            query: crate::ir::ScoreboardQuery::Scalar {
                scalar: "count".to_string(),
            },
            nested_path,
        }
    }

    fn pred(expr: Expr) -> crate::ir::PredSrc {
        crate::ir::PredSrc {
            expr,
            src_text: "direct.count".to_string(),
        }
    }

    #[test]
    fn frame_local_reads_in_terminator_operands_are_not_shareable() {
        let literal = || Expr::Literal {
            value: 1,
            ty: IrType::UInt(Some(32)),
        };
        let cases = [
            Terminator::Branch(
                scoreboard_scalar_read(Some(vec!["direct".to_string()])),
                BlockId(0),
                BlockId(0),
            ),
            Terminator::WaitCycles(
                scoreboard_scalar_read(Some(vec!["direct".to_string()])),
                None,
                BlockId(0),
            ),
            Terminator::WaitUntil {
                preds: vec![pred(scoreboard_scalar_read(Some(vec![
                    "direct".to_string()
                ])))],
                mode: crate::ir::WaitMode::Single,
                succ: BlockId(0),
            },
            // Any-of timeout diagnostics have no FailDiag guard, so only the
            // terminator predicate scan can reject this reachable shape.
            Terminator::WaitUntilTimeout {
                preds: vec![
                    pred(scoreboard_scalar_read(Some(vec!["direct".to_string()]))),
                    pred(literal()),
                ],
                mode: crate::ir::WaitMode::AnyOf,
                cycles: literal(),
                on_fire: BlockId(0),
                on_timeout: BlockId(0),
            },
            Terminator::WaitUntilTimeout {
                preds: vec![pred(literal())],
                mode: crate::ir::WaitMode::Single,
                cycles: scoreboard_scalar_read(Some(vec!["direct".to_string()])),
                on_fire: BlockId(0),
                on_timeout: BlockId(0),
            },
        ];

        for terminator in cases {
            let f = single_block_lifecycle(vec![], terminator);
            assert_eq!(
                lifecycle_shareable_kind(&f),
                None,
                "a frame-local terminator operand must force re-inline"
            );
        }
    }

    #[test]
    fn owned_scoreboard_read_in_lifecycle_is_not_shareable() {
        // An owned/env scoreboard read carries `nested_path: Some` and emits as
        // a bare `direct.count` run-frame local — undeclared in an out-of-line
        // body. This is the exact shape of the A3 miscompile. Carried inside an
        // assert condition (the realistic position), it must exclude sharing.
        let f = single_block_lifecycle(
            vec![Stmt::AssertCheck {
                cond: Expr::Binary(
                    BinOp::Ge,
                    Box::new(scoreboard_scalar_read(Some(vec!["direct".to_string()]))),
                    Box::new(Expr::Literal {
                        value: 0,
                        ty: IrType::UInt(Some(32)),
                    }),
                ),
                on_fail: crate::ir::FmtArgs {
                    fmt: String::new(),
                    args: vec![],
                },
            }],
            Terminator::Return,
        );
        assert_eq!(
            lifecycle_shareable_kind(&f),
            None,
            "a bare-receiver scoreboard read in a lifecycle body must re-inline"
        );
    }

    #[test]
    fn timeout_fail_diag_frame_local_guard_is_not_shareable() {
        // WaitUntilTimeout diagnostics are otherwise safe to emit out of
        // line, but their re-evaluated predicate guard can still read a
        // run-frame-local receiver. The FailDiag whitelist must scan it.
        let f = single_block_lifecycle(
            vec![Stmt::FailDiag {
                guard: Some(scoreboard_scalar_read(Some(vec!["direct".to_string()]))),
                args: crate::ir::FmtArgs {
                    fmt: "not ready".to_string(),
                    args: vec![],
                },
            }],
            Terminator::Return,
        );
        assert_eq!(lifecycle_shareable_kind(&f), None);
    }

    #[test]
    fn tb_field_scoreboard_read_in_lifecycle_stays_shareable() {
        // `nested_path: None` is a scoreboard-typed TESTBENCH field, emitted as
        // `_tb.direct.count`. The prologue aliases `_tb`, so this stays safely
        // shareable — the A3 fix must not over-reject it.
        let f = single_block_lifecycle(
            vec![Stmt::Assign(LocalId(0), scoreboard_scalar_read(None))],
            Terminator::Return,
        );
        assert_eq!(lifecycle_shareable_kind(&f), Some(LifecycleEmit::Plain));
    }

    #[test]
    fn transactor_state_read_in_lifecycle_is_not_shareable() {
        let read = Expr::TransactorState {
            instance: "drv".to_string(),
            field: "count".to_string(),
        };
        let f = single_block_lifecycle(vec![Stmt::Assign(LocalId(0), read)], Terminator::Return);
        assert_eq!(lifecycle_shareable_kind(&f), None);
    }

    #[test]
    fn component_path_read_in_lifecycle_is_not_shareable() {
        // A `ComponentBase::Path` read emits a bare `env.sb.field` receiver.
        let read = Expr::ComponentField {
            base: ComponentBase::Path(vec!["env".to_string(), "sb".to_string()]),
            field: "count".to_string(),
        };
        let f = single_block_lifecycle(vec![Stmt::Assign(LocalId(0), read)], Terminator::Return);
        assert_eq!(lifecycle_shareable_kind(&f), None);
    }

    #[test]
    fn component_local_read_in_lifecycle_stays_shareable() {
        // A `ComponentBase::Local` receiver is the lifecycle function's OWN
        // body-local, so it is in scope out of line and must not over-reject.
        let read = Expr::ComponentField {
            base: ComponentBase::Local(LocalId(0)),
            field: "count".to_string(),
        };
        let f = single_block_lifecycle(vec![Stmt::Assign(LocalId(0), read)], Terminator::Return);
        assert_eq!(lifecycle_shareable_kind(&f), Some(LifecycleEmit::Plain));
    }

    // ---- Exhaustive follow-up: shapes the first (non-exhaustive) fix missed. ----

    fn lit(v: u64) -> Expr {
        Expr::Literal {
            value: v,
            ty: IrType::UInt(Some(32)),
        }
    }

    #[test]
    fn component_idle_path_read_in_lifecycle_is_not_shareable() {
        // `agent.idle_in(N)` → `ComponentIdle{base: Path}` emits a bare
        // `agent._last_in_cycle` frame local. The confirmed-reachable shape the
        // first fix missed (its match ended in `_ => false`).
        let read = Expr::ComponentIdle {
            base: ComponentBase::Path(vec!["tagger".to_string()]),
            subpath: vec![],
            kind: IdleKind::In,
            n: Box::new(lit(2)),
        };
        let f = single_block_lifecycle(vec![Stmt::Assign(LocalId(0), read)], Terminator::Return);
        assert_eq!(lifecycle_shareable_kind(&f), None);
    }

    #[test]
    fn component_idle_local_base_stays_shareable() {
        // A `Local` base is a body-local — must not over-reject.
        let read = Expr::ComponentIdle {
            base: ComponentBase::Local(LocalId(0)),
            subpath: vec![],
            kind: IdleKind::Both,
            n: Box::new(lit(2)),
        };
        let f = single_block_lifecycle(vec![Stmt::Assign(LocalId(0), read)], Terminator::Return);
        assert_eq!(lifecycle_shareable_kind(&f), Some(LifecycleEmit::Plain));
    }

    #[test]
    fn transactor_idle_read_in_lifecycle_is_not_shareable() {
        let read = Expr::TransactorIdle {
            field: "drv".to_string(),
            transactor: TransactorId(0),
            storage: "drv".to_string(),
            kind: IdleKind::Out,
            n: Box::new(lit(3)),
        };
        let f = single_block_lifecycle(vec![Stmt::Assign(LocalId(0), read)], Terminator::Return);
        assert_eq!(lifecycle_shareable_kind(&f), None);
    }

    #[test]
    fn temporal_slot_read_in_lifecycle_is_not_shareable() {
        // `_harc_ps<i>` / `_harc_cur<i>` are per-check-closure static cells,
        // not ambient-prologue names.
        let read = Expr::TemporalSlot {
            slot: 0,
            kind: TemporalFn::Past,
        };
        let f = single_block_lifecycle(vec![Stmt::Assign(LocalId(0), read)], Terminator::Return);
        assert_eq!(lifecycle_shareable_kind(&f), None);
    }

    #[test]
    fn width_cast_wrapped_frame_local_read_is_not_shareable() {
        // `(direct.count as uint<8>)` — the frame-local scoreboard read is
        // NESTED under a `WidthCast`. The first fix did not descend into
        // `WidthCast`; the exhaustive walk does.
        let read = Expr::WidthCast {
            kind: WidthCastKind::Trunc,
            width: 8,
            src_width: Some(32),
            inner: Box::new(scoreboard_scalar_read(Some(vec!["direct".to_string()]))),
        };
        let f = single_block_lifecycle(vec![Stmt::Assign(LocalId(0), read)], Terminator::Return);
        assert_eq!(lifecycle_shareable_kind(&f), None);
    }

    #[test]
    fn width_cast_wrapped_safe_read_stays_shareable() {
        // A width-cast over a literal has no frame local — must stay shareable.
        let read = Expr::WidthCast {
            kind: WidthCastKind::Zext,
            width: 64,
            src_width: Some(32),
            inner: Box::new(lit(7)),
        };
        let f = single_block_lifecycle(vec![Stmt::Assign(LocalId(0), read)], Terminator::Return);
        assert_eq!(lifecycle_shareable_kind(&f), Some(LifecycleEmit::Plain));
    }

    fn dut_vec_port(lane: Option<LaneIndex>) -> PortRef {
        PortRef {
            testbench_field: "dut".to_string(),
            origin: crate::ir::PortOrigin::Dut,
            port_path: vec!["vec".to_string()],
            aggregate_path: false,
            deferred_bus_binding: None,
            direction: None,
            width: None,
            access: PortAccess::Port,
            probe: None,
            value_type: None,
            lane,
        }
    }

    #[test]
    fn dut_port_runtime_lane_frame_local_is_not_shareable() {
        // `dut.vec[direct.count] = 1` — the runtime lane subscript reads a
        // frame local. The `PortRef` lane is an expression position the first
        // fix never scanned.
        let port = dut_vec_port(Some(LaneIndex::Var(Box::new(scoreboard_scalar_read(
            Some(vec!["direct".to_string()]),
        )))));
        let f = single_block_lifecycle(vec![Stmt::DutWrite(port, lit(1))], Terminator::Return);
        assert_eq!(lifecycle_shareable_kind(&f), None);
    }

    #[test]
    fn dut_port_const_lane_stays_shareable() {
        // A constant lane renders a literal — must not over-reject.
        let port = dut_vec_port(Some(LaneIndex::Const(2)));
        let f = single_block_lifecycle(vec![Stmt::DutWrite(port, lit(1))], Terminator::Return);
        assert_eq!(lifecycle_shareable_kind(&f), Some(LifecycleEmit::Plain));
    }

    #[test]
    fn covbin_read_stays_shareable() {
        // `CovBin` reads `_tb.<cov>.<point>.<bin>` — the prologue aliases
        // `_tb`, so it stays safely shareable.
        let read = Expr::CovBin {
            inst: CovgroupInstance {
                tb_field: "cg".to_string(),
                covgroup: CovgroupId(0),
            },
            point: "p".to_string(),
            bin: "b".to_string(),
        };
        let f = single_block_lifecycle(vec![Stmt::Assign(LocalId(0), read)], Terminator::Return);
        assert_eq!(lifecycle_shareable_kind(&f), Some(LifecycleEmit::Plain));
    }
}
