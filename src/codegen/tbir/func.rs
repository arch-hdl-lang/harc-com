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
    ECx, escape_c, expr_cpp, fmt_arg_cpp, helper_cpp_name, lane_width, port_read, port_signal,
    wide_words_over_128,
};
use crate::codegen::cpp_tb::EmitError;
use crate::ir::{
    BusBindingSchema, CallTarget, Expr, FileLogLevel, FmtArgs, IrType, LogLevel, PredSrc,
    RecordSchema, Stmt, TbFunction, TbProgram, Terminator, TransactorMethodSchema,
    TransactorSchema, WaitMode,
};
use std::collections::HashMap;
use std::fmt::Write as _;

const INDENT: &str = "    ";

/// Names that the surrounding scaffolding owns; user locals colliding
/// with them get a `_u_` prefix so the loop-switch body cannot shadow
/// captured references (`errors`, `dut`, ...).
const RESERVED: &[&str] = &[
    "dut", "tfp", "ctx", "errors", "cycle_count", "trace", "log_ctx", "tick", "sched", "argc",
    "argv", "now_ps", "clocks_", "target", "harc_rng", "test_sel", "sim_log_line", "sim_logf_line",
    "eval_clocks_until", "_tb", "_fatal", "_trace_time", "_wave_path", "_checkers",
    "_post_eval_services", "_auto_cov_reports", "_run_slot", "_slot", "_harc_trace_dump_next",
    "_harc_trace_dump_at", "__bb", "__done", "_wu_budget", "_wu_satisfied",
    // C++ keywords that are plausible HARC identifiers.
    "auto", "bool", "break", "case", "char", "class", "const", "continue", "default", "delete",
    "do", "double", "else", "enum", "false", "float", "for", "if", "int", "long", "namespace",
    "new", "operator", "private", "protected", "public", "register", "return", "short", "signed",
    "static", "struct", "switch", "template", "this", "true", "union", "unsigned", "using",
    "virtual", "void", "while",
];

/// Per-local emitted C++ names. Lowering already deduplicated names
/// within the function; this only steps around scaffolding collisions.
fn cpp_local_names(func: &TbFunction) -> Vec<String> {
    func.locals
        .iter()
        .map(|l| {
            if RESERVED.contains(&l.name.as_str()) {
                format!("_u_{}", l.name)
            } else {
                l.name.clone()
            }
        })
        .collect()
}

/// Emit one function as a loop-switch at `depth` indentation levels,
/// wrapped in its own brace scope so multiple functions (run + check)
/// can share one coroutine body without name collisions. `bindings`
/// is the owning testbench's bus-binding table — the metadata that
/// expands `CallTarget::TransactorMethod` call edges into the
/// canonical req/rsp wire protocol.
pub(super) fn emit_function(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
    records: &[RecordSchema],
    bindings: &[BusBindingSchema],
    lanes: &HashMap<String, u32>,
    depth: usize,
) -> Result<(), EmitError> {
    let names = cpp_local_names(func);
    let cx = ECx { func, names: &names, lanes };
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let pad3 = INDENT.repeat(depth + 3);

    writeln!(out, "{pad}{{ // {} (TB-IR loop-switch)", func.name).ok();
    declare_locals(out, prog, func, &names, 0, depth + 1)?;
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
                let cond = expr_cpp(&cx, c)?;
                writeln!(out, "{pad3}if ({cond}) {{ __bb = {}; }} else {{ __bb = {}; }}", t.0, f.0)
                    .ok();
            }
            Terminator::WaitCycles(n, clock, b) => {
                let n = expr_cpp(&cx, n)?;
                match clock {
                    None => {
                        writeln!(
                            out,
                            "{pad3}co_await harc_rt::wait_cycles(_slot, (uint32_t)({n}));"
                        )
                        .ok();
                    }
                    Some(c) => {
                        // `wait N cycles on <clock>` — mirror v1's
                        // inline eval_clocks_until loop (cpp_tb.rs,
                        // StmtKind::Wait with a clock): advance
                        // simulated time edge-by-edge until the named
                        // clock has seen N more rising edges, then run
                        // the checkers. v1 emits this inline (no
                        // coroutine yield) regardless of coroutine
                        // context — the main loop's full-primary-period
                        // stride is too coarse when the named clock is
                        // faster than the primary — so the loop-switch
                        // does the same: no co_await, identical
                        // scheduler interaction, identical cycle
                        // timing.
                        let idx = c.index;
                        writeln!(
                            out,
                            "{pad3}{{ long long _target = clocks_[{idx}].rising_count + \
                             (long long)({n}); while (clocks_[{idx}].rising_count < _target) {{"
                        )
                        .ok();
                        writeln!(out, "{pad3}{INDENT}long long _next = clocks_[0].next_edge_ps;")
                            .ok();
                        writeln!(
                            out,
                            "{pad3}{INDENT}for (auto& _ck : clocks_) if (_ck.next_edge_ps < \
                             _next) _next = _ck.next_edge_ps;"
                        )
                        .ok();
                        writeln!(out, "{pad3}{INDENT}eval_clocks_until(_next);").ok();
                        writeln!(out, "{pad3}}} for (auto& _c : _checkers) _c(); }}").ok();
                    }
                }
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::WaitCyclesSync(n, b) => {
                // v1's non-coroutine wait path (helper / testbench-
                // method lambda bodies): synchronous tick loop, no
                // scheduler yield.
                let n = expr_cpp(&cx, n)?;
                writeln!(out, "{pad3}for (int _w = 0; _w < {n}; _w++) tick();").ok();
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::WaitTimePs(ps, b) => {
                // Wall-clock wait — v1's inline emission for a Time
                // duration: advance absolute time, no coroutine yield,
                // no checker pass.
                writeln!(out, "{pad3}eval_clocks_until(now_ps + {ps});").ok();
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::Return => {
                writeln!(out, "{pad3}__done = true;").ok();
            }
            Terminator::Fatal(args) => {
                emit_log_call(out, &cx, "FATAL", None, args, depth + 3)?;
                writeln!(out, "{pad3}errors++; _fatal = true;").ok();
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
                let n = expr_cpp(&cx, cycles)?;
                writeln!(out, "{pad3}int64_t _wu_budget = (int64_t)({n});").ok();
                writeln!(
                    out,
                    "{pad3}bool _wu_satisfied = co_await harc_rt::wait_until_timeout(_slot, \
                     [&]{{ return {cond}; }}, (uint32_t)_wu_budget);"
                )
                .ok();
                writeln!(
                    out,
                    "{pad3}if (_wu_satisfied) {{ __bb = {}; }} else {{ errors++; __bb = {}; }}",
                    on_fire.0, on_timeout.0
                )
                .ok();
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

/// Forward declaration for a lowered pure helper, so source-order
/// emission supports helper-to-helper calls in any order.
pub(super) fn emit_helper_prototype(out: &mut String, func: &TbFunction) {
    let names = cpp_local_names(func);
    let ret_ty = if func.ret.is_some() { "uint64_t" } else { "void" };
    let params = names[..func.params.len()]
        .iter()
        .map(|n| format!("uint64_t {n}"))
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(out, "static {ret_ty} {}({params});", helper_cpp_name(&func.name)).ok();
}

/// Emit one `FunctionKind::Helper` function (a lowered *pure* helper)
/// as a file-scope C++ function. Pure helpers contain only scalar
/// computation — no DUT access, no logging, no suspension — so the
/// loop-switch body is restricted to `Assign` statements and
/// `Jump`/`Branch`/`Return` terminators; anything else is a lowering
/// bug surfaced as an `EmitError`.
///
/// Signature convention: the first `params.len()` locals ARE the
/// parameters (TB-IR convention), so they emit as parameters and are
/// not re-declared in the body. Everything is `uint64_t`, matching the
/// loop-switch local model.
pub(super) fn emit_helper_function(out: &mut String, func: &TbFunction) -> Result<(), EmitError> {
    let names = cpp_local_names(func);
    // Pure helpers are scalar-only: no DUT access, so no lane table.
    let empty_lanes = HashMap::new();
    let cx = ECx { func, names: &names, lanes: &empty_lanes };
    let nparams = func.params.len();

    let ret_ty = if func.ret.is_some() { "uint64_t" } else { "void" };
    let params = names[..nparams]
        .iter()
        .map(|n| format!("uint64_t {n}"))
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(out, "static {ret_ty} {}({params}) {{", helper_cpp_name(&func.name)).ok();
    for n in &names[nparams..] {
        writeln!(out, "{INDENT}uint64_t {n} = 0; (void){n};").ok();
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
                Stmt::Assign(l, e) => {
                    let name = &names[l.index()];
                    let e = expr_cpp(&cx, e)?;
                    writeln!(out, "{pad3}{name} = {e};").ok();
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
                let cond = expr_cpp(&cx, c)?;
                writeln!(out, "{pad3}if ({cond}) {{ __bb = {}; }} else {{ __bb = {}; }}", t.0, f.0)
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
        Some(_) => writeln!(out, "{pad2}default: return 0;").ok(),
        None => writeln!(out, "{pad2}default: return;").ok(),
    };
    writeln!(out, "{INDENT}{INDENT}}}").ok();
    writeln!(out, "{INDENT}}}").ok();
    writeln!(out, "}}").ok();
    Ok(())
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
            if let Expr::Call(CallTarget::TransactorMethod { bus_field, method }, args) = e {
                return emit_transactor_call(
                    out, cx, bindings, *l, bus_field, method, args, depth,
                );
            }
            let name = &names[l.index()];
            let e = expr_cpp(cx, e)?;
            writeln!(out, "{pad}{name} = {e};").ok();
        }
        Stmt::RecordInit(l, r) => {
            let name = &names[l.index()];
            let rec = records.get(r.index()).ok_or_else(|| {
                EmitError(format!(
                    "tbir: RecordInit of `{name}` in {} references missing record r{}",
                    func.name, r.0
                ))
            })?;
            writeln!(out, "{pad}{name} = {}{{}};", rec.name).ok();
        }
        Stmt::TransactorCall { dest, call } => {
            let Expr::Call(CallTarget::TransactorMethod { bus_field, method }, args) = call
            else {
                return Err(EmitError(format!(
                    "tbir: TransactorCall in {} carries a non-call-edge payload \
                     (verifier invariant violated)",
                    func.name
                )));
            };
            // Resolve the instance field to its transactor type via the
            // owner testbench — the lambda is named `<Type>_<method>`,
            // mirroring v1's hookable lambda naming.
            let xname = func
                .owner
                .and_then(|o| prog.testbenches.get(o.index()))
                .and_then(|tb| {
                    tb.transactor_fields
                        .iter()
                        .find(|(f, _)| f == bus_field)
                        .map(|(_, xid)| prog.transactor(*xid).name.as_str())
                })
                .ok_or_else(|| {
                    EmitError(format!(
                        "tbir: TransactorCall `{bus_field}.{method}` in {} does not \
                         resolve through the owner testbench",
                        func.name
                    ))
                })?;
            let mut rendered = Vec::with_capacity(args.len());
            for a in args {
                rendered.push(expr_cpp(cx, a)?);
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
        Stmt::RecordFieldWrite { local, field, value } => {
            let name = &names[local.index()];
            let e = expr_cpp(cx, value)?;
            writeln!(out, "{pad}{name}.{field} = {e};").ok();
        }
        Stmt::TbFieldWrite { field, value } => {
            let e = expr_cpp(cx, value)?;
            writeln!(out, "{pad}_tb.{field} = {e};").ok();
        }
        Stmt::DutWrite(p, e) => {
            let sig = port_signal(p);
            match p.lane {
                // Packed multi-lane port: bit-deposit through the
                // runtime helper; unpacked-array port: raw subscript
                // (v1's emit_signal_assignment split).
                Some(lane) => {
                    let e = expr_cpp(cx, e)?;
                    match lane_width(cx, p) {
                        Some(w) => {
                            writeln!(
                                out,
                                "{pad}harc_rt::harc_vec_lane_write<{w}>({sig}, \
                                 (std::size_t)({lane}), {e});"
                            )
                            .ok();
                        }
                        None => {
                            writeln!(out, "{pad}{sig}[{lane}] = {e};").ok();
                        }
                    }
                }
                None => {
                    // > 128-bit literal into a wide signal: word-list
                    // path through the checked helper, parameterized
                    // by the words the value actually needs (v1).
                    if let Some(words) = wide_words_over_128(e) {
                        let req = words
                            .iter()
                            .rposition(|w| *w != 0)
                            .map_or(1, |i| i + 1);
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
                        let e = expr_cpp(cx, e)?;
                        writeln!(out, "{pad}harc_rt::harc_assign({sig}, {e});").ok();
                    }
                }
            }
        }
        Stmt::DutRead(l, p) => {
            let name = &names[l.index()];
            writeln!(out, "{pad}{name} = {};", port_read(cx, p)).ok();
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
                    writeln!(out, "{pad}errors++;").ok();
                }
                "FATAL" => {
                    writeln!(out, "{pad}errors++; _fatal = true;").ok();
                }
                _ => {}
            }
        }
        Stmt::AssertCheck { cond, on_fail } => {
            let cond = expr_cpp(cx, cond)?;
            writeln!(out, "{pad}if (!({cond})) {{").ok();
            emit_log_call(out, cx, "FAIL", None, on_fail, depth + 1)?;
            writeln!(out, "{pad}{INDENT}errors++;").ok();
            writeln!(out, "{pad}}}").ok();
        }
        Stmt::CovReport(inst) => {
            writeln!(out, "{pad}_tb.{}.report();", inst.tb_field).ok();
        }
        Stmt::FailDiag { guard, args } => match guard {
            // Per-sub-predicate breakdown: log only if the predicate
            // is STILL false at timeout (v1's "not yet true:" lines).
            // No errors++ — the WaitUntilTimeout terminator already
            // bumped it once on the timeout edge.
            Some(g) => {
                let g = expr_cpp(cx, g)?;
                writeln!(out, "{pad}if (!({g})) {{").ok();
                emit_log_call(out, cx, "FAIL", None, args, depth + 1)?;
                writeln!(out, "{pad}}}").ok();
            }
            None => emit_log_call(out, cx, "FAIL", None, args, depth)?,
        },
    }
    Ok(())
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
    bindings: &[BusBindingSchema],
    dest: crate::ir::LocalId,
    bus_field: &str,
    method: &str,
    args: &[Expr],
    depth: usize,
) -> Result<(), EmitError> {
    let pad = INDENT.repeat(depth);
    let func = cx.func;
    let names = cx.names;
    let schema = bindings
        .iter()
        .find(|b| b.field == bus_field)
        .and_then(|b| b.methods.iter().find(|m| m.name == method))
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
    let wire = |sig: &str| format!("dut->{bus_field}_{method}_{sig}");
    let budget_wait = |out: &mut String, sig: &str| {
        writeln!(
            out,
            "{pad}{{ int _b = 16; while (!{} && _b > 0) {{ co_await \
             harc_rt::wait_cycles(_slot, 1); _b--; }} }}",
            wire(sig)
        )
        .ok();
    };
    let trace_event = |out: &mut String, phase: &str| {
        writeln!(
            out,
            "{pad}trace.tlm_call(cycle_count, \"\", \"{}\", \"{}\", \"{phase}\", \"initiator\");",
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
    budget_wait(out, "req_ready");
    writeln!(out, "{pad}co_await harc_rt::wait_cycles(_slot, 1);").ok();
    writeln!(out, "{pad}{} = 0;", wire("req_valid")).ok();
    writeln!(out, "{pad}{} = 1;", wire("rsp_ready")).ok();
    budget_wait(out, "rsp_valid");
    trace_event(out, "response");
    if schema.has_ret {
        // Capture BEFORE the trailing tick — rsp_data is valid in the
        // same cycle as rsp_valid (mirrors v1; for result-less or
        // discarded calls v1 skips the capture but still completes the
        // rsp handshake).
        writeln!(
            out,
            "{pad}{} = harc_rt::harc_read({});",
            names[dest.index()],
            wire("rsp_data")
        )
        .ok();
    }
    writeln!(out, "{pad}co_await harc_rt::wait_cycles(_slot, 1);").ok();
    writeln!(out, "{pad}{} = 0;", wire("rsp_ready")).ok();
    Ok(())
}

/// Hoisted local declarations for a loop-switch body, skipping the
/// first `skip` locals (function parameters — they arrive as C++
/// parameters and must not be re-declared). Record-typed locals hoist
/// as default-constructed structs; the `RecordInit` at the source
/// `let` site re-runs the field defaults (v1 declares at the let
/// site, so loop iterations re-default-construct).
fn declare_locals(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
    names: &[String],
    skip: usize,
    depth: usize,
) -> Result<(), EmitError> {
    let pad = INDENT.repeat(depth);
    for (l, n) in func.locals.iter().zip(names).skip(skip) {
        if let IrType::Record(r) = l.ty {
            let rec = prog.records.get(r.index()).ok_or_else(|| {
                EmitError(format!(
                    "tbir: local `{n}` in {} references missing record r{}",
                    func.name, r.0
                ))
            })?;
            writeln!(out, "{pad}{} {n}{{}}; (void){n};", rec.name).ok();
        } else {
            writeln!(out, "{pad}uint64_t {n} = 0; (void){n};").ok();
        }
    }
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
/// Deliberately NOT emitted (v1 emits them, but they are inert dead
/// text under this subset and land with their constructs): the
/// `<Type>_<method>_pre`/`_post` hook vectors and their fan-out loops
/// (statement-level `on obj.method pre/post` hooks are rejected at
/// lowering), the transactor instance struct, and the per-instance
/// heartbeat stamps (no `idle()` predicates in the subset).
pub(super) fn emit_method(
    out: &mut String,
    prog: &TbProgram,
    schema: &TransactorSchema,
    m: &TransactorMethodSchema,
    depth: usize,
) -> Result<(), EmitError> {
    let func = prog.function(m.function);
    let names = cpp_local_names(func);
    // Method bodies poke the DUT directly but carry no `--sv` packed-
    // lane table of their own (no fixture combines transactor methods
    // with packed Vec lanes); an empty table falls back to raw
    // subscripts, matching v1 for that case.
    let empty_lanes = HashMap::new();
    let cx = ECx {
        func,
        names: &names,
        lanes: &empty_lanes,
    };
    let nparams = func.params.len();
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let pad3 = INDENT.repeat(depth + 3);

    let ret_ty = if func.ret.is_some() { "uint64_t" } else { "void" };
    let params = names[..nparams]
        .iter()
        .map(|n| format!("uint64_t {n}"))
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(
        out,
        "{pad}auto {}_{} = [&]({params}) -> {ret_ty} {{",
        schema.name, m.name
    )
    .ok();
    declare_locals(out, prog, func, &names, nparams, depth + 1)?;
    writeln!(out, "{pad1}int __bb = {};", func.entry.0).ok();
    writeln!(out, "{pad1}while (true) {{").ok();
    writeln!(out, "{pad2}switch (__bb) {{").ok();
    for (bi, block) in func.blocks.iter().enumerate() {
        writeln!(out, "{pad2}case {bi}: {{").ok();
        for s in &block.stmts {
            // Method bodies have no testbench bus-binding scope — a
            // bus-bound call edge in here is a lowering bug and errors
            // inside `emit_stmt` (empty binding table).
            emit_stmt(out, prog, &cx, &[], &[], s, depth + 3)?;
        }
        match &block.terminator {
            Terminator::Jump(b) => {
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::Branch(c, t, f) => {
                let cond = expr_cpp(&cx, c)?;
                writeln!(out, "{pad3}if ({cond}) {{ __bb = {}; }} else {{ __bb = {}; }}", t.0, f.0)
                    .ok();
            }
            Terminator::WaitCycles(n, None, b) => {
                // v1's synchronous wait: one tick() per cycle.
                let n = expr_cpp(&cx, n)?;
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
            Terminator::Return => match func.ret {
                Some(r) => {
                    writeln!(out, "{pad3}return {};", names[r.index()]).ok();
                }
                None => {
                    writeln!(out, "{pad3}return;").ok();
                }
            },
            other @ (Terminator::WaitCycles(_, Some(_), _)
            | Terminator::WaitCyclesSync(_, _)
            | Terminator::WaitTimePs(_, _)
            | Terminator::WaitUntilTimeout { .. }
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
        Some(_) => writeln!(out, "{pad2}default: return 0;").ok(),
        None => writeln!(out, "{pad2}default: return;").ok(),
    };
    writeln!(out, "{pad2}}}").ok(); // switch
    writeln!(out, "{pad1}}}").ok(); // while
    writeln!(out, "{pad}}};").ok(); // lambda
    Ok(())
}

/// Join wait-until sub-predicates the way v1 does: a single predicate
/// emits bare; multiple emit as a parenthesized `&&` chain (`all of`)
/// or `||` chain (`any of`).
fn preds_cpp(
    cx: &ECx<'_>,
    preds: &[PredSrc],
    mode: WaitMode,
) -> Result<String, EmitError> {
    if preds.is_empty() {
        return Err(EmitError(format!(
            "tbir: wait-until terminator with no predicates in {} — lowering bug",
            cx.func.name
        )));
    }
    if preds.len() == 1 {
        return expr_cpp(cx, &preds[0].expr);
    }
    let joiner = match mode {
        WaitMode::Single | WaitMode::AllOf => " && ",
        WaitMode::AnyOf => " || ",
    };
    let parts = preds
        .iter()
        .map(|p| expr_cpp(cx, &p.expr))
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
            write!(out, "{pad}sim_log_line(\"{sev}\", \"{}\"", escape_c(&args.fmt)).ok();
        }
    }
    for a in &args.args {
        let rendered = fmt_arg_cpp(cx, a)?;
        write!(out, ", {rendered}").ok();
    }
    writeln!(out, ");").ok();
    Ok(())
}
