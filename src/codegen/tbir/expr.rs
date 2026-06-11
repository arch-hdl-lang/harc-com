//! TB-IR expression → C++ text.

use crate::codegen::cpp_tb::EmitError;
use crate::ir::{BinOp, CallTarget, Expr, FmtArg, PortRef, TbFunction, UnOp};
use std::fmt::Write as _;

/// C++ symbol for a lowered pure-helper function. Prefixed so a HARC
/// helper name can never collide with scaffolding identifiers or C++
/// keywords.
pub(super) fn helper_cpp_name(name: &str) -> String {
    format!("harc_helper_{name}")
}

/// C-escape a string for placement inside a C++ string literal.
pub(super) fn escape_c(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

/// `dut-><flat_port_name>` — dotted sub-paths flatten with `_`, the
/// same convention the v1 backend uses for bus-bundle members.
pub(super) fn port_lvalue(p: &PortRef) -> String {
    format!("dut->{}", p.port_path.join("_"))
}

pub(super) fn port_read(p: &PortRef) -> String {
    format!("harc_rt::harc_read({})", port_lvalue(p))
}

/// Render an IR expression. `names` maps `LocalId` index → emitted C++
/// variable name.
pub(super) fn expr_cpp(
    func: &TbFunction,
    names: &[String],
    e: &Expr,
) -> Result<String, EmitError> {
    Ok(match e {
        Expr::Literal { value, .. } => format!("{value}"),
        Expr::Local(l) => names
            .get(l.index())
            .cloned()
            .ok_or_else(|| EmitError(format!("tbir: dangling local %{} in {}", l.0, func.name)))?,
        Expr::Port(p) => port_read(p),
        Expr::Binary(op, a, b) => {
            let a = expr_cpp(func, names, a)?;
            let b = expr_cpp(func, names, b)?;
            format!("({a} {} {b})", bin_op_cpp(*op))
        }
        Expr::Unary(op, a) => {
            let a = expr_cpp(func, names, a)?;
            format!("{}({a})", un_op_cpp(*op))
        }
        // Check-phase bin counter read — the covergroup instance lives
        // in the `_tb` struct (cov fields exist only on non-synthetic
        // testbenches, so `_tb` is always in scope here).
        Expr::CovBin { inst, point, bin } => {
            format!("_tb.{}.{point}.{bin}", inst.tb_field)
        }
        Expr::Call(target, args) => {
            let name = match target {
                CallTarget::Helper(n) => helper_cpp_name(n),
                CallTarget::Builtin(_) | CallTarget::TransactorMethod { .. } => {
                    return Err(EmitError(
                        "tbir: builtin/transactor calls are not emitted yet (lowering should \
                         have rejected them)"
                            .to_string(),
                    ));
                }
            };
            let mut rendered = Vec::with_capacity(args.len());
            for a in args {
                rendered.push(expr_cpp(func, names, a)?);
            }
            format!("{name}({})", rendered.join(", "))
        }
    })
}

/// Render one pre-parsed `${...}` capture as a printf argument,
/// mirroring v1's `emit_interp_arg` (long-long ABI or wide-hex helper).
pub(super) fn fmt_arg_cpp(
    func: &TbFunction,
    names: &[String],
    arg: &FmtArg,
) -> Result<String, EmitError> {
    let inner = expr_cpp(func, names, &arg.expr)?;
    let mut out = String::new();
    match arg.wide_hex {
        Some((width, upper)) => {
            let upper_str = if upper { "true" } else { "false" };
            let helper = if width > 32 {
                "harc_rt::HarcHexBufWide"
            } else {
                "harc_rt::HarcHexBuf128"
            };
            write!(out, "(const char*){helper}({inner}, {width}, {upper_str})").ok();
        }
        None => {
            write!(out, "harc_rt::harc_printf_ll({inner})").ok();
        }
    }
    Ok(out)
}

fn bin_op_cpp(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "&&",
        BinOp::Or => "||",
        BinOp::BitAnd => "&",
        BinOp::BitOr => "|",
        BinOp::BitXor => "^",
        BinOp::Shl => "<<",
        BinOp::Shr => ">>",
    }
}

fn un_op_cpp(op: UnOp) -> &'static str {
    match op {
        UnOp::Neg => "-",
        UnOp::Not => "!",
        UnOp::BitNot => "~",
    }
}
