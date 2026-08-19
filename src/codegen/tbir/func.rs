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
    comp_base_cpp, comp_base_cpp_subst_cx, escape_c, expr_cpp, fmt_arg_cpp, helper_cpp_name,
    lane_index_cpp, lane_width, port_read, port_signal, probe_read_accessor, wide_words_over_128,
    ECx,
};
use crate::ast::ExprKind;
use crate::codegen::cpp_tb::EmitError;
use crate::ir::{
    BlockId, BusBindingSchema, CallTarget, ConstraintRef, Expr, FileLogLevel, FmtArgs, IrType,
    LocalId, LogLevel, PredSrc, RecordSchema, Stmt, TbFunction, TbProgram, Terminator,
    TransactorMethodSchema, TransactorSchema, WaitMode,
};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

const INDENT: &str = "    ";

/// Names that the surrounding scaffolding owns; user locals colliding
/// with them get a `_u_` prefix so the loop-switch body cannot shadow
/// captured references (`errors`, `dut`, ...).
const RESERVED: &[&str] = &[
    "dut",
    "tfp",
    "ctx",
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
    "_run_slot",
    "_slot",
    "_harc_trace_dump_next",
    "_harc_trace_dump_at",
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

fn randomize_snippet_for(
    prog: &TbProgram,
    names: &[String],
    target: LocalId,
    constraints: ConstraintRef,
    snippets: &[String],
) -> Option<String> {
    let snippet = snippets.get(constraints.index())?;
    let site = prog.constraint_sites.get(constraints.index())?;
    let ExprKind::Ident(src) = &*site.target.kind else {
        return Some(snippet.clone());
    };
    let dst = names.get(target.index())?;
    if src.name == *dst {
        Some(snippet.clone())
    } else {
        Some(rewrite_cpp_ident(snippet, &src.name, dst))
    }
}

fn rewrite_cpp_ident(input: &str, src: &str, dst: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut it = input.char_indices().peekable();
    while let Some((start, ch)) = it.next() {
        if ch == '"' || ch == '\'' {
            let quote = ch;
            out.push(ch);
            let mut escaped = false;
            for (_, c) in it.by_ref() {
                out.push(c);
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == quote {
                    break;
                }
            }
            continue;
        }
        if is_cpp_ident_start(ch) {
            let mut ident = String::new();
            ident.push(ch);
            while let Some((_, c)) = it.peek().copied() {
                if !is_cpp_ident_continue(c) {
                    break;
                }
                ident.push(c);
                it.next();
            }
            if ident == src && should_rewrite_cpp_ident_at(input, start) {
                out.push_str(dst);
            } else {
                out.push_str(&ident);
            }
            continue;
        }
        out.push(ch);
    }
    out
}

fn should_rewrite_cpp_ident_at(input: &str, start: usize) -> bool {
    let mut prev = input[..start].chars().rev().filter(|c| !c.is_whitespace());
    match prev.next() {
        Some('.') => false,
        Some('>') if prev.next() == Some('-') => false,
        Some(':') if prev.next() == Some(':') => false,
        _ => true,
    }
}

fn is_cpp_ident_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic()
}

fn is_cpp_ident_continue(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
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
    let cx = ECx {
            prog: Some(prog),
        func,
        names: &names,
        lanes,
        self_subst: None,
        dut_type,
        trace_component: "",
        state_receiver: None,
    };
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
                writeln!(
                    out,
                    "{pad3}if ({cond}) {{ __bb = {}; }} else {{ __bb = {}; }}",
                    t.0, f.0
                )
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
                        writeln!(
                            out,
                            "{pad3}{INDENT}long long _next = clocks_[0].next_edge_ps;"
                        )
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
                let snippet =
                    randomize_snippet_for(prog, &names, *target, *constraints, randomize_snippets)
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
            prog: Some(prog),
        func,
        names: &names,
        lanes: &empty_lanes,
        self_subst: None,
        dut_type: "",
        trace_component: "",
        state_receiver: None,
    };
    let nparams = func.params.len();
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let pad3 = INDENT.repeat(depth + 3);

    // The element C++ type (for the `std::vector<T>` return type) — a
    // record name for a `RecordSeq` accumulator, or the scalar C++ type
    // for a `Seq` accumulator.
    let acc_ty = func
        .ret
        .map(|r| func.local(r).ty.clone())
        .unwrap_or(IrType::Unknown);
    let elem = match &acc_ty {
        IrType::RecordSeq(rid) => records
            .get(rid.index())
            .ok_or_else(|| {
                EmitError(format!(
                    "tbir: tseq `{}` references missing element record r{}",
                    func.name, rid.0
                ))
            })?
            .name
            .clone(),
        IrType::Seq(scalar) => super::local_scalar_cty(scalar).to_string(),
        _ => {
            return Err(EmitError(format!(
                "tbir: tseq `{}` has no RecordSeq/Seq return accumulator (lowering bug)",
                func.name
            )));
        }
    };

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
                writeln!(
                    out,
                    "{pad3}if ({cond}) {{ __bb = {}; }} else {{ __bb = {}; }}",
                    t.0, f.0
                )
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
                target,
                constraints,
                succ,
            } => {
                let snippet =
                    randomize_snippet_for(prog, &names, *target, *constraints, randomize_snippets)
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

/// C++ scalar type for a pure-helper local (param, internal local, or
/// return slot). Helper values live in the 64-bit loop-switch domain, so
/// this is a 64-bit-rank mapping: a `sint` local declares `int64_t` (v1's
/// `c_type_for` → `cpp_sint_for_width`), everything else `uint64_t`. This
/// makes the C++ variable's signedness match `expr_is_signed(Local)` —
/// without it, `sint` values routed through a helper computed `/`, `%`,
/// and ordered comparisons UNSIGNED, diverging from `--codegen v1`
/// (harc#532). `>>` was already saved by the forced `(int64_t)` cast the
/// shift emitter inserts, so only the cast-less operators diverged.
fn helper_local_cty(ty: &crate::ir::IrType) -> &'static str {
    match ty {
        crate::ir::IrType::SInt(_) => "int64_t",
        _ => "uint64_t",
    }
}

/// Return type for a helper: the declared type of its return slot, or
/// `void` when it has none. Kept identical between the prototype and the
/// definition so the two C++ signatures match.
fn helper_ret_cty(func: &TbFunction) -> &'static str {
    match func.ret {
        Some(r) => func
            .locals
            .get(r.index())
            .map(|l| helper_local_cty(&l.ty))
            .unwrap_or("uint64_t"),
        None => "void",
    }
}

/// Comma-joined parameter list for a helper's C++ signature. The first
/// `params.len()` locals ARE the parameters (TB-IR convention), so each
/// param's declared type comes from `func.locals[i].ty`.
fn helper_param_list(func: &TbFunction, names: &[String]) -> String {
    names[..func.params.len()]
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let cty = func
                .locals
                .get(i)
                .map(|l| helper_local_cty(&l.ty))
                .unwrap_or("uint64_t");
            format!("{cty} {n}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Forward declaration for a lowered pure helper, so source-order
/// emission supports helper-to-helper calls in any order.
pub(super) fn emit_helper_prototype(out: &mut String, func: &TbFunction) {
    let names = cpp_local_names(func);
    let ret_ty = helper_ret_cty(func);
    let params = helper_param_list(func, &names);
    writeln!(
        out,
        "static {ret_ty} {}({params});",
        helper_cpp_name(&func.name)
    )
    .ok();
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
    // Pure helpers are scalar-only: no DUT access, so no lane table, no
    // probe access (`dut_type` unused → `""`), and no record fields
    // (`records` empty).
    let empty_lanes = HashMap::new();
    let cx = ECx {
        prog: None,
        func,
        names: &names,
        lanes: &empty_lanes,
        self_subst: None,
        dut_type: "",
        trace_component: "",
        state_receiver: None,
    };
    let nparams = func.params.len();

    let ret_ty = helper_ret_cty(func);
    let params = helper_param_list(func, &names);
    writeln!(
        out,
        "static {ret_ty} {}({params}) {{",
        helper_cpp_name(&func.name)
    )
    .ok();
    for (i, n) in names.iter().enumerate().skip(nparams) {
        let cty = func
            .locals
            .get(i)
            .map(|l| helper_local_cty(&l.ty))
            .unwrap_or("uint64_t");
        writeln!(out, "{INDENT}{cty} {n} = 0; (void){n};").ok();
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
        Some(_) => writeln!(out, "{pad2}default: return 0;").ok(),
        None => writeln!(out, "{pad2}default: return;").ok(),
    };
    writeln!(out, "{INDENT}{INDENT}}}").ok();
    writeln!(out, "{INDENT}}}").ok();
    writeln!(out, "}}").ok();
    Ok(())
}

/// C++ name of the file-scope hit counter behind one `cover` check.
/// File scope (not a closure-local `static`) so the end-of-test summary,
/// which lives in the test's `run_*` function rather than inside the run
/// coroutine, can read it. v1 declares the same counter as a `static`
/// local INSIDE the coroutine lambda and then reads it from the enclosing
/// function — a scope error that makes v1's `cover` emission fail to
/// compile (documented divergence; see `docs/tbir-mvp.md`).
pub(super) fn cover_counter_name(tag: &str) -> String {
    format!("_cov_{tag}_hits")
}

/// Declare each temporal latch's `static` previous-value cell and this
/// cycle's value local, at the head of a concurrent-check closure.
/// Returns nothing; the matching write-back is `emit_temporal_writeback`.
fn emit_temporal_latches(
    out: &mut String,
    cx: &ECx<'_>,
    temporals: &[crate::ir::TemporalSlot],
    depth: usize,
) -> Result<(), EmitError> {
    let pad = INDENT.repeat(depth);
    for (i, _) in temporals.iter().enumerate() {
        writeln!(out, "{pad}static int64_t _harc_ps{i} = 0;").ok();
    }
    for (i, slot) in temporals.iter().enumerate() {
        let inner = expr_cpp(cx, &slot.inner)?;
        writeln!(out, "{pad}int64_t _harc_cur{i} = (int64_t)({inner});").ok();
    }
    Ok(())
}

/// Copy each latch's current value into its `static` cell, so the next
/// cycle's `past`/`rose`/`fell`/`stable` reads see it.
fn emit_temporal_writeback(out: &mut String, n: usize, depth: usize) {
    let pad = INDENT.repeat(depth);
    for i in 0..n {
        writeln!(out, "{pad}_harc_ps{i} = _harc_cur{i};").ok();
    }
}

/// One concurrent property check: a `_checkers` closure evaluated after
/// every primary-clock edge from this registration point onward. Mirrors
/// v1's `emit_property_check` — same three temporal shapes, same failure
/// lines, same error-counter policy (`assert` bumps, `assume` does not).
fn emit_property_check(
    out: &mut String,
    cx: &ECx<'_>,
    schema: &crate::ir::PropertyCheckSchema,
    depth: usize,
) -> Result<(), EmitError> {
    use crate::ir::PropertyShape;
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let sev = schema.severity.tag();
    let label = escape_c(&schema.label);

    writeln!(out, "{pad}_checkers.push_back([&]() {{").ok();
    emit_temporal_latches(out, cx, &schema.temporals, depth + 1)?;

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
            let a = expr_cpp(cx, ante)?;
            let b = expr_cpp(cx, cons)?;
            writeln!(out, "{pad1}static bool _harc_prev = false;").ok();
            writeln!(out, "{pad1}bool _harc_a = (bool)({a});").ok();
            writeln!(out, "{pad1}bool _harc_b = (bool)({b});").ok();
            writeln!(out, "{pad1}if (_harc_prev && !_harc_b) {{").ok();
            fail_arm(out, " (|=>)")?;
            writeln!(out, "{pad1}}}").ok();
            writeln!(out, "{pad1}_harc_prev = _harc_a;").ok();
        }
        PropertyShape::Implies { ante, cons } => {
            let a = expr_cpp(cx, ante)?;
            let b = expr_cpp(cx, cons)?;
            writeln!(out, "{pad1}if ((bool)({a}) && !(bool)({b})) {{").ok();
            fail_arm(out, " (|->)")?;
            writeln!(out, "{pad1}}}").ok();
        }
        PropertyShape::Invariant(e) => {
            let c = expr_cpp(cx, e)?;
            writeln!(out, "{pad1}if (!({c})) {{").ok();
            fail_arm(out, "")?;
            writeln!(out, "{pad1}}}").ok();
        }
    }

    emit_temporal_writeback(out, schema.temporals.len(), depth + 1);
    writeln!(out, "{pad}}});").ok();
    Ok(())
}

/// One concurrent `cover` witness: a `_checkers` closure that bumps the
/// check's file-scope hit counter on every primary-clock edge where the
/// predicate holds. Mirrors v1's `StmtKind::Cover` emission, plus the
/// temporal-latch machinery v1's cover path lacks.
fn emit_cover_check(
    out: &mut String,
    cx: &ECx<'_>,
    schema: &crate::ir::CoverCheckSchema,
    depth: usize,
) -> Result<(), EmitError> {
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let counter = cover_counter_name(&schema.tag);
    writeln!(out, "{pad}_checkers.push_back([&]() {{").ok();
    emit_temporal_latches(out, cx, &schema.temporals, depth + 1)?;
    let cond = expr_cpp(cx, &schema.cond)?;
    writeln!(out, "{pad1}if ((bool)({cond})) {counter}++;").ok();
    emit_temporal_writeback(out, schema.temporals.len(), depth + 1);
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
    prog: &TbProgram,
    id: crate::ir::CycleHandlerId,
    schema: &crate::ir::CycleHandlerSchema,
    depth: usize,
) -> Result<(), EmitError> {
    use crate::ir::CycleHandlerKind;
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let lambda = &prog
        .functions
        .get(schema.function.index())
        .ok_or_else(|| EmitError(format!("tbir: cycle handler h{} has no body", id.0)))?
        .name;
    // Per-handler tag for the `static` latch cells. Unique per closure
    // scope already, but tagged by handler id so a debugger shows which
    // `on` a cell belongs to.
    let tag = format!("_onh_{}", id.0);
    let vec = schema.phase.service_vec();
    writeln!(out, "{pad}{vec}.push_back([&]() {{").ok();
    match &schema.kind {
        CycleHandlerKind::Periodic { period } => {
            // `last = 0` means the FIRST firing is at cycle `period`, not
            // cycle 0 — "every N cycles", not "now and every N cycles".
            writeln!(out, "{pad1}static int64_t {tag}_last = 0;").ok();
            writeln!(
                out,
                "{pad1}if ((int64_t)cycle_count - {tag}_last >= {period}) {{"
            )
            .ok();
            writeln!(out, "{pad2}{tag}_last = (int64_t)cycle_count;").ok();
            writeln!(out, "{pad2}{lambda}();").ok();
            writeln!(out, "{pad1}}}").ok();
        }
        CycleHandlerKind::Trigger { trigger, edge } => {
            let t = expr_cpp(cx, trigger)?;
            match edge {
                crate::ir::CycleEdge::Level => {
                    writeln!(out, "{pad1}if ((bool)({t})) {{").ok();
                    writeln!(out, "{pad2}{lambda}();").ok();
                    writeln!(out, "{pad1}}}").ok();
                }
                crate::ir::CycleEdge::Rising | crate::ir::CycleEdge::Falling => {
                    writeln!(out, "{pad1}static bool {tag}_prev = false;").ok();
                    writeln!(out, "{pad1}bool {tag}_curr = (bool)({t});").ok();
                    let cond = match edge {
                        crate::ir::CycleEdge::Rising => format!("!{tag}_prev && {tag}_curr"),
                        _ => format!("{tag}_prev && !{tag}_curr"),
                    };
                    writeln!(out, "{pad1}if ({cond}) {{").ok();
                    writeln!(out, "{pad2}{lambda}();").ok();
                    writeln!(out, "{pad1}}}").ok();
                    writeln!(out, "{pad1}{tag}_prev = {tag}_curr;").ok();
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
        Some(IrType::Event(crate::ir::EventPayload::Scalar { signed })) => Ok(if *signed {
            "int64_t".to_string()
        } else {
            "uint64_t".to_string()
        }),
        Some(IrType::Event(crate::ir::EventPayload::Record(r))) => {
            Ok(prog.records[r.index()].name.clone())
        }
        _ => Err(EmitError(format!(
            "tbir: {} uses local {} as an event channel but it is not event-typed",
            cx.func.name, event.0
        ))),
    }
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
                    out, cx, records, bindings, *l, bus_field, method, args, depth,
                );
            }
            let name = &names[l.index()];
            let e = expr_cpp(cx, e)?;
            writeln!(out, "{pad}{name} = {e};").ok();
        }
        Stmt::RecordInit(l, r) => {
            let name = &names[l.index()];
            // Shared test-scope records are default-constructed once by
            // the enclosing test. Re-initializing them inside run/check/
            // hook bodies would wipe persistent host state.
            if shared_record_names(prog, func).contains(name) {
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
        Stmt::TransactorCall { dest, call } => {
            let Expr::Call(CallTarget::TransactorMethod { bus_field, method }, args) = call else {
                return Err(EmitError(format!(
                    "tbir: TransactorCall in {} carries a non-call-edge payload \
                     (verifier invariant violated)",
                    func.name
                )));
            };
            // Resolve the instance field to its transactor type via the
            // owner testbench — the lambda is named `<Type>_<method>`,
            // mirroring v1's hookable lambda naming.
            let xschema = func
                .owner
                .and_then(|o| prog.testbenches.get(o.index()))
                .and_then(|tb| {
                    tb.transactor_fields
                        .iter()
                        .find(|(f, _)| f == bus_field)
                        .map(|(_, xid)| prog.transactor(*xid))
                })
                .ok_or_else(|| {
                    EmitError(format!(
                        "tbir: TransactorCall `{bus_field}.{method}` in {} does not \
                         resolve through the owner testbench",
                        func.name
                    ))
                })?;
            let xname = xschema.name.as_str();
            let mut rendered = Vec::with_capacity(args.len() + 1);
            // State-receiver ABI (#494 P1b): an unbound stateful
            // transactor's method takes the calling instance's per-instance
            // state struct by reference as the leading arg, so `a.go()` and
            // `b.go()` mutate their own state through one shared body.
            if uses_state_receiver(xschema) {
                rendered.push(bus_field.clone());
            }
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
        Stmt::TransactorSelfCall { dest, call } => {
            let Expr::Call(CallTarget::TransactorSelfMethod { transactor, method }, args) = call
            else {
                return Err(EmitError(format!(
                    "tbir: TransactorSelfCall in {} carries a non-self-call payload \
                     (verifier invariant violated)",
                    func.name
                )));
            };
            let mut rendered = Vec::with_capacity(args.len() + 1);
            // Same-type self-call: forward the current state receiver so the
            // callee mutates the SAME per-instance struct (#494 P1b). A
            // self-call only appears inside a method body, and a stateful
            // type's methods all carry `self_state`, so the presence of a
            // receiver in scope tracks the callee's ABI exactly.
            if let Some(recv) = cx.state_receiver {
                rendered.push(recv.to_string());
            }
            for a in args {
                rendered.push(expr_cpp(cx, a)?);
            }
            let invoke = format!("{transactor}_{method}({})", rendered.join(", "));
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
            let name = &names[local.index()];
            let e = expr_cpp(cx, value)?;
            let dst =
                super::expr::record_access_cpp(cx, name, field, path, mid_indices, index.as_ref())?;
            writeln!(out, "{pad}{dst} = {e};").ok();
        }
        Stmt::RecordWriteCb {
            local,
            binding,
            field,
            offset,
            value,
            callback,
        } => {
            // Passive RAL `record_write` with a per-register write callback
            // registered on the binding: mirror update wrapped in the
            // recursion-depth guard, then the callback fires with the
            // observed value. Mirrors v1's `try_emit_record_write`
            // (`<binding>_cb_depth` / `HARC_RAL_CB_MAX_DEPTH`); the FATAL
            // message uses the const-decoded `at addr 0x..` to match v1.
            let name = &names[local.index()];
            let v = expr_cpp(cx, value)?;
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
            writeln!(out, "{p2}{name}.{field} = _rec_data;").ok();
            if let Some(fid) = callback {
                let cb_name = &prog.function(*fid).name;
                writeln!(out, "{p2}{cb_name}(_rec_data);").ok();
            }
            writeln!(out, "{p2}{binding}_cb_depth--;").ok();
            writeln!(out, "{p1}}}").ok();
            writeln!(out, "{pad}}}").ok();
        }
        Stmt::TbFieldWrite { field, value } => {
            let e = expr_cpp(cx, value)?;
            writeln!(out, "{pad}_tb.{field} = {e};").ok();
        }
        Stmt::TbQueuePush { field, value } => {
            let e = expr_cpp(cx, value)?;
            writeln!(out, "{pad}_tb.{field}.push({e});").ok();
        }
        Stmt::TbQueuePop { field, dest } => {
            let name = &names[dest.index()];
            writeln!(out, "{pad}{name} = _tb.{field}.pop();").ok();
        }
        Stmt::TransactorStateWrite {
            instance,
            field,
            value,
        } => {
            let recv = super::expr::resolve_state_instance(cx, instance)?;
            let e = expr_cpp(cx, value)?;
            writeln!(out, "{pad}{recv}.{field} = {e};").ok();
        }
        // `last.addr = addr` on a bound-to target transactor whole-record
        // state field — a nested member of the value-record struct on the
        // per-instance struct (`<instance>.<field>.<path…> = <value>`).
        Stmt::TransactorStateRecordFieldWrite {
            instance,
            field,
            path,
            value,
        } => {
            // Through the same receiver resolver the READ side and the
            // scalar write already use. Written raw, an empty instance —
            // which is what a transactor's own method body carries for a
            // self-reference — emitted a leading-dot `.cur.tag = 5;`.
            let recv = super::expr::resolve_state_instance(cx, instance)?;
            let e = expr_cpp(cx, value)?;
            writeln!(out, "{pad}{recv}.{field}.{} = {e};", path.join(".")).ok();
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
            let e = expr_cpp(cx, value)?;
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
            let base = probe_read_accessor(cx.dut_type, p);
            let val = expr_cpp(cx, e)?;
            writeln!(out, "{pad}dut->rootp->{base}_drv = {val};").ok();
            writeln!(out, "{pad}dut->rootp->{base}_en = 1;").ok();
        }
        Stmt::DutWrite(p, e) => {
            let sig = port_signal(cx, p);
            match &p.lane {
                // Packed multi-lane port: bit-deposit through the
                // runtime helper; unpacked-array port: raw subscript
                // (v1's emit_signal_assignment split). The lane index is
                // a constant or a runtime expression — v1 re-renders an
                // arbitrary `&Expr` here.
                Some(lane) => {
                    let idx = lane_index_cpp(cx, lane)?;
                    let e = expr_cpp(cx, e)?;
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
                        let e = expr_cpp(cx, e)?;
                        writeln!(out, "{pad}harc_rt::harc_assign({sig}, {e});").ok();
                    }
                }
            }
        }
        Stmt::DutRead(l, p) => {
            let name = &names[l.index()];
            writeln!(out, "{pad}{name} = {};", port_read(cx, p)?).ok();
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
                    writeln!(out, "{pad}ctx.errors++;").ok();
                }
                "FATAL" => {
                    writeln!(out, "{pad}ctx.errors++; _fatal = true;").ok();
                }
                _ => {}
            }
        }
        Stmt::AssertCheck { cond, on_fail } => {
            let cond = expr_cpp(cx, cond)?;
            writeln!(out, "{pad}if (!({cond})) {{").ok();
            emit_log_call(out, cx, "FAIL", None, on_fail, depth + 1)?;
            writeln!(out, "{pad}{INDENT}ctx.errors++;").ok();
            writeln!(out, "{pad}}}").ok();
        }
        // Same guard shape as `AssertCheck`, minus the error bump — an
        // assumption bounds the inputs, it does not fail the test.
        Stmt::AssumeCheck { cond, on_fail } => {
            let cond = expr_cpp(cx, cond)?;
            writeln!(out, "{pad}if (!({cond})) {{").ok();
            emit_log_call(out, cx, "ASSUME", None, on_fail, depth + 1)?;
            writeln!(out, "{pad}}}").ok();
        }
        Stmt::CovReport(inst) => {
            writeln!(out, "{pad}_tb.{}.report();", inst.tb_field).ok();
        }
        Stmt::PropertyCheck(p) => {
            let schema = prog.property_checks.get(p.index()).ok_or_else(|| {
                EmitError(format!(
                    "tbir: {} references missing property check p{}",
                    func.name, p.0
                ))
            })?;
            emit_property_check(out, cx, schema, depth)?;
        }
        Stmt::CoverCheck(c) => {
            let schema = prog.cover_checks.get(c.index()).ok_or_else(|| {
                EmitError(format!(
                    "tbir: {} references missing cover check c{}",
                    func.name, c.0
                ))
            })?;
            emit_cover_check(out, cx, schema, depth)?;
        }
        // v1's shape exactly: a subscriber is a closure pushed onto the
        // channel vector, and `emit` calls each subscriber synchronously
        // in subscription order. The subscriber BODY is a separate
        // test-scope lambda (like a cycle handler's) so the pushed
        // closure outlives the block that registered it.
        Stmt::EventSubscribe { event, handler } => {
            let chan = &names[event.index()];
            let body = &prog
                .functions
                .get(handler.index())
                .ok_or_else(|| {
                    EmitError(format!(
                        "tbir: {} subscribes to fn{} which does not resolve",
                        func.name, handler.0
                    ))
                })?
                .name;
            let ty = event_payload_cty(prog, cx, *event)?;
            writeln!(
                out,
                "{pad}{chan}.push_back([&]({ty} _p) {{ {body}(_p); }});"
            )
            .ok();
        }
        Stmt::EventEmit { event, args } => {
            let chan = &names[event.index()];
            let mut rendered = Vec::with_capacity(args.len());
            for a in args {
                rendered.push(expr_cpp(cx, a)?);
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
            emit_cycle_handler(out, cx, prog, *h, schema, depth)?;
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
        Stmt::ScoreboardOp {
            field,
            op,
            nested_path,
            ..
        } => {
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
            // cx-aware so a `self.<field>` write re-roots at the running
            // instance under `cx.self_subst` (the `--mt` active-driver worker
            // coroutine has no `self` parameter). Byte-identical to the bare
            // `comp_base_cpp` when `self_subst` is `None` (every prior site).
            writeln!(
                out,
                "{pad}{}.{field} = {e};",
                comp_base_cpp_subst_cx(cx, base)
            )
            .ok();
        }
        Stmt::ComponentVecElementWrite { base, field, index, value } => {
            let index = expr_cpp(cx, index)?;
            let value = expr_cpp(cx, value)?;
            writeln!(out, "{pad}{}.{field}[{index}] = {value};", comp_base_cpp_subst_cx(cx, base)).ok();
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
        Stmt::ComponentCall {
            base,
            component,
            method,
            args,
            dest,
        } => {
            let comp = prog.components.get(component.index()).ok_or_else(|| {
                EmitError(format!(
                    "tbir: ComponentCall in {} references missing component c{}",
                    func.name, component.0
                ))
            })?;
            let mut rendered = vec![comp_base_cpp_subst_cx(cx, base)];
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
        // `<recv>.<queue>.push(value)` on a composite-component `queue<T>`
        // field — `self.errors.push(err)` inside a method body, or
        // `checker.sb.errors.push(e)` from the test. Mirrors v1's
        // `HarcQueue::push`.
        Stmt::ComponentQueuePush { base, queue, value } => {
            let e = expr_cpp(cx, value)?;
            writeln!(out, "{pad}{}.{queue}.push({e});", comp_base_cpp(base)).ok();
        }
        // `let v = <recv>.<queue>.pop()` — pop the queue front into a local.
        Stmt::ComponentQueuePop { base, queue, dest } => {
            let name = &names[dest.index()];
            writeln!(out, "{pad}{name} = {}.{queue}.pop();", comp_base_cpp(base)).ok();
        }
        // `<dst>.<field> = <src>` — whole sub-component value copy
        // (`checker.sb = sb`). A plain C++ struct copy of two run-scope
        // component locals (v1's `_tb.checker.sb = _tb.sb;`).
        Stmt::ComponentSubAssign { dst, field, src } => {
            writeln!(
                out,
                "{pad}{}.{field} = {};",
                comp_base_cpp(dst),
                comp_base_cpp(src)
            )
            .ok();
        }
        // `yield t` — append a record value to the sequence accumulator
        // (v1's `_result.push_back(t)`).
        Stmt::SeqPush { seq, value } => {
            let name = &names[seq.index()];
            let v = expr_cpp(cx, value)?;
            writeln!(out, "{pad}{name}.push_back({v});").ok();
        }
        Stmt::TlmFork(desc) => emit_tlm_fork(out, cx, bindings, desc, depth)?,
        Stmt::TlmJoinAll(pending) => emit_tlm_join_all(out, cx, records, bindings, pending, depth)?,
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
        writeln!(out, "{pad}co_await harc_rt::wait_cycles(_slot, 1);").ok();
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
                "co_await harc_rt::wait_cycles(_slot, 1)",
                &format!("TLM {}.{} fork request", desc.bus_field, desc.method),
            )
        )
        .ok();
        writeln!(out, "{pad}co_await harc_rt::wait_cycles(_slot, 1);").ok();
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
    for p in pending {
        let binding = resolve_binding(bindings, &p.bus_field, &p.method, cx.func)?;
        let wire = |sig: &str| format!("dut->{}", binding.wire_name(&p.method, sig));
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
                "co_await harc_rt::wait_cycles(_slot, 1)",
                &format!("TLM {}.{} fork response", p.bus_field, p.method),
                "_rsp_ok",
            )
        )
        .ok();
        writeln!(out, "{pad1}if (_rsp_ok) {{").ok();
        if let (Some(dest), true) = (p.dest, p.has_ret) {
            let capture = tlm_capture_expr(cx, records, dest, &wire("rsp_data"));
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
    writeln!(out, "{pad}{{").ok();
    writeln!(out, "{pad1}int _tlm_pending = {};", pending.len()).ok();
    for idx in 0..pending.len() {
        writeln!(out, "{pad1}bool _tlm_seen_{idx} = false;").ok();
    }
    writeln!(out, "{pad1}int _tlm_budget = 256;").ok();
    writeln!(out, "{pad1}while (_tlm_pending > 0 && _tlm_budget > 0) {{").ok();
    for p in pending {
        let binding = resolve_binding(bindings, &p.bus_field, &p.method, cx.func)?;
        writeln!(
            out,
            "{pad2}dut->{} = 0;",
            binding.wire_name(&p.method, "rsp_ready")
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
            let capture = tlm_capture_expr(cx, records, dest, &wire("rsp_data"));
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
    writeln!(out, "{pad2}co_await harc_rt::wait_cycles(_slot, 1);").ok();
    writeln!(out, "{pad2}if (!_tlm_accept) _tlm_budget--;").ok();
    writeln!(out, "{pad1}}}").ok();
    for p in pending {
        let binding = resolve_binding(bindings, &p.bus_field, &p.method, cx.func)?;
        writeln!(
            out,
            "{pad1}dut->{} = 0;",
            binding.wire_name(&p.method, "rsp_ready")
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
    records: &[RecordSchema],
    dest: crate::ir::LocalId,
    raw_wire: &str,
) -> String {
    if let Some(IrType::Record(rid)) = cx.func.locals.get(dest.index()).map(|l| &l.ty) {
        if let Some(rec) = records.get(rid.index()) {
            return format!("harc_unpack_{}({raw_wire})", rec.name);
        }
    }
    format!("harc_rt::harc_read({raw_wire})")
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
    bindings
        .iter()
        .find(|b| b.field == bus_field)
        .ok_or_else(|| {
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
    records: &[RecordSchema],
    bindings: &[BusBindingSchema],
    dest: crate::ir::LocalId,
    bus_field: &str,
    method: &str,
    args: &[Expr],
    depth: usize,
) -> Result<(), EmitError> {
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let func = cx.func;
    let names = cx.names;
    let binding = bindings
        .iter()
        .find(|b| b.field == bus_field)
        .ok_or_else(|| {
            EmitError(format!(
                "tbir: unresolved transactor call `{bus_field}.{method}` in {} — \
             verifier should have rejected it",
                func.name
            ))
        })?;
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
    let wire = |sig: &str| format!("dut->{}", binding.wire_name(method, sig));
    let budget_wait = |out: &mut String, sig: &str, label: &str| {
        writeln!(
            out,
            "{pad}{}",
            crate::codegen::bounded_handshake_wait(
                &wire(sig),
                crate::codegen::TLM_WAIT_BOUND,
                "co_await harc_rt::wait_cycles(_slot, 1)",
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
    writeln!(out, "{pad}co_await harc_rt::wait_cycles(_slot, 1);").ok();
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
            "co_await harc_rt::wait_cycles(_slot, 1)",
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
        let capture = tlm_capture_expr(cx, records, dest, &wire("rsp_data"));
        writeln!(out, "{pad2}{} = {capture};", names[dest.index()]).ok();
    }
    writeln!(out, "{pad1}}}").ok();
    writeln!(out, "{pad}}}").ok();
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
///
/// Names of record locals that are SHARED test-scope host state. This
/// includes transaction/struct-typed testbench fields plus regblock
/// mirrors whose binding declares `on regs.REG` write callbacks. These
/// are declared + default-constructed ONCE at test scope and captured by
/// `[&]`, so every function that touches them must reference that one
/// cell by name — never re-declare or re-init it.
pub(super) fn shared_record_names(prog: &TbProgram, func: &TbFunction) -> HashSet<String> {
    let mut out = HashSet::new();
    if let Some(o) = func.owner {
        if let Some(tb) = prog.testbenches.get(o.index()) {
            for (field, _) in &tb.record_fields {
                out.insert(field.clone());
            }
            for b in &tb.regblock_bindings {
                if !b.callbacks.is_empty() {
                    out.insert(b.field.clone());
                }
            }
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
) -> Result<(), EmitError> {
    let pad = INDENT.repeat(depth);
    let shared = shared_record_names(prog, func);
    for (l, n) in func.locals.iter().zip(names).skip(skip) {
        // Shared test-scope record state is declared once by the enclosing
        // test; skip its per-function declaration so references resolve to
        // the captured object.
        if shared.contains(n) {
            continue;
        }
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
            // A scalar-element transaction-sequence local —
            // `std::vector<T>` over the scalar C++ type. v1's tseq scalar
            // accumulator / call-result shape.
            IrType::Seq(ref scalar) => {
                let cty = super::local_scalar_cty(scalar);
                writeln!(out, "{pad}std::vector<{cty}> {n}{{}}; (void){n};").ok();
            }
            // A test-scope event channel — v1's subscriber vector.
            IrType::Event(payload) => {
                let cty = match payload {
                    crate::ir::EventPayload::Scalar { signed: true } => "int64_t".to_string(),
                    crate::ir::EventPayload::Scalar { signed: false } => "uint64_t".to_string(),
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
            ref ty => {
                let cty = super::local_scalar_cty(ty);
                writeln!(out, "{pad}{cty} {n} = 0; (void){n};").ok();
            }
        }
    }
    Ok(())
}

/// Whether a transactor type's methods use the per-instance state-receiver
/// ABI (#494 P1b): the shared body takes a leading `<Type>_state&
/// self_state` param and each call site passes its instance's struct.
///
/// True only for an UNBOUND stateful transactor — that is the form whose
/// method bodies lowering leaves with empty-instance state placeholders so
/// they resolve against the receiver. A bound-to initiator BFM instead
/// bakes its (single) instance name into the shared body, so it keeps the
/// plain arg-only signature even when it carries state; a stateless type
/// has no per-instance struct at all.
pub(super) fn uses_state_receiver(schema: &TransactorSchema) -> bool {
    !schema.state_fields.is_empty() && schema.bound_bus.is_none()
}

pub(super) fn declare_method_slot(
    out: &mut String,
    prog: &TbProgram,
    schema: &TransactorSchema,
    m: &TransactorMethodSchema,
    depth: usize,
) -> Result<(), EmitError> {
    let func = prog.function(m.function);
    let ret_ty = if func.ret.is_some() {
        "uint64_t"
    } else {
        "void"
    };
    let mut param_tys: Vec<String> = Vec::new();
    // State-receiver ABI (#494 P1b): an unbound stateful transactor's
    // method takes its per-instance state struct by reference as the
    // leading param, so one shared body serves any number of instances.
    if uses_state_receiver(schema) {
        param_tys.push(format!("{}&", super::runtime::unbound_state_struct_ty(schema)));
    }
    param_tys.extend((0..func.params.len()).map(|i| match func.locals[i].ty {
        IrType::Record(r) => prog.records[r.index()].name.clone(),
        IrType::RecordSeq(r) => format!("std::vector<{}>", prog.records[r.index()].name),
        IrType::Seq(ref scalar) => format!("std::vector<{}>", super::local_scalar_cty(scalar)),
        ref ty => super::local_scalar_cty(ty).to_string(),
    }));
    let params = param_tys.join(", ");
    let pad = INDENT.repeat(depth);
    writeln!(
        out,
        "{pad}std::function<{ret_ty}({params})> {}_{};",
        schema.name, m.name
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
        self_subst: None,
        dut_type: &schema.dut_type,
        trace_component: "",
        state_receiver: has_state.then_some("self_state"),
    };
    let nparams = func.params.len();
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let pad3 = INDENT.repeat(depth + 3);

    let ret_ty = if func.ret.is_some() {
        "uint64_t"
    } else {
        "void"
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
            super::runtime::unbound_state_struct_ty(schema)
        ));
    }
    param_list.extend(names[..nparams].iter().enumerate().map(|(i, n)| {
        match func.locals[i].ty {
            IrType::Record(r) => format!("{} {n}", prog.records[r.index()].name),
            IrType::RecordSeq(r) => format!("std::vector<{}> {n}", prog.records[r.index()].name),
            IrType::Seq(ref scalar) => {
                format!("std::vector<{}> {n}", super::local_scalar_cty(scalar))
            }
            ref ty => format!("{} {n}", super::local_scalar_cty(ty)),
        }
    }));
    let params = param_list.join(", ");
    writeln!(
        out,
        "{pad}{}_{} = [&]({params}) -> {ret_ty} {{",
        schema.name, m.name
    )
    .ok();
    declare_locals(out, prog, func, &names, nparams, depth + 1)?;
    // Pre-hooks (`on <obj>.<method> pre`) fire BEFORE the body, with the
    // same args the method received (mirrors v1's `<Type>_<method>_pre`
    // fan-out loop). The hook lambdas are emitted just before this method.
    let hook_args = names[..nparams].join(", ");
    for fid in &m.pre_hooks {
        writeln!(out, "{pad1}{}({hook_args});", prog.function(*fid).name).ok();
    }
    // Hook-vector fan-out for hook-triggered covergroups
    // (`covergroup G @(drv.send(t) pre)`): fire the `<Type>_<method>_pre`
    // subscribers before the body, with the method's args. The vector is
    // only declared when `cov_hook_subs` is non-empty, so guard the
    // fan-out the same way (v1's `emit_hook_vectors` + fan-out). `pre`
    // and `post` subscribers are partitioned by side.
    let has_pre_cov = m
        .cov_hook_subs
        .iter()
        .any(|(_, s)| matches!(s, crate::ast::HookSide::Pre));
    let has_post_cov = m
        .cov_hook_subs
        .iter()
        .any(|(_, s)| matches!(s, crate::ast::HookSide::Post));
    if has_pre_cov {
        writeln!(
            out,
            "{pad1}for (auto& _h : {}_{}_pre) _h({hook_args});",
            schema.name, m.name
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
                let cond = expr_cpp(&cx, c)?;
                writeln!(
                    out,
                    "{pad3}if ({cond}) {{ __bb = {}; }} else {{ __bb = {}; }}",
                    t.0, f.0
                )
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
            Terminator::WaitTimePs(ps, b) => {
                // Wall-clock settle inside a synchronous method body:
                // v1 advances absolute simulation time inline, and these
                // lambdas capture `now_ps` / `eval_clocks_until` by reference.
                writeln!(out, "{pad3}eval_clocks_until(now_ps + {ps});").ok();
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::Return => {
                // Post-hooks (`on <obj>.<method> post`) fire AFTER the
                // body, before returning — with the same args (mirrors
                // v1's `<Type>_<method>_post` fan-out at the lambda end).
                // v1 places the fan-out at the body's natural end, so an
                // early `return` skips it; in this subset hooked methods
                // are void and fall through to a single terminal Return,
                // so firing at every void return matches v1 exactly.
                for fid in &m.post_hooks {
                    writeln!(out, "{pad3}{}({hook_args});", prog.function(*fid).name).ok();
                }
                // Hook-vector fan-out for hook-triggered covergroups
                // sampled on this method's `post` boundary.
                if has_post_cov {
                    writeln!(
                        out,
                        "{pad3}for (auto& _h : {}_{}_post) _h({hook_args});",
                        schema.name, m.name
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
                let snippet =
                    randomize_snippet_for(prog, &names, *target, *constraints, randomize_snippets)
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
                let n = expr_cpp(&cx, cycles)?;
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
    randomize_snippets: &[String],
    depth: usize,
) -> Result<(), EmitError> {
    let lambda = format!("{}_{}", comp.name, m.name);
    emit_component_fn_lambda(
        out,
        prog,
        comp,
        m.function,
        &lambda,
        Some(ComponentHookCtx {
            method_name: &m.name,
            cov_hook_subs: &m.cov_hook_subs,
        }),
        randomize_snippets,
        depth,
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
    randomize_snippets: &[String],
    depth: usize,
) -> Result<(), EmitError> {
    let lambda = on_handler_lambda_name(comp, oh);
    emit_component_fn_lambda(
        out,
        prog,
        comp,
        oh.function,
        &lambda,
        None,
        randomize_snippets,
        depth,
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
    randomize_snippets: &[String],
    depth: usize,
) -> Result<(), EmitError> {
    let lambda = periodic_handler_lambda_name(comp, ph);
    emit_component_fn_lambda(
        out,
        prog,
        comp,
        ph.function,
        &lambda,
        None,
        randomize_snippets,
        depth,
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
    randomize_snippets: &[String],
    depth: usize,
) -> Result<(), EmitError> {
    let lambda = cycle_handler_lambda_name(comp, ch);
    emit_component_fn_lambda(
        out,
        prog,
        comp,
        ch.function,
        &lambda,
        None,
        randomize_snippets,
        depth,
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
    randomize_snippets: &[String],
    depth: usize,
) -> Result<(), EmitError> {
    let lambda = watchdog_lambda_name(comp, w);
    emit_component_fn_lambda(
        out,
        prog,
        comp,
        w.function,
        &lambda,
        None,
        randomize_snippets,
        depth,
    )
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
            prog: Some(prog),
        func,
        names: &names,
        lanes: &empty_lanes,
        self_subst: Some(instance),
        dut_type: "",
        trace_component: "",
        state_receiver: None,
    };
    expr_cpp(&cx, e)
}

/// Render a testbench-scoped cycle-trigger predicate (issue #494 P2b) as
/// standalone C++ text. Unlike `clause_expr_cpp` (component-self scope),
/// this uses the TEST-scope ECx: `self_subst: None` and a real `dut_type`
/// so `dut.<sig>` reads route to the shared `dut` handle and `_tb.<field>`
/// reads resolve to the captured host struct — matching `emit_test_hook`.
pub(super) fn tb_service_expr_cpp(
    prog: &TbProgram,
    function: crate::ir::FunctionId,
    dut_type: &str,
    e: &Expr,
) -> Result<String, EmitError> {
    let func = prog.function(function);
    let names = cpp_local_names(func);
    let empty_lanes = HashMap::new();
    let cx = ECx {
            prog: Some(prog),
        func,
        names: &names,
        lanes: &empty_lanes,
        self_subst: None,
        dut_type,
        trace_component: "",
        state_receiver: None,
    };
    expr_cpp(&cx, e)
}

/// Shared lambda emission for component methods and on-handlers: a free
/// `<lambda>(<Comp>& self, args...)` loop-switch over the lowered CFG.
struct ComponentHookCtx<'a> {
    method_name: &'a str,
    cov_hook_subs: &'a [(crate::ir::CovgroupId, crate::ast::HookSide)],
}

fn emit_component_fn_lambda(
    out: &mut String,
    prog: &TbProgram,
    comp: &crate::ir::ComponentSchema,
    function: crate::ir::FunctionId,
    lambda: &str,
    hook_ctx: Option<ComponentHookCtx<'_>>,
    randomize_snippets: &[String],
    depth: usize,
) -> Result<(), EmitError> {
    let func = prog.function(function);
    let names = cpp_local_names(func);
    let empty_lanes = HashMap::new();
    // Component method/on-handler bodies are host-side; no DUT probes.
    let cx = ECx {
            prog: Some(prog),
        func,
        names: &names,
        lanes: &empty_lanes,
        self_subst: None,
        dut_type: "",
        trace_component: "",
        state_receiver: None,
    };
    let nparams = func.params.len();
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let pad3 = INDENT.repeat(depth + 3);

    // A record-returning method (`function predict_read(...) ->
    // ReadResponse`) returns the record struct by value; a scalar return
    // widens to uint64_t; no return is `void`.
    let ret_ty = match func.ret.map(|r| &func.locals[r.index()].ty) {
        Some(IrType::Record(r)) => prog.records[r.index()].name.clone(),
        Some(_) => "uint64_t".to_string(),
        None => "void".to_string(),
    };
    // The receiver `self`, then one parameter per declared param — a
    // record param (`on in_ev(t)` with `event<TinyTxn>`) is taken by
    // value as the record struct; a component-typed param (`observe(addr,
    // model: ProtocolModel)`) by value as the component struct; every
    // other param widens to uint64_t.
    let mut params = vec![format!("{}& self", comp.name)];
    for (i, n) in names[..nparams].iter().enumerate() {
        let pty = match func.locals[i].ty {
            IrType::Record(r) => prog.records[r.index()].name.clone(),
            // A transaction-sequence param (a sequencer's
            // `hookable dispatch(txns: TSeq<RegOp>)`) is taken by value as
            // `std::vector<Record>`, matching the tseq generator's return.
            IrType::RecordSeq(r) => format!("std::vector<{}>", prog.records[r.index()].name),
            // The scalar-element analogue (`TSeq<uint<N>>`) — `std::vector<T>`
            // over the scalar C++ type (#453).
            IrType::Seq(ref scalar) => format!("std::vector<{}>", super::local_scalar_cty(scalar)),
            IrType::Component(c) => prog.components[c.index()].name.clone(),
            ref ty => super::local_scalar_cty(ty),
        };
        params.push(format!("{pty} {n}"));
    }
    let params = params.join(", ");
    writeln!(out, "{pad}auto {lambda} = [&]({params}) -> {ret_ty} {{").ok();
    declare_locals(out, prog, func, &names, nparams, depth + 1)?;
    let hook_args = names[..nparams].join(", ");
    let has_pre_cov = hook_ctx.as_ref().is_some_and(|ctx| {
        ctx.cov_hook_subs
            .iter()
            .any(|(_, s)| matches!(s, crate::ast::HookSide::Pre))
    });
    let has_post_cov = hook_ctx.as_ref().is_some_and(|ctx| {
        ctx.cov_hook_subs
            .iter()
            .any(|(_, s)| matches!(s, crate::ast::HookSide::Post))
    });
    if has_pre_cov {
        let ctx = hook_ctx.as_ref().expect("pre hook context present");
        writeln!(
            out,
            "{pad1}for (auto& _h : self._harc_cov_{}_pre) _h({hook_args});",
            ctx.method_name
        )
        .ok();
    }
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
                writeln!(
                    out,
                    "{pad3}if ({cond}) {{ __bb = {}; }} else {{ __bb = {}; }}",
                    t.0, f.0
                )
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
            Terminator::WaitTimePs(ps, b) => {
                // Same wall-clock settle support as transactor methods.
                writeln!(out, "{pad3}eval_clocks_until(now_ps + {ps});").ok();
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::Return => match func.ret {
                Some(r) => {
                    if has_post_cov
                        && func
                            .implicit_returns
                            .contains(&BlockId(bi as u32))
                    {
                        let ctx = hook_ctx.as_ref().expect("post hook context present");
                        writeln!(
                            out,
                            "{pad3}for (auto& _h : self._harc_cov_{}_post) _h({hook_args});",
                            ctx.method_name
                        )
                        .ok();
                    }
                    writeln!(out, "{pad3}return {};", names[r.index()]).ok();
                }
                None => {
                    if has_post_cov
                        && func
                            .implicit_returns
                            .contains(&BlockId(bi as u32))
                    {
                        let ctx = hook_ctx.as_ref().expect("post hook context present");
                        writeln!(
                            out,
                            "{pad3}for (auto& _h : self._harc_cov_{}_post) _h({hook_args});",
                            ctx.method_name
                        )
                        .ok();
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
                let snippet =
                    randomize_snippet_for(prog, &names, *target, *constraints, randomize_snippets)
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
                let n = expr_cpp(&cx, cycles)?;
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
    // The unreachable switch default returns a value-initialized result —
    // `{}` covers both a record struct and a scalar (zero), matching the
    // `default: return 0;` for scalars while compiling for a record return.
    match func.ret {
        Some(_) => writeln!(out, "{pad2}default: return {{}};").ok(),
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
    mt: bool,
    actor_threads: &mut Vec<(String, String)>,
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
            prog: Some(prog),
            func,
            names: &names,
            lanes: &empty_lanes,
            self_subst: None,
            dut_type: "",
            trace_component: instance,
            state_receiver: None,
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
                tag_count as usize,
                bindings,
                mt,
                actor_threads,
                depth,
            )?;
            continue;
        }

        crate::codegen::tbir::runtime::register_actor_slot(
            out,
            mt,
            actor_threads,
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
            writeln!(
                out,
                "{pad2}uint64_t {local} = harc_rt::harc_read({});",
                wire(arg)
            )
            .ok();
        }
        writeln!(out, "{pad2}co_await harc_rt::wait_cycles(_slot, 1);").ok();
        writeln!(out, "{pad2}{} = 0;", wire("req_ready")).ok();
        writeln!(
            out,
            "{pad2}{instance}._last_in_cycle = (uint64_t)cycle_count;"
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
            let drive =
                if let Some(IrType::Record(rid)) = func.locals.get(ret.index()).map(|l| &l.ty) {
                    let rec = prog.records.get(rid.index()).ok_or_else(|| {
                        EmitError(format!(
                            "tbir: target responder `{}` returns missing record r{}",
                            func.name, rid.0
                        ))
                    })?;
                    format!(
                        "harc_drive_{}({}, {});",
                        rec.name,
                        wire("rsp_data"),
                        &names[ret.index()]
                    )
                } else {
                    format!(
                        "harc_rt::harc_assign({}, {});",
                        wire("rsp_data"),
                        &names[ret.index()]
                    )
                };
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
            "{pad2}{instance}._last_out_cycle = (uint64_t)cycle_count;"
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
    declare_locals(out, prog, func, names, nparams, depth)?;
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
                let cond = expr_cpp(cx, c)?;
                writeln!(
                    out,
                    "{pad_body}if ({cond}) {{ __bb = {}; }} else {{ __bb = {}; }}",
                    t.0, f.0
                )
                .ok();
            }
            Terminator::WaitCycles(n, None, b) => {
                let n = expr_cpp(cx, n)?;
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
    bindings: &[BusBindingSchema],
    actor_threads: &mut Vec<(String, String)>,
    depth: usize,
) -> Result<(), EmitError> {
    let comp = &prog.components[component.index()];
    let inst_tag = inst_path.replace('.', "_");
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    for oh in &comp.on_handlers {
        let func = prog.function(oh.function);
        let names = cpp_local_names(func);
        let empty_lanes = HashMap::new();
        // Driver bodies poke the bound bus's DUT wires (already prefix-filled
        // at lowering) and forward downstream blocking TLM calls; no probes.
        // `self_subst = inst_path` so a `self.<field>` access (`self.last_read
        // = val`) resolves to the running instance (`drv.last_read`) — the
        // worker coroutine has no `self` parameter, unlike the synchronous
        // on-handler lambda.
        let cx = ECx {
            prog: Some(prog),
            func,
            names: &names,
            lanes: &empty_lanes,
            self_subst: Some(inst_path),
            dut_type: "",
            trace_component: inst_path,
            state_receiver: None,
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
            "{pad2}{inst_path}._last_in_cycle = (uint64_t)cycle_count;"
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
///     `lane_req_valid`, `lane_rsp_valid`, one arg array per request arg,
///     and (value methods) a `lane_rsp_data` array. The TB-IR value model
///     is uniformly `uint64_t` (unlike v1's precise C-types), so every
///     array element is `uint64_t` / `bool`; the runtime `harc_read`/
///     `harc_assign` helpers still width-correct the bus wires.
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
    tag_count: usize,
    bindings: &[BusBindingSchema],
    mt: bool,
    actor_threads: &mut Vec<(String, String)>,
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
    for arg in &tm.args {
        let arr = format!("{prefix}_arg_{arg}");
        writeln!(out, "{pad}std::array<uint64_t, {tag_count}> {arr}{{}};").ok();
    }
    if tm.has_ret {
        writeln!(
            out,
            "{pad}std::array<uint64_t, {tag_count}> {lane_rsp_data}{{}};"
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
    crate::codegen::tbir::runtime::register_actor_slot(
        out,
        mt,
        actor_threads,
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
    for arg in &tm.args {
        let arr = format!("{prefix}_arg_{arg}");
        writeln!(
            out,
            "{pad3}{arr}[_tag] = harc_rt::harc_read({});",
            wire(arg)
        )
        .ok();
    }
    writeln!(out, "{pad3}{lane_busy}[_tag].store(true);").ok();
    writeln!(out, "{pad3}{lane_req_valid}[_tag].store(true);").ok();
    writeln!(
        out,
        "{pad3}{instance}._last_in_cycle = (uint64_t)cycle_count;"
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
        crate::codegen::tbir::runtime::register_actor_slot(
            out,
            mt,
            actor_threads,
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
            writeln!(out, "{pad2}uint64_t {local} = {arr}[{lane}];").ok();
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
    crate::codegen::tbir::runtime::register_actor_slot(
        out,
        mt,
        actor_threads,
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
        writeln!(
            out,
            "{pad3}harc_rt::harc_assign({}, {lane_rsp_data}[(size_t)_sel]);",
            wire("rsp_data")
        )
        .ok();
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
        "{pad3}{instance}._last_out_cycle = (uint64_t)cycle_count;"
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

/// Emit one closure-hook body (`FunctionKind::TestHook`) as a free
/// `[&]`-capturing lambda named by `func.name`. Structurally identical to
/// a transactor method (synchronous loop-switch, `tick()`-based waits)
/// since a hook fires inside the synchronous method call / `record_write`
/// dispatch, not the run coroutine. The body resolves the shared `_tb`
/// host struct, the firing transactor's state, and the regblock mirror
/// (all `[&]`-captured) — v1's reference-capturing hook closure.
pub(super) fn emit_test_hook(
    out: &mut String,
    prog: &TbProgram,
    func: &TbFunction,
    dut_type: &str,
    depth: usize,
) -> Result<(), EmitError> {
    let names = cpp_local_names(func);
    let empty_lanes = HashMap::new();
    let cx = ECx {
            prog: Some(prog),
        func,
        names: &names,
        lanes: &empty_lanes,
        self_subst: None,
        dut_type,
        trace_component: "",
        state_receiver: None,
    };
    let nparams = func.params.len();
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let pad3 = INDENT.repeat(depth + 3);

    // Hooks are void in this subset; a value-returning hook is never
    // produced by lowering (the firing site discards any result).
    let param_ty = |i: usize| match func.locals[i].ty {
        IrType::Record(r) => prog.records[r.index()].name.clone(),
        IrType::RecordSeq(r) => format!("std::vector<{}>", prog.records[r.index()].name),
        IrType::Seq(ref scalar) => format!("std::vector<{}>", super::local_scalar_cty(scalar)),
        _ => "uint64_t".to_string(),
    };
    let params = names[..nparams]
        .iter()
        .enumerate()
        .map(|(i, n)| format!("{} {n}", param_ty(i)))
        .collect::<Vec<_>>()
        .join(", ");
    // A per-register `on regs.REG` write callback can re-enter
    // `record_write` and thus call ITSELF — a direct `auto` lambda cannot
    // reference its own deduced type in its initializer, so hooks are
    // declared as a forward `std::function` slot, then assigned (mirrors
    // v1's `std::function` callback holder). Method pre/post hooks never
    // self-recurse but use the same shape uniformly.
    let sig_params = (0..nparams).map(param_ty).collect::<Vec<_>>().join(", ");
    writeln!(out, "{pad}std::function<void({sig_params})> {};", func.name).ok();
    writeln!(out, "{pad}{} = [&]({params}) -> void {{", func.name).ok();
    declare_locals(out, prog, func, &names, nparams, depth + 1)?;
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
                let cond = expr_cpp(&cx, c)?;
                writeln!(
                    out,
                    "{pad3}if ({cond}) {{ __bb = {}; }} else {{ __bb = {}; }}",
                    t.0, f.0
                )
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
            Terminator::WaitTimePs(ps, b) => {
                // Same wall-clock settle support as the method lambdas that
                // invoke hooks: closure hooks capture scheduler time by reference.
                writeln!(out, "{pad3}eval_clocks_until(now_ps + {ps});").ok();
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
