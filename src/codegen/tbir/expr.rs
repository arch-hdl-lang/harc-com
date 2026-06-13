//! TB-IR expression → C++ text.

use crate::codegen::cpp_tb::EmitError;
use crate::ir::{BinOp, CallTarget, Expr, FmtArg, PortRef, TbFunction, UnOp, WidthCastKind};
use std::collections::HashMap;
use std::fmt::Write as _;

/// Per-function emission context: the function (for diagnostics), the
/// emitted local names, and the `--sv` packed-lane width table
/// (`EmitOpts::vec_lane_widths`) that routes `dut.<port>[i]` lane
/// accesses through `harc_rt::harc_vec_lane_*<W>` (v1's
/// `dut_packed_lane` split).
pub(super) struct ECx<'a> {
    pub func: &'a TbFunction,
    pub names: &'a [String],
    pub lanes: &'a HashMap<String, u32>,
}

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
/// same convention the v1 backend uses for bus-bundle members. The
/// bare signal, with no lane applied (lane handling is per call site:
/// reads via `port_read`, writes in `func.rs`).
pub(super) fn port_signal(p: &PortRef) -> String {
    format!("dut->{}", p.port_path.join("_"))
}

pub(super) fn port_read(cx: &ECx<'_>, p: &PortRef) -> String {
    let sig = port_signal(p);
    match p.lane {
        None => format!("harc_rt::harc_read({sig})"),
        // Packed multi-lane port: bit-extract through the runtime
        // helper. True unpacked-array port: raw subscript (correct on
        // both backends; v1 emits the same, with no harc_read wrap).
        Some(lane) => match lane_width(cx, p) {
            Some(w) => {
                format!("harc_rt::harc_vec_lane_read<{w}>({sig}, (std::size_t)({lane}))")
            }
            None => format!("{sig}[{lane}]"),
        },
    }
}

/// Lane width when the port is in the packed-lane table (single-
/// segment DUT ports only — the table is keyed by top-level names).
pub(super) fn lane_width(cx: &ECx<'_>, p: &PortRef) -> Option<u32> {
    match p.port_path.as_slice() {
        [name] => cx.lanes.get(name).copied(),
        _ => None,
    }
}

/// Render an IR expression.
pub(super) fn expr_cpp(cx: &ECx<'_>, e: &Expr) -> Result<String, EmitError> {
    Ok(match e {
        Expr::Literal { value, .. } => format!("{value}"),
        Expr::WideLiteral(words) => wide_literal_cpp(words),
        Expr::Local(l) => cx
            .names
            .get(l.index())
            .cloned()
            .ok_or_else(|| {
                EmitError(format!("tbir: dangling local %{} in {}", l.0, cx.func.name))
            })?,
        Expr::Port(p) => port_read(cx, p),
        // Record-field read on a record-typed local: `t.tag`. The
        // lowering validated the field against the schema.
        Expr::RecordField { local, field } => {
            let name = cx.names.get(local.index()).cloned().ok_or_else(|| {
                EmitError(format!(
                    "tbir: dangling local %{} in {}",
                    local.0, cx.func.name
                ))
            })?;
            format!("{name}.{field}")
        }
        // Scalar testbench field read — a `_tb` struct member (scalar
        // fields exist only on non-synthetic testbenches).
        Expr::TbField(field) => format!("_tb.{field}"),
        Expr::Binary(op, a, b) => {
            // Wide-literal == / != routing: when either operand is a
            // > 128-bit literal, compare word-by-word through
            // `harc_eq_words` (v1's special case — `_harc_u128` would
            // silently truncate the literal).
            if matches!(op, BinOp::Eq | BinOp::Ne) {
                if let Some(s) = wide_eq_cpp(cx, *op, a, b)? {
                    return Ok(s);
                }
            }
            let a = expr_cpp(cx, a)?;
            let b = expr_cpp(cx, b)?;
            format!("({a} {} {b})", bin_op_cpp(*op))
        }
        Expr::Unary(op, a) => {
            let a = expr_cpp(cx, a)?;
            format!("{}({a})", un_op_cpp(*op))
        }
        Expr::Ternary(c, t, e2) => {
            // v1 wraps the whole conditional in parens so it cannot
            // bind into a surrounding higher-precedence operator.
            let c = expr_cpp(cx, c)?;
            let t = expr_cpp(cx, t)?;
            let e2 = expr_cpp(cx, e2)?;
            format!("({c} ? {t} : {e2})")
        }
        Expr::WidthCast {
            kind,
            width,
            src_width,
            inner,
        } => width_cast_cpp(cx, *kind, *width, *src_width, inner)?,
        // Check-phase bin counter read — the covergroup instance lives
        // in the `_tb` struct (cov fields exist only on non-synthetic
        // testbenches, so `_tb` is always in scope here).
        Expr::CovBin { inst, point, bin } => {
            format!("_tb.{}.{point}.{bin}", inst.tb_field)
        }
        Expr::Call(target, args) => {
            let name = match target {
                CallTarget::Helper(n) => helper_cpp_name(n),
                CallTarget::Builtin(_) => {
                    return Err(EmitError(
                        "tbir: builtin calls are not emitted yet (lowering should \
                         have rejected them)"
                            .to_string(),
                    ));
                }
                CallTarget::TransactorMethod { bus_field, method } => {
                    // Call edges never emit from expression position:
                    // bus-bound edges emit only as a whole Assign RHS
                    // (func.rs `emit_transactor_call`), transactor-bound
                    // edges only as `Stmt::TransactorCall`. Reaching here
                    // means the verifier's invariant was bypassed.
                    return Err(EmitError(format!(
                        "tbir: transactor call edge `{bus_field}.{method}` in expression \
                         position — verifier pins it to Assign-RHS / TransactorCall \
                         (lowering/pass bug)"
                    )));
                }
            };
            let mut rendered = Vec::with_capacity(args.len());
            for a in args {
                rendered.push(expr_cpp(cx, a)?);
            }
            format!("{name}({})", rendered.join(", "))
        }
    })
}

/// General-position wide-literal rendering, mirroring v1's
/// `c_value_literal`: ≤ 128 bits → `_harc_u128` shifted-OR composite;
/// above → `harc_rt::HarcWide<N>` brace-init.
fn wide_literal_cpp(words: &[u32]) -> String {
    if words.len() <= 4 {
        let mut padded = [0u32; 4];
        padded[..words.len()].copy_from_slice(words);
        let lo = (padded[0] as u64) | ((padded[1] as u64) << 32);
        let hi = (padded[2] as u64) | ((padded[3] as u64) << 32);
        return format!("(((_harc_u128)0x{hi:x}ULL << 64) | (_harc_u128)0x{lo:x}ULL)");
    }
    let mut out = format!("harc_rt::HarcWide<{}>({{", words.len());
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!("0x{w:x}u"));
    }
    out.push_str("})");
    out
}

/// The word list when `e` is a > 128-bit wide literal (the
/// `harc_eq_words`/`harc_assign_words` routing threshold — ≤ 128-bit
/// literals flow through the `_harc_u128` composite like v1).
pub(super) fn wide_words_over_128(e: &Expr) -> Option<&[u32]> {
    match e {
        Expr::WideLiteral(words) if words.len() > 4 => Some(words),
        _ => None,
    }
}

/// `harc_eq_words` routing for `==`/`!=` with a > 128-bit literal
/// operand. The signal side passes as an L-value (no `harc_read`
/// wrap) so the helper sees the raw `VlWide<N>` — v1's shape.
fn wide_eq_cpp(
    cx: &ECx<'_>,
    op: BinOp,
    lhs: &Expr,
    rhs: &Expr,
) -> Result<Option<String>, EmitError> {
    let lhs_words = wide_words_over_128(lhs);
    let rhs_words = wide_words_over_128(rhs);
    let Some(words) = lhs_words.or(rhs_words) else {
        return Ok(None);
    };
    // The signal side is whichever operand is not the wide literal
    // (prefer treating rhs as the literal, like v1).
    let sig_side = if rhs_words.is_some() { lhs } else { rhs };
    let sig = match sig_side {
        Expr::Port(p) if p.lane.is_none() => port_signal(p),
        other => expr_cpp(cx, other)?,
    };
    let list = words
        .iter()
        .map(|w| format!("0x{w:x}u"))
        .collect::<Vec<_>>()
        .join(", ");
    let eq = format!("harc_rt::harc_eq_words({sig}, {{{list}}})");
    Ok(Some(match op {
        BinOp::Eq => eq,
        _ => format!("(!{eq})"),
    }))
}

/// Width-method emission, ≤ 64-bit subset of v1's
/// `try_emit_width_method` (lowering rejects wider). All casts target
/// `uint64_t` (v1's `cpp_uint_for_width` for every width ≤ 64), so:
/// trunc masks (plain cast at width 64), zext is a plain cast, sext
/// shift-fills when the source width is known and smaller (plain cast
/// otherwise), resize narrows with a mask and widens with a plain
/// cast (mask-narrow when the source width is unknown).
fn width_cast_cpp(
    cx: &ECx<'_>,
    kind: WidthCastKind,
    width: u32,
    src_width: Option<u32>,
    inner: &Expr,
) -> Result<String, EmitError> {
    let e = expr_cpp(cx, inner)?;
    let mask = |w: u32| (1u64 << w) - 1;
    let trunc_shape = |e: &str| {
        if width == 64 {
            format!("((uint64_t)({e}))")
        } else {
            format!("((uint64_t)((({e}) & 0x{:X}ULL)))", mask(width))
        }
    };
    let plain_cast = |e: &str| format!("((uint64_t)({e}))");
    Ok(match kind {
        WidthCastKind::Trunc => trunc_shape(&e),
        WidthCastKind::Zext => plain_cast(&e),
        WidthCastKind::Sext => match src_width {
            Some(sw) if sw < width => {
                let shift = 64 - sw;
                if width == 64 {
                    format!("((uint64_t)(((int64_t)((uint64_t)({e}) << {shift})) >> {shift}))")
                } else {
                    format!(
                        "((uint64_t)(((int64_t)((uint64_t)({e}) << {shift})) >> {shift}) \
                         & 0x{:X}ULL)",
                        mask(width)
                    )
                }
            }
            _ => plain_cast(&e),
        },
        WidthCastKind::Resize => match src_width {
            Some(sw) if width < sw => trunc_shape(&e),
            Some(_) => plain_cast(&e),
            // Unknown source width — default to mask-narrow (v1).
            None => trunc_shape(&e),
        },
    })
}

/// Render one pre-parsed `${...}` capture as a printf argument,
/// mirroring v1's `emit_interp_arg` (long-long ABI or wide-hex helper).
pub(super) fn fmt_arg_cpp(cx: &ECx<'_>, arg: &FmtArg) -> Result<String, EmitError> {
    let inner = expr_cpp(cx, &arg.expr)?;
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
