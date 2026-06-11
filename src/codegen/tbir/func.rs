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

use super::expr::{escape_c, expr_cpp, fmt_arg_cpp, port_lvalue, port_read};
use crate::codegen::cpp_tb::EmitError;
use crate::ir::{FileLogLevel, FmtArgs, LogLevel, Stmt, TbFunction, Terminator};
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
    "_harc_trace_dump_at", "__bb", "__done",
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
/// can share one coroutine body without name collisions.
pub(super) fn emit_function(
    out: &mut String,
    func: &TbFunction,
    depth: usize,
) -> Result<(), EmitError> {
    let names = cpp_local_names(func);
    let pad = INDENT.repeat(depth);
    let pad1 = INDENT.repeat(depth + 1);
    let pad2 = INDENT.repeat(depth + 2);
    let pad3 = INDENT.repeat(depth + 3);

    writeln!(out, "{pad}{{ // {} (TB-IR loop-switch)", func.name).ok();
    for n in &names {
        writeln!(out, "{pad1}uint64_t {n} = 0; (void){n};").ok();
    }
    writeln!(out, "{pad1}int __bb = {};", func.entry.0).ok();
    writeln!(out, "{pad1}bool __done = false;").ok();
    writeln!(out, "{pad1}while (!__done) {{").ok();
    writeln!(out, "{pad2}switch (__bb) {{").ok();
    for (bi, block) in func.blocks.iter().enumerate() {
        writeln!(out, "{pad2}case {bi}: {{").ok();
        for s in &block.stmts {
            emit_stmt(out, func, &names, s, depth + 3)?;
        }
        match &block.terminator {
            Terminator::Jump(b) => {
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::Branch(c, t, f) => {
                let cond = expr_cpp(func, &names, c)?;
                writeln!(out, "{pad3}if ({cond}) {{ __bb = {}; }} else {{ __bb = {}; }}", t.0, f.0)
                    .ok();
            }
            Terminator::WaitCycles(n, b) => {
                let n = expr_cpp(func, &names, n)?;
                writeln!(
                    out,
                    "{pad3}co_await harc_rt::wait_cycles(_slot, (uint32_t)({n}));"
                )
                .ok();
                writeln!(out, "{pad3}__bb = {};", b.0).ok();
            }
            Terminator::Return => {
                writeln!(out, "{pad3}__done = true;").ok();
            }
            Terminator::Fatal(args) => {
                emit_log_call(out, func, &names, "FATAL", None, args, depth + 3)?;
                writeln!(out, "{pad3}errors++; _fatal = true;").ok();
                writeln!(out, "{pad3}__done = true;").ok();
            }
            Terminator::WaitUntil { .. } | Terminator::WaitUntilTimeout { .. } => {
                return Err(EmitError(format!(
                    "tbir: `wait until` reached codegen in {} — lowering should have rejected it",
                    func.name
                )));
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

fn emit_stmt(
    out: &mut String,
    func: &TbFunction,
    names: &[String],
    s: &Stmt,
    depth: usize,
) -> Result<(), EmitError> {
    let pad = INDENT.repeat(depth);
    match s {
        Stmt::Assign(l, e) => {
            let name = &names[l.index()];
            let e = expr_cpp(func, names, e)?;
            writeln!(out, "{pad}{name} = {e};").ok();
        }
        Stmt::DutWrite(p, e) => {
            let e = expr_cpp(func, names, e)?;
            writeln!(out, "{pad}harc_rt::harc_assign({}, {e});", port_lvalue(p)).ok();
        }
        Stmt::DutRead(l, p) => {
            let name = &names[l.index()];
            writeln!(out, "{pad}{name} = {};", port_read(p)).ok();
        }
        Stmt::Log { level, args } => {
            let (sev, file) = match level {
                LogLevel::Info => ("INFO", None),
                LogLevel::Warn => ("WARN", None),
                LogLevel::Error => ("ERROR", None),
                LogLevel::Fatal => ("FATAL", None),
                LogLevel::File { path, level } => (
                    match level {
                        FileLogLevel::Info => "INFO",
                        FileLogLevel::Warn => "WARN",
                        FileLogLevel::Error => "ERROR",
                        FileLogLevel::Fatal => "FATAL",
                    },
                    Some(path.as_str()),
                ),
            };
            emit_log_call(out, func, names, sev, file, args, depth)?;
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
            let cond = expr_cpp(func, names, cond)?;
            writeln!(out, "{pad}if (!({cond})) {{").ok();
            emit_log_call(out, func, names, "FAIL", None, on_fail, depth + 1)?;
            writeln!(out, "{pad}{INDENT}errors++;").ok();
            writeln!(out, "{pad}}}").ok();
        }
        Stmt::CovReport(inst) => {
            writeln!(out, "{pad}_tb.{}.report();", inst.tb_field).ok();
        }
    }
    Ok(())
}

/// `sim_log_line("SEV", "fmt", args...)` or the `sim_logf_line` file
/// variant — identical call shape to v1 so runtime log/trace text
/// matches byte-for-byte.
fn emit_log_call(
    out: &mut String,
    func: &TbFunction,
    names: &[String],
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
        let rendered = fmt_arg_cpp(func, names, a)?;
        write!(out, ", {rendered}").ok();
    }
    writeln!(out, ");").ok();
    Ok(())
}
