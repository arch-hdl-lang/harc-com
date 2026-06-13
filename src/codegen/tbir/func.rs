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
    ECx, comp_base_cpp, escape_c, expr_cpp, fmt_arg_cpp, helper_cpp_name, lane_width, port_read,
    port_signal, probe_read_accessor, wide_words_over_128,
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
    depth: usize,
) -> Result<(), EmitError> {
    let names = cpp_local_names(func);
    let cx = ECx { func, names: &names, lanes, self_subst: None, dut_type, trace_component: "" };
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
            Terminator::Randomize {
                constraints, succ, ..
            } => {
                // Splice in v1's Z3-solve snippet for this site (built by
                // `cpp_tb::emit_randomize_snippets`, indexed by the
                // `ConstraintRef`, pre-indented at this body depth). The
                // solve writes the record fields back into the target
                // local and emits the trace event, exactly like v1.
                let snippet = randomize_snippets.get(constraints.index()).ok_or_else(|| {
                    EmitError(format!(
                        "tbir: Randomize in {} references missing constraint snippet c{}",
                        func.name, constraints.0
                    ))
                })?;
                out.push_str(snippet);
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
    let names = cpp_local_names(func);
    // tseq bodies hold no packed-lane DUT access (no DUT at all), so no
    // probe access either (`dut_type` unused → `""`).
    let empty_lanes = HashMap::new();
    let cx = ECx {
        func,
        names: &names,
        lanes: &empty_lanes,
        self_subst: None,
        dut_type: "",
        trace_component: "",
    };
    let nparams = func.params.len();
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let pad3 = INDENT.repeat(depth + 3);

    // The element record name (for the `std::vector<Record>` return type).
    let IrType::RecordSeq(rid) = func
        .ret
        .map(|r| func.local(r).ty.clone())
        .unwrap_or(IrType::Unknown)
    else {
        return Err(EmitError(format!(
            "tbir: tseq `{}` has no RecordSeq return accumulator (lowering bug)",
            func.name
        )));
    };
    let elem = records
        .get(rid.index())
        .ok_or_else(|| {
            EmitError(format!(
                "tbir: tseq `{}` references missing element record r{}",
                func.name, rid.0
            ))
        })?
        .name
        .clone();

    let params = names[..nparams]
        .iter()
        .map(|n| format!("uint64_t {n}"))
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(
        out,
        "{pad}auto {} = [&]({params}) -> std::vector<{elem}> {{",
        func.name
    )
    .ok();
    declare_locals(out, prog, func, &names, nparams, depth + 1)?;
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
                let cond = expr_cpp(&cx, c)?;
                writeln!(out, "{pad3}if ({cond}) {{ __bb = {}; }} else {{ __bb = {}; }}", t.0, f.0)
                    .ok();
            }
            Terminator::WaitCycles(n, None, b) | Terminator::WaitCyclesSync(n, b) => {
                // v1's synchronous lambda wait: one tick() per cycle.
                let n = expr_cpp(&cx, n)?;
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
                constraints, succ, ..
            } => {
                let snippet = randomize_snippets.get(constraints.index()).ok_or_else(|| {
                    EmitError(format!(
                        "tbir: Randomize in tseq {} references missing constraint snippet c{}",
                        func.name, constraints.0
                    ))
                })?;
                out.push_str(snippet);
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
            | Terminator::WaitTimePs(_, _)
            | Terminator::WaitUntilTimeout { .. }
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
    writeln!(out, "{pad}}};").ok();
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
    // Pure helpers are scalar-only: no DUT access, so no lane table and
    // no probe access (`dut_type` unused → `""`).
    let empty_lanes = HashMap::new();
    let cx = ECx { func, names: &names, lanes: &empty_lanes, self_subst: None, dut_type: "", trace_component: "" };
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
        Stmt::TransactorStateWrite { instance, field, value } => {
            let e = expr_cpp(cx, value)?;
            writeln!(out, "{pad}{instance}.{field} = {e};").ok();
        }
        Stmt::DutWrite(p, e) if matches!(p.access, crate::ir::PortAccess::Force) => {
            // `dut.<force_probe> = expr` → the two-store drv+en pair the
            // bound SV stub picks up to procedurally force the target
            // (docs/probe-signals.md §4.1; v1's `emit_signal_assignment`
            // force-probe arm). The read-side mangled accessor is the
            // base; `_drv`/`_en` derive by suffix.
            let base = probe_read_accessor(cx.dut_type, p);
            let val = expr_cpp(cx, e)?;
            writeln!(out, "{pad}dut->rootp->{base}_drv = {val};").ok();
            writeln!(out, "{pad}dut->rootp->{base}_en = 1;").ok();
        }
        Stmt::DutWrite(p, e) => {
            let sig = port_signal(cx, p);
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
        Stmt::ProbeRelease(p) => {
            // `release dut.<force_probe>` → clear the enable wire so the
            // bound SV stub releases its procedural force (v1's `release`
            // → `<mangled>_en = 0`). Lowering guaranteed `access == Force`.
            let base = probe_read_accessor(cx.dut_type, p);
            writeln!(out, "{pad}dut->rootp->{base}_en = 0;").ok();
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
        Stmt::ScoreboardOp { field, op, nested_path, .. } => {
            use crate::ir::ScoreboardOp;
            // `None` → testbench field (`_tb.<field>`); `Some(path)` →
            // env-nested data scoreboard, accessed by the run-scope path.
            // A `self`-rooted path is a self-relative sub-scoreboard inside
            // a component body — re-root `self` at the running instance via
            // `self_subst` (the cycle-trigger / on-handler poke form).
            let base = match nested_path {
                Some(p) if p.first().map(String::as_str) == Some("self") => {
                    let root = cx.self_subst.unwrap_or("self");
                    std::iter::once(root.to_string())
                        .chain(p.iter().skip(1).cloned())
                        .collect::<Vec<_>>()
                        .join(".")
                }
                Some(p) => p.join("."),
                None => format!("_tb.{field}"),
            };
            match op {
                ScoreboardOp::QueuePush { queue, value } => {
                    let e = expr_cpp(cx, value)?;
                    writeln!(out, "{pad}{base}.{queue}.push({e});").ok();
                }
                ScoreboardOp::QueuePop { queue, dest } => {
                    let name = &names[dest.index()];
                    writeln!(out, "{pad}{name} = {base}.{queue}.pop();").ok();
                }
                ScoreboardOp::ScalarWrite { scalar, value } => {
                    let e = expr_cpp(cx, value)?;
                    writeln!(out, "{pad}{base}.{scalar} = {e};").ok();
                }
            }
        }
        // Composite-component scalar field write — `self.count = ...`
        // inside a method body, or `env.sb.errors = ...` from the test.
        Stmt::ComponentFieldWrite { base, field, value } => {
            let e = expr_cpp(cx, value)?;
            writeln!(out, "{pad}{}.{field} = {e};", comp_base_cpp(base)).ok();
        }
        // `emit observed(v)` inside a method body: fan the args out to
        // every subscriber registered on `self.<event>`, then bump the
        // component's `_last_out_cycle` heartbeat (v1's emit lowering).
        Stmt::ComponentEmit { base, event, args } => {
            let mut rendered = Vec::with_capacity(args.len());
            for a in args {
                rendered.push(expr_cpp(cx, a)?);
            }
            let csv = rendered.join(", ");
            let recv = comp_base_cpp(base);
            writeln!(out, "{pad}for (auto& _s : {recv}.{event}) _s({csv});").ok();
            writeln!(out, "{pad}{recv}._last_out_cycle = (uint64_t)cycle_count;").ok();
        }
        // `env.source.publish(3)` — a free `<Comp>_<method>(receiver,
        // args)` lambda call (v1's `emit_component_method` shape).
        Stmt::ComponentCall { base, component, method, args, dest } => {
            let comp = prog.components.get(component.index()).ok_or_else(|| {
                EmitError(format!(
                    "tbir: ComponentCall in {} references missing component c{}",
                    func.name, component.0
                ))
            })?;
            let mut rendered = vec![comp_base_cpp(base)];
            for a in args {
                rendered.push(expr_cpp(cx, a)?);
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
        // `yield t` — append a record value to the sequence accumulator
        // (v1's `_result.push_back(t)`).
        Stmt::SeqPush { seq, value } => {
            let name = &names[seq.index()];
            let v = expr_cpp(cx, value)?;
            writeln!(out, "{pad}{name}.push_back({v});").ok();
        }
        Stmt::TlmFork(desc) => emit_tlm_fork(out, cx, bindings, desc, depth)?,
        Stmt::TlmJoinAll(pending) => emit_tlm_join_all(out, cx, bindings, pending, depth)?,
    }
    Ok(())
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
    let binding = resolve_binding(bindings, &desc.bus_field, &desc.method, cx.func)?;
    let wire = |sig: &str| format!("dut->{}", binding.wire_name(&desc.method, sig));
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
    emit_tlm_trace(out, &pad, cx.trace_component, &desc.bus_field, &desc.method, "request", "initiator", tag_arg.as_deref());
    writeln!(out, "{pad}{} = 1;", wire("req_valid")).ok();
    writeln!(
        out,
        "{pad}{{ int _b = 16; while (!{} && _b > 0) {{ co_await harc_rt::wait_cycles(_slot, 1); _b--; }} }}",
        wire("req_ready")
    )
    .ok();
    writeln!(out, "{pad}co_await harc_rt::wait_cycles(_slot, 1);").ok();
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
        emit_tagged_tlm_join_all(out, cx, bindings, pending, depth)
    } else {
        emit_ordered_tlm_join_all(out, cx, bindings, pending, depth)
    }
}

/// Issue-order FIFO drain for `blocking` forks (v1's
/// `emit_ordered_tlm_join_all`): for each pending fork in issue order,
/// raise `rsp_ready`, budget-wait `rsp_valid` (64-cycle), capture
/// `rsp_data` into the dest, tick, drop `rsp_ready`.
fn emit_ordered_tlm_join_all(
    out: &mut String,
    cx: &ECx<'_>,
    bindings: &[BusBindingSchema],
    pending: &[crate::ir::TlmForkDesc],
    depth: usize,
) -> Result<(), EmitError> {
    let pad = INDENT.repeat(depth);
    let names = cx.names;
    for p in pending {
        let binding = resolve_binding(bindings, &p.bus_field, &p.method, cx.func)?;
        let wire = |sig: &str| format!("dut->{}", binding.wire_name(&p.method, sig));
        writeln!(out, "{pad}// join_all bus.{} response", p.method).ok();
        writeln!(out, "{pad}{} = 1;", wire("rsp_ready")).ok();
        writeln!(
            out,
            "{pad}{{ int _b = 64; while (!{} && _b > 0) {{ co_await harc_rt::wait_cycles(_slot, 1); _b--; }} }}",
            wire("rsp_valid")
        )
        .ok();
        if let (Some(dest), true) = (p.dest, p.has_ret) {
            writeln!(
                out,
                "{pad}{} = harc_rt::harc_read({});",
                names[dest.index()],
                wire("rsp_data")
            )
            .ok();
        }
        emit_tlm_trace(out, &pad, cx.trace_component, &p.bus_field, &p.method, "response", "initiator", None);
        writeln!(out, "{pad}co_await harc_rt::wait_cycles(_slot, 1);").ok();
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
    bindings: &[BusBindingSchema],
    pending: &[crate::ir::TlmForkDesc],
    depth: usize,
) -> Result<(), EmitError> {
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let pad3 = INDENT.repeat(depth + 3);
    let names = cx.names;
    writeln!(out, "{pad}{{").ok();
    writeln!(out, "{pad1}int _tlm_pending = {};", pending.len()).ok();
    for idx in 0..pending.len() {
        writeln!(out, "{pad1}bool _tlm_seen_{idx} = false;").ok();
    }
    writeln!(out, "{pad1}int _tlm_budget = 256;").ok();
    writeln!(out, "{pad1}while (_tlm_pending > 0 && _tlm_budget > 0) {{").ok();
    for p in pending {
        let binding = resolve_binding(bindings, &p.bus_field, &p.method, cx.func)?;
        writeln!(out, "{pad2}dut->{} = 0;", binding.wire_name(&p.method, "rsp_ready")).ok();
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
        let binding = resolve_binding(bindings, &p.bus_field, &p.method, cx.func)?;
        let wire = |sig: &str| format!("dut->{}", binding.wire_name(&p.method, sig));
        writeln!(
            out,
            "{pad2}if (!_tlm_seen_{idx} && {} && {} == {tag}) {{",
            wire("rsp_valid"),
            wire("rsp_tag")
        )
        .ok();
        if let (Some(dest), true) = (p.dest, p.has_ret) {
            writeln!(
                out,
                "{pad3}{} = harc_rt::harc_read({});",
                names[dest.index()],
                wire("rsp_data")
            )
            .ok();
        }
        emit_tlm_trace(out, &pad3, cx.trace_component, &p.bus_field, &p.method, "response", "initiator", Some(&format!("(int64_t){tag}")));
        writeln!(out, "{pad3}{} = 1;", wire("rsp_ready")).ok();
        writeln!(out, "{pad3}_tlm_seen_{idx} = true;").ok();
        writeln!(out, "{pad3}_tlm_pending--;").ok();
        writeln!(out, "{pad3}_tlm_accept = true;").ok();
        writeln!(out, "{pad2}}}").ok();
    }
    writeln!(out, "{pad2}co_await harc_rt::wait_cycles(_slot, 1);").ok();
    writeln!(out, "{pad2}if (!_tlm_accept) _tlm_budget--;").ok();
    writeln!(out, "{pad1}}}").ok();
    for p in pending {
        let binding = resolve_binding(bindings, &p.bus_field, &p.method, cx.func)?;
        writeln!(out, "{pad1}dut->{} = 0;", binding.wire_name(&p.method, "rsp_ready")).ok();
    }
    writeln!(out, "{pad}}}").ok();
    Ok(())
}

/// Resolve a bus binding for a fork/join wire emission, with the same
/// "verifier should have rejected it" hard-error contract as
/// `emit_transactor_call`.
fn resolve_binding<'a>(
    bindings: &'a [BusBindingSchema],
    bus_field: &str,
    method: &str,
    func: &TbFunction,
) -> Result<&'a BusBindingSchema, EmitError> {
    bindings.iter().find(|b| b.field == bus_field).ok_or_else(|| {
        EmitError(format!(
            "tbir: unresolved fork/join binding `{bus_field}.{method}` in {} — \
             verifier should have rejected it",
            func.name
        ))
    })
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
    let binding = bindings.iter().find(|b| b.field == bus_field).ok_or_else(|| {
        EmitError(format!(
            "tbir: unresolved transactor call `{bus_field}.{method}` in {} — \
             verifier should have rejected it",
            func.name
        ))
    })?;
    let schema = binding.methods.iter().find(|m| m.name == method).ok_or_else(|| {
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
    let wire = |sig: &str| format!("dut->{}", binding.wire_name(method, sig));
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
        match l.ty {
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
            _ => {
                writeln!(out, "{pad}uint64_t {n} = 0; (void){n};").ok();
            }
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
    // Method bodies access the bound DUT (`schema.dut_type`); probes are
    // never declared on a transactor DUT (lowering rejects it), but pass
    // the real type so any `PortAccess` would mangle correctly.
    let cx = ECx {
        func,
        names: &names,
        lanes: &empty_lanes,
        self_subst: None,
        dut_type: &schema.dut_type,
        trace_component: "",
    };
    let nparams = func.params.len();
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let pad3 = INDENT.repeat(depth + 3);

    let ret_ty = if func.ret.is_some() { "uint64_t" } else { "void" };
    // A record-typed param (`send(t: RegOp)`) is taken by value as the
    // record struct — the body binds it and reads its fields, mirroring
    // v1's by-value struct param; every scalar param widens to uint64_t.
    let params = names[..nparams]
        .iter()
        .enumerate()
        .map(|(i, n)| match func.locals[i].ty {
            IrType::Record(r) => format!("{} {n}", prog.records[r.index()].name),
            IrType::RecordSeq(r) => format!("std::vector<{}> {n}", prog.records[r.index()].name),
            _ => format!("uint64_t {n}"),
        })
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
            | Terminator::Randomize { .. }
            | Terminator::Fatal(_)) => {
                // Lowering rejects these inside method bodies (or, for
                // Fatal, never produces the terminator at all). A
                // `randomize` in a method body is out of subset — the
                // constraint-IR problem table only catalogs test/tseq
                // sites, so a method-body solve has no problem-id.
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
    comp: &crate::ir::ComponentSchema,
    m: &crate::ir::ComponentMethodSchema,
    depth: usize,
) -> Result<(), EmitError> {
    let lambda = format!("{}_{}", comp.name, m.name);
    emit_component_fn_lambda(out, prog, comp, m.function, &lambda, depth)
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
    depth: usize,
) -> Result<(), EmitError> {
    let lambda = on_handler_lambda_name(comp, oh);
    emit_component_fn_lambda(out, prog, comp, oh.function, &lambda, depth)
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
    depth: usize,
) -> Result<(), EmitError> {
    let lambda = periodic_handler_lambda_name(comp, ph);
    emit_component_fn_lambda(out, prog, comp, ph.function, &lambda, depth)
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
    depth: usize,
) -> Result<(), EmitError> {
    let lambda = cycle_handler_lambda_name(comp, ch);
    emit_component_fn_lambda(out, prog, comp, ch.function, &lambda, depth)
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
    depth: usize,
) -> Result<(), EmitError> {
    let lambda = watchdog_lambda_name(comp, w);
    emit_component_fn_lambda(out, prog, comp, w.function, &lambda, depth)
}

/// The free-lambda name for a watchdog body (`<Comp>_watchdog<fid>`).
pub(super) fn watchdog_lambda_name(
    comp: &crate::ir::ComponentSchema,
    w: &crate::ir::WatchdogSchema,
) -> String {
    format!("{}_watchdog{}", comp.name, w.function.0)
}

/// Render a watchdog/periodic clause expr (`period` / `max_idle`) for
/// emission inside a per-instance `_checkers` closure. The clause was
/// lowered in `function`'s self-component context, so a field read is a
/// `ComponentField { SelfField }`; `instance` substitutes for `self`
/// since the closure has no `self` in scope.
pub(super) fn clause_expr_cpp(
    prog: &TbProgram,
    function: crate::ir::FunctionId,
    instance: &str,
    e: &Expr,
) -> Result<String, EmitError> {
    let func = prog.function(function);
    let names = cpp_local_names(func);
    let empty_lanes = HashMap::new();
    // Component clause exprs read component fields, not DUT probes.
    let cx = ECx {
        func,
        names: &names,
        lanes: &empty_lanes,
        self_subst: Some(instance),
        dut_type: "",
        trace_component: "",
    };
    expr_cpp(&cx, e)
}

/// Shared lambda emission for component methods and on-handlers: a free
/// `<lambda>(<Comp>& self, args...)` loop-switch over the lowered CFG.
fn emit_component_fn_lambda(
    out: &mut String,
    prog: &TbProgram,
    comp: &crate::ir::ComponentSchema,
    function: crate::ir::FunctionId,
    lambda: &str,
    depth: usize,
) -> Result<(), EmitError> {
    let func = prog.function(function);
    let names = cpp_local_names(func);
    let empty_lanes = HashMap::new();
    // Component method/on-handler bodies are host-side; no DUT probes.
    let cx = ECx {
        func,
        names: &names,
        lanes: &empty_lanes,
        self_subst: None,
        dut_type: "",
        trace_component: "",
    };
    let nparams = func.params.len();
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let pad3 = INDENT.repeat(depth + 3);

    let ret_ty = if func.ret.is_some() { "uint64_t" } else { "void" };
    // The receiver `self`, then one parameter per declared param — a
    // record param (`on in_ev(t)` with `event<TinyTxn>`) is taken by
    // value as the record struct; every other param widens to uint64_t.
    let mut params = vec![format!("{}& self", comp.name)];
    for (i, n) in names[..nparams].iter().enumerate() {
        let pty = match func.locals[i].ty {
            IrType::Record(r) => prog.records[r.index()].name.clone(),
            // A transaction-sequence param (a sequencer's
            // `hookable dispatch(txns: TSeq<RegOp>)`) is taken by value as
            // `std::vector<Record>`, matching the tseq generator's return.
            IrType::RecordSeq(r) => format!("std::vector<{}>", prog.records[r.index()].name),
            _ => "uint64_t".to_string(),
        };
        params.push(format!("{pty} {n}"));
    }
    let params = params.join(", ");
    writeln!(out, "{pad}auto {lambda} = [&]({params}) -> {ret_ty} {{").ok();
    declare_locals(out, prog, func, &names, nparams, depth + 1)?;
    writeln!(out, "{pad1}int __bb = {};", func.entry.0).ok();
    writeln!(out, "{pad1}while (true) {{").ok();
    writeln!(out, "{pad2}switch (__bb) {{").ok();
    for (bi, block) in func.blocks.iter().enumerate() {
        writeln!(out, "{pad2}case {bi}: {{").ok();
        for s in &block.stmts {
            // Record table visible so an `on`-handler body that inits a
            // record local (`let c : Completion`) resolves `RecordInit`.
            emit_stmt(out, prog, &cx, &prog.records, &[], s, depth + 3)?;
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
                let n = expr_cpp(&cx, n)?;
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
            | Terminator::Randomize { .. }
            | Terminator::Fatal(_)) => {
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
    match func.ret {
        Some(_) => writeln!(out, "{pad2}default: return 0;").ok(),
        None => writeln!(out, "{pad2}default: return;").ok(),
    };
    writeln!(out, "{pad2}}}").ok(); // switch
    writeln!(out, "{pad1}}}").ok(); // while
    writeln!(out, "{pad}}};").ok(); // lambda
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
pub(super) fn emit_target_actor(
    out: &mut String,
    prog: &TbProgram,
    actor: &crate::ir::TargetTlmActorSchema,
    bindings: &[BusBindingSchema],
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
        let method = &tm.name;
        let func = prog.function(tm.function);
        let names = cpp_local_names(func);
        let empty_lanes = HashMap::new();
        // Target-TLM responder bodies are wire-protocol only; no probes.
        // A downstream forwarded `back.read(...)` initiator trace event
        // carries the responder-instance name in its `component` field
        // (v1's `current_component_instance`), so set the trace context.
        let cx = ECx {
            func,
            names: &names,
            lanes: &empty_lanes,
            self_subst: None,
            dut_type: "",
            trace_component: instance,
        };
        let wire = |sig: &str| match binding {
            Some(b) => format!("dut->{}", b.wire_name(method, sig)),
            None => format!("dut->{bus_field}_{method}_{sig}"),
        };
        let slot_var = format!("_{instance}_{method}_target_slot");
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

        writeln!(out, "{pad}harc_rt::ThreadSlot {slot_var};").ok();
        writeln!(out, "{pad}sched.slots.push_back(&{slot_var});").ok();
        writeln!(
            out,
            "{pad}{slot_var}.thread = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{"
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
            writeln!(
                out,
                "{pad2}uint64_t {local} = harc_rt::harc_read({});",
                wire(arg)
            )
            .ok();
        }
        writeln!(out, "{pad2}co_await harc_rt::wait_cycles(_slot, 1);").ok();
        writeln!(out, "{pad2}{} = 0;", wire("req_ready")).ok();
        writeln!(out, "{pad2}{instance}._last_in_cycle = (uint64_t)cycle_count;").ok();
        trace_event(out, "request", depth + 2);

        // Responder body as a coroutine loop-switch. The args are already
        // declared above; declare the remaining locals (incl. the ret
        // slot) and run the switch. `Return` sets `__done`; the captured
        // ret local survives the loop for the response drive.
        writeln!(out, "{pad2}{{ // {} (TB-IR responder loop-switch)", func.name).ok();
        declare_locals(out, prog, func, &names, nparams, depth + 3)?;
        writeln!(out, "{}int __bb = {};", INDENT.repeat(depth + 3), func.entry.0).ok();
        writeln!(out, "{}bool __done = false;", INDENT.repeat(depth + 3)).ok();
        writeln!(out, "{}while (!__done) {{", INDENT.repeat(depth + 3)).ok();
        writeln!(out, "{}switch (__bb) {{", INDENT.repeat(depth + 4)).ok();
        let pad4 = INDENT.repeat(depth + 4);
        let pad5 = INDENT.repeat(depth + 5);
        for (bi, block) in func.blocks.iter().enumerate() {
            writeln!(out, "{pad4}case {bi}: {{").ok();
            for s in &block.stmts {
                // The responder body runs in test scope: a downstream
                // blocking TLM call edge (`back.read(...)` — nested
                // forwarding) resolves against the test's bus bindings,
                // so pass them through. `emit_transactor_call` uses
                // `co_await` (the responder body is a coroutine), and an
                // unresolved `back` surfaces as an EmitError there.
                emit_stmt(out, prog, &cx, &[], bindings, s, depth + 5)?;
            }
            match &block.terminator {
                Terminator::Jump(b) => {
                    writeln!(out, "{pad5}__bb = {};", b.0).ok();
                }
                Terminator::Branch(c, t, f) => {
                    let cond = expr_cpp(&cx, c)?;
                    writeln!(out, "{pad5}if ({cond}) {{ __bb = {}; }} else {{ __bb = {}; }}", t.0, f.0).ok();
                }
                Terminator::WaitCycles(n, None, b) => {
                    let n = expr_cpp(&cx, n)?;
                    writeln!(out, "{pad5}co_await harc_rt::wait_cycles(_slot, (uint32_t)({n}));").ok();
                    writeln!(out, "{pad5}__bb = {};", b.0).ok();
                }
                Terminator::WaitUntil { preds, mode, succ } => {
                    let cond = preds_cpp(&cx, preds, *mode)?;
                    writeln!(out, "{pad5}co_await harc_rt::wait_until(_slot, [&]{{ return {cond}; }});").ok();
                    writeln!(out, "{pad5}__bb = {};", succ.0).ok();
                }
                Terminator::Return => {
                    writeln!(out, "{pad5}__done = true;").ok();
                }
                other => {
                    return Err(EmitError(format!(
                        "tbir: target responder `{}` contains terminator {other:?} — \
                         lowering gate failed",
                        func.name
                    )));
                }
            }
            writeln!(out, "{pad5}break;").ok();
            writeln!(out, "{pad4}}}").ok();
        }
        writeln!(out, "{pad4}default: __done = true; break;").ok();
        writeln!(out, "{}}}", INDENT.repeat(depth + 4)).ok(); // switch
        writeln!(out, "{}}}", INDENT.repeat(depth + 3)).ok(); // while
        // Drive the response payload, trace, handshake.
        if tm.has_ret {
            let ret = func.ret.ok_or_else(|| {
                EmitError(format!(
                    "tbir: target responder `{}` declares a return but carries no ret slot",
                    func.name
                ))
            })?;
            writeln!(
                out,
                "{pad2}{INDENT}harc_rt::harc_assign({}, {});",
                wire("rsp_data"),
                &names[ret.index()]
            )
            .ok();
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
        writeln!(out, "{pad2}{instance}._last_out_cycle = (uint64_t)cycle_count;").ok();
        writeln!(out, "{pad1}}}").ok(); // while(true)
        writeln!(out, "{pad1}co_return;").ok();
        writeln!(out, "{pad}}}(&{slot_var});").ok();
    }
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
