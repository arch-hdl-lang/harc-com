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
    /// The whole program, for schema-driven type lookups —
    /// `expr_is_signed` resolves record fields, `_tb` scalar fields,
    /// transactor state, and component fields through it so `sint`
    /// host state gets an arithmetic `>>` (#524 adversarial-review
    /// finding 6 + residual). `None` only in contexts that are
    /// scalar-only by construction (pure helpers).
    pub prog: Option<&'a crate::ir::TbProgram>,
    /// Simple name of the DUT type (`CpuPipe`). Used to form the
    /// Verilator-mangled probe accessor `dut->rootp-><DutType>__DOT__
    /// harc_probes__DOT__<name>` for `PortAccess::Probe`/`Force` reads
    /// and writes. Empty (`""`) in contexts that can never host a probe
    /// access (pure helpers, tseqs, transactor methods, component
    /// lambdas — probes are test-scope only); the probe-accessor path is
    /// structurally unreachable there.
    pub dut_type: &'a str,
    /// When `Some`, a `ComponentField`/`ComponentIdle` with a `SelfField`
    /// base renders against this instance path instead of the literal
    /// `self`. Used when a watchdog/periodic clause expr (lowered in the
    /// component-self context) is emitted inside a per-instance
    /// `_checkers` closure, which has no `self` in scope.
    pub self_subst: Option<&'a str>,
    /// Component-instance context for `tlm_call` trace events emitted by
    /// a bus-call edge inside this body (v1's
    /// `current_component_instance`). Empty (`""`) at test-run scope; the
    /// responder-instance name (`"target"`) when emitting a bound-to
    /// target responder body, so a downstream forwarded `back.read(...)`
    /// initiator trace event carries the same `component` field v1
    /// records. Most ECx sites can never host such an edge and leave it
    /// `""`.
    pub trace_component: &'a str,
    /// State-receiver name (#494 P1b) for empty-instance
    /// `TransactorState`/`TransactorStateWrite` nodes inside a type-shared
    /// stateful-transactor method body. `Some("self_state")` while
    /// emitting such a body so `self.<field>` renders against the
    /// per-instance struct the caller passed by reference; `None`
    /// everywhere else (an empty instance is then a lowering/pass bug).
    pub state_receiver: Option<&'a str>,
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
///
/// For a `PortAccess::Probe`/`Force` reference, the signal is NOT a
/// top-level DUT port but a DUT-internal value surfaced through the SV
/// `bind` stub: it routes through `dut->rootp-><DutType>__DOT__
/// harc_probes__DOT__<name>` (the read-side accessor; force probes
/// additionally carry `_drv`/`_en` siblings handled in `func.rs`).
/// Mirrors v1's `Emitter::probes` lowering. See docs/probe-signals.md.
pub(super) fn port_signal(cx: &ECx<'_>, p: &PortRef) -> String {
    match p.access {
        crate::ir::PortAccess::Port => {
            let sep = if p.aggregate_path { "." } else { "_" };
            format!("dut->{}", p.port_path.join(sep))
        }
        crate::ir::PortAccess::Probe | crate::ir::PortAccess::Force => {
            format!("dut->rootp->{}", probe_read_accessor(cx.dut_type, p))
        }
    }
}

/// Verilator-mangled read-side accessor for a probe (`<DutType>__DOT__
/// harc_probes__DOT__<name>`). `name` is the single-segment probe name
/// in `port_path`. Matches `crate::codegen::sv_stub::mangled_accessor`.
pub(super) fn probe_read_accessor(dut_type: &str, p: &PortRef) -> String {
    crate::codegen::sv_stub::mangled_accessor(dut_type, &p.port_path.join("_"))
}

pub(super) fn port_read(cx: &ECx<'_>, p: &PortRef) -> Result<String, EmitError> {
    let sig = port_signal(cx, p);
    Ok(match &p.lane {
        None => format!("harc_rt::harc_read({sig})"),
        // Packed multi-lane port: bit-extract through the runtime
        // helper. True unpacked-array port: raw subscript (correct on
        // both backends; v1 emits the same, with no harc_read wrap).
        // The lane index is a constant literal or a runtime expression —
        // v1's `dut_packed_lane` re-renders an arbitrary `&Expr` here.
        Some(lane) => {
            let idx = lane_index_cpp(cx, lane)?;
            match lane_width(cx, p) {
                Some(w) => {
                    format!("harc_rt::harc_vec_lane_read<{w}>({sig}, (std::size_t)({idx}))")
                }
                None => format!("{sig}[{idx}]"),
            }
        }
    })
}

/// Render a lane index: a constant folds to its literal, a runtime index
/// lowers like any other IR value expression.
pub(super) fn lane_index_cpp(
    cx: &ECx<'_>,
    lane: &crate::ir::LaneIndex,
) -> Result<String, EmitError> {
    match lane {
        crate::ir::LaneIndex::Const(c) => Ok(c.to_string()),
        crate::ir::LaneIndex::Var(e) => expr_cpp(cx, e),
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

/// Resolve the C++ receiver for a transactor-state access. A non-empty
/// `instance` is a fully-bound test-scope read (`a.calls`) and renders as
/// itself. An EMPTY instance is a type-shared method-body placeholder
/// (#494 P1b) and renders against the per-call state receiver
/// (`self_state`); reaching an empty instance with no receiver in scope
/// means a lowering/pass bug left a placeholder unbound.
pub(super) fn resolve_state_instance<'a>(
    cx: &ECx<'a>,
    instance: &'a str,
) -> Result<&'a str, EmitError> {
    if !instance.is_empty() {
        return Ok(instance);
    }
    cx.state_receiver.ok_or_else(|| {
        EmitError(format!(
            "tbir: empty-instance transactor-state access in {} with no state receiver \
             in scope (unfilled placeholder — lowering/pass bug)",
            cx.func.name
        ))
    })
}

/// Render an IR expression.
pub(super) fn expr_cpp(cx: &ECx<'_>, e: &Expr) -> Result<String, EmitError> {
    Ok(match e {
        Expr::Literal { value, ty } => match ty {
            crate::ir::IrType::SInt(_) => signed_literal_cpp(*value),
            // Keep unsigned file-scope constants at uint64_t rank so C++
            // applies the same usual-arithmetic conversions as v1.
            crate::ir::IrType::UInt(_) => format!("((uint64_t)({value}))"),
            _ => format!("{value}"),
        },
        // The framework-provided cycle counter — emitted as the in-scope
        // `cycle_count` (a captured `ctx.cycle_count` reference), matching
        // v1's bare-ident emission of `cycle_count`.
        Expr::CycleCount => "(uint64_t)cycle_count".to_string(),
        // The framework error counter. Use the context member directly so
        // source locals named `errors` cannot shadow assertion accounting.
        Expr::ErrorCount => "ctx.errors".to_string(),
        Expr::WideLiteral(words) => wide_literal_cpp(words),
        Expr::Local(l) => cx.names.get(l.index()).cloned().ok_or_else(|| {
            EmitError(format!("tbir: dangling local %{} in {}", l.0, cx.func.name))
        })?,
        Expr::Port(p) => port_read(cx, p)?,
        // Record-field read on a record-typed local: `t.tag`. The
        // lowering validated the field against the schema.
        Expr::RecordField {
            local,
            field,
            path,
            mid_indices,
            index,
        } => {
            let name = cx.names.get(local.index()).cloned().ok_or_else(|| {
                EmitError(format!(
                    "tbir: dangling local %{} in {}",
                    local.0, cx.func.name
                ))
            })?;
            record_access_cpp(cx, &name, field, path, mid_indices, index.as_deref())?
        }
        // Register-level frontdoor read in a general expression position
        // (assert condition / format arg). v1's inline assignment-
        // expression: RW/RO does the bus read AND predicts the mirror in
        // one expression (`(regs.NAME = <Helper>_read(off))`); WO serves
        // from the mirror cell. The `read` lambda is a plain C++ call —
        // not the bus wire protocol — so this is a legitimate value.
        Expr::RegRead {
            mirror,
            helper_ty,
            field,
            offset,
            reads_bus,
        } => {
            let name = cx.names.get(mirror.index()).cloned().ok_or_else(|| {
                EmitError(format!(
                    "tbir: dangling mirror local %{} in {}",
                    mirror.0, cx.func.name
                ))
            })?;
            if *reads_bus {
                format!("({name}.{field} = {helper_ty}_read({offset}))")
            } else {
                format!("{name}.{field}")
            }
        }
        // Scalar testbench field read — a `_tb` struct member (scalar
        // fields exist only on non-synthetic testbenches).
        Expr::TbField(field) => format!("_tb.{field}"),
        // Latch readings render against the per-closure cells the
        // concurrent-check emitter declares (`func::emit_property_check`
        // / `emit_cover_check`). `_harc_ps<i>` is the `static` previous
        // value, `_harc_cur<i>` this cycle's — both scoped to the one
        // `_checkers` closure, so plain indexed names cannot collide
        // across checks the way v1's span-tagged names guard against.
        Expr::TemporalSlot { slot, kind } => match kind {
            crate::ir::TemporalFn::Past => format!("_harc_ps{slot}"),
            crate::ir::TemporalFn::Rose => format!("(!_harc_ps{slot} && _harc_cur{slot})"),
            crate::ir::TemporalFn::Fell => format!("(_harc_ps{slot} && !_harc_cur{slot})"),
            crate::ir::TemporalFn::Stable => format!("(_harc_ps{slot} == _harc_cur{slot})"),
        },
        Expr::TbQueueQuery { field, query } => match query {
            crate::ir::ScoreboardQuery::QueueSize { .. } => {
                format!("((uint64_t)_tb.{field}.size())")
            }
            crate::ir::ScoreboardQuery::QueueEmpty { .. } => format!("_tb.{field}.empty()"),
            crate::ir::ScoreboardQuery::Scalar { .. } => {
                return Err(EmitError(format!(
                    "tbir: scalar query on testbench queue `{field}`"
                )));
            }
        },
        // Bound-to target transactor instance state — a member of the
        // generated per-instance struct (`<instance>.<field>`), matching
        // v1's `field_subs` substitution at the responder body and the
        // direct struct access at the test-scope read.
        Expr::TransactorState { instance, field } => {
            format!("{}.{field}", resolve_state_instance(cx, instance)?)
        }
        // Bound-to target transactor whole-record state SUBFIELD read —
        // a nested member of the value-record struct on the per-instance
        // struct (`<instance>.<field>.<path…>`). Routed through the same
        // state-receiver resolver as scalar/queue state.
        Expr::TransactorStateRecordField {
            instance,
            field,
            path,
        } => format!(
            "{}.{field}.{}",
            resolve_state_instance(cx, instance)?,
            path.join(".")
        ),
        // Bound-to target transactor `queue<T>` state field size/empty
        // read — a `harc_rt::HarcQueue<T>` member of the per-instance
        // struct. Mirrors the scoreboard/component queue-query shapes.
        Expr::TransactorStateQueueQuery {
            instance,
            field,
            query,
        } => {
            use crate::ir::ScoreboardQuery;
            let instance = resolve_state_instance(cx, instance)?;
            match query {
                ScoreboardQuery::QueueSize { .. } => {
                    format!("((uint64_t){instance}.{field}.size())")
                }
                ScoreboardQuery::QueueEmpty { .. } => {
                    format!("{instance}.{field}.empty()")
                }
                // Scalar/pop never appear on a state-queue query (lowering
                // routes them elsewhere); render defensively.
                ScoreboardQuery::Scalar { .. } => format!("{instance}.{field}"),
            }
        }
        // Scoreboard read on a `_tb` struct member (scoreboard fields
        // exist only on non-synthetic testbenches). Mirrors v1's direct
        // struct/queue access.
        Expr::ScoreboardQuery {
            field,
            query,
            nested_path,
            ..
        } => {
            use crate::ir::ScoreboardQuery;
            // `None` → testbench field (`_tb.<field>`); `Some(path)` →
            // env-nested data scoreboard, accessed by the run-scope path.
            // A `self`-rooted path re-roots at the running instance via
            // `self_subst` (self-relative sub-scoreboard read in a body).
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
            match query {
                ScoreboardQuery::Scalar { scalar } => format!("{base}.{scalar}"),
                // size() returns size_t — cast to the IR's uint64 model
                // so it composes uniformly in arithmetic/comparisons.
                ScoreboardQuery::QueueSize { queue } => {
                    format!("((uint64_t){base}.{queue}.size())")
                }
                ScoreboardQuery::QueueEmpty { queue } => {
                    format!("{base}.{queue}.empty()")
                }
            }
        }
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
            let a_signed = expr_is_signed(cx, a);
            let a_width = expr_shift_width(cx, a);
            let a = expr_cpp(cx, a)?;
            let b = expr_cpp(cx, b)?;
            match op {
                BinOp::Shl if a_width.is_some_and(|w| w > 128) => {
                    let width = a_width.unwrap();
                    format!("harc_rt::harc_wide_mask_bits((({a}) << ({b})), {width})")
                }
                BinOp::Shl if a_width.is_some_and(|w| w > 64) => {
                    let width = a_width.unwrap();
                    format!("harc_rt::harc_shl_u128((_harc_u128)({a}), (uint64_t)({b}), {width})")
                }
                BinOp::Shl => format!("(((uint64_t)({a})) << {b})"),
                BinOp::Shr if a_width.is_some_and(|w| w > 128) && a_signed => {
                    let width = a_width.unwrap();
                    format!("harc_rt::harc_wide_ashr(({a}), (uint64_t)({b}), {width})")
                }
                BinOp::Shr if a_width.is_some_and(|w| w > 128) => {
                    let width = a_width.unwrap();
                    format!("harc_rt::harc_wide_mask_bits((({a}) >> ({b})), {width})")
                }
                BinOp::Shr if a_width.is_some_and(|w| w > 64) && a_signed => {
                    let width = a_width.unwrap();
                    format!("harc_rt::harc_ashr_u128((_harc_u128)({a}), (uint64_t)({b}), {width})")
                }
                BinOp::Shr if a_width.is_some_and(|w| w > 64) => {
                    let width = a_width.unwrap();
                    format!("harc_rt::harc_shr_u128((_harc_u128)({a}), (uint64_t)({b}), {width})")
                }
                BinOp::Shr if a_signed => {
                    format!("(((int64_t)({a})) >> {b})")
                }
                BinOp::Shr => format!("(((uint64_t)({a})) >> {b})"),
                _ => format!("({a} {} {b})", bin_op_cpp(*op)),
            }
        }
        Expr::Unary(op, a) => {
            let width = expr_static_width(cx, a);
            let a = expr_cpp(cx, a)?;
            match op {
                UnOp::BitNot => bit_not_cpp(&a, width),
                _ => format!("{}({a})", un_op_cpp(*op)),
            }
        }
        Expr::Ternary(c, t, e2) => {
            // v1 wraps the whole conditional in parens so it cannot
            // bind into a surrounding higher-precedence operator.
            let c = expr_cpp(cx, c)?;
            let t = expr_cpp(cx, t)?;
            let e2 = expr_cpp(cx, e2)?;
            format!("({c} ? {t} : {e2})")
        }
        Expr::BitSlice { target, hi, lo } => {
            let t = match &**target {
                Expr::Port(p) if p.lane.is_none() => {
                    format!("harc_rt::harc_read({})", port_signal(cx, p))
                }
                other => format!("({})", expr_cpp(cx, other)?),
            };
            let width = hi - lo + 1;
            if width <= 64 {
                format!("harc_rt::harc_bits({t}, {hi}, {lo})")
            } else if width <= 128 {
                format!(
                    "static_cast<_harc_u128>(harc_rt::harc_wide_extract_bits<4>({t}, {lo}, {width}))"
                )
            } else {
                let words = width.div_ceil(32);
                format!("harc_rt::harc_wide_extract_bits<{words}>({t}, {lo}, {width})")
            }
        }
        // Runtime bounds go through the same helper v1 emits. The target
        // is passed UNCAST so overload resolution picks the right
        // `harc_bits`: a scalar converts to `_harc_u128`, and a
        // `HarcWide<N>` binds the wide overload that slices out of all N
        // words. Casting to `_harc_u128` first would go through
        // `HarcWide::operator _harc_u128`, which keeps only the low four
        // words — `w[200:193]` on a `uint<256>` would read 0. A whole
        // port is the one shape that needs a wrapper: the raw Verilator
        // signal is a `WData` array with no `harc_bits` overload, so it
        // widens through `harc_read` (v1's shape) first.
        Expr::BitSliceDyn { target, hi, lo } => {
            let t = match &**target {
                Expr::Port(p) if p.lane.is_none() => {
                    format!("harc_rt::harc_read({})", port_signal(cx, p))
                }
                other => format!("({})", expr_cpp(cx, other)?),
            };
            let hi = expr_cpp(cx, hi)?;
            let lo = expr_cpp(cx, lo)?;
            format!("harc_rt::harc_bits({t}, (uint32_t)({hi}), (uint32_t)({lo}))")
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
        // Hook-param cover target (`cover t.burst`). The `param` name is
        // the hookable method's by-value closure argument, so it renders as
        // a plain member access — same shape as `RecordField`. This is only
        // reached from the hook-sampler closure (see `cover_expr_cpp`); a
        // general expression position never carries one.
        Expr::CovHookParam {
            param,
            field,
            index,
        } => match index {
            Some(idx) => {
                let i = expr_cpp(cx, idx)?;
                format!("{param}.{field}[{i}]")
            }
            None => format!("{param}.{field}"),
        },
        Expr::CovHookArg { param } => param.clone(),
        // Composite-component scalar field read: self-relative inside a
        // method body (`self.count`) or a dotted path from a test-scope
        // component local (`env.sb.count`). Both name plain by-value C++
        // struct members (v1's `emit_component_struct` shape).
        Expr::ComponentField { base, field } => {
            format!("{}.{field}", comp_base_cpp_subst_cx(cx, base))
        }
        Expr::ComponentVecElement { base, field, index } => {
            let index = expr_cpp(cx, index)?;
            format!("{}.{field}[{index}]", comp_base_cpp_subst_cx(cx, base))
        }
        // A whole composite-component value passed by value as a method
        // arg (`sb.observe(addr, model)` reads `model` here). Render the
        // receiver — a plain C++ struct value, copied at the call.
        Expr::ComponentValue { base } => comp_base_cpp_subst_cx(cx, base),
        // Composite-component `queue<T>` size()/empty() read — mirrors the
        // `ScoreboardQuery` queue reads but resolves the receiver via the
        // component base (self-relative or test-scope path).
        Expr::ComponentQueueQuery { base, query } => {
            use crate::ir::ScoreboardQuery;
            let recv = comp_base_cpp_subst(base, cx.self_subst);
            match query {
                // A scalar query never reaches a queue field — defensive.
                ScoreboardQuery::Scalar { scalar } => format!("{recv}.{scalar}"),
                ScoreboardQuery::QueueSize { queue } => {
                    format!("((uint64_t){recv}.{queue}.size())")
                }
                ScoreboardQuery::QueueEmpty { queue } => {
                    format!("{recv}.{queue}.empty()")
                }
            }
        }
        // Heartbeat-idle predicate on a component instance — mirrors v1's
        // `emit_idle_predicate`: compares `cycle_count` minus the
        // `_last_in_cycle`/`_last_out_cycle` stamp against the threshold.
        Expr::ComponentIdle { base, kind, n } => {
            let recv = comp_base_cpp_subst(base, cx.self_subst);
            let n = expr_cpp(cx, n)?;
            match kind {
                crate::ir::IdleKind::In => {
                    format!("(((uint64_t)cycle_count - {recv}._last_in_cycle) >= (uint64_t)({n}))")
                }
                crate::ir::IdleKind::Out => {
                    format!("(((uint64_t)cycle_count - {recv}._last_out_cycle) >= (uint64_t)({n}))")
                }
                crate::ir::IdleKind::Both => format!(
                    "((((uint64_t)cycle_count - {recv}._last_in_cycle) >= (uint64_t)({n})) \
                     && (((uint64_t)cycle_count - {recv}._last_out_cycle) >= (uint64_t)({n})))"
                ),
            }
        }
        // `<seq>.size()` — element count of a RecordSeq local. Cast to the
        // uint64 model so it composes in the loop bound comparison.
        Expr::SeqLen(l) => {
            let name = cx.names.get(l.index()).cloned().ok_or_else(|| {
                EmitError(format!("tbir: dangling local %{} in {}", l.0, cx.func.name))
            })?;
            format!("((uint64_t){name}.size())")
        }
        // `<seq>[<index>]` — record value at `index` in a RecordSeq local
        // (v1's `txns[i]` subscript). Record-valued; used as the RHS of the
        // `for t in <seq>` loop-variable copy.
        Expr::SeqIndex { seq, index } => {
            let name = cx.names.get(seq.index()).cloned().ok_or_else(|| {
                EmitError(format!(
                    "tbir: dangling local %{} in {}",
                    seq.0, cx.func.name
                ))
            })?;
            let idx = expr_cpp(cx, index)?;
            format!("{name}[{idx}]")
        }
        Expr::Call(target, args) => {
            let name = match target {
                CallTarget::Helper(n) => helper_cpp_name(n),
                // Extern reference functions emit with the RAW symbol
                // name (no `harc_helper_` mangling) so the call binds to
                // the user's `extern "C"` definition supplied via
                // `--ref-src`; the forward decl is emitted file-scope.
                CallTarget::ExternFn(n) => n.clone(),
                CallTarget::Builtin(_) => {
                    return Err(EmitError(
                        "tbir: builtin calls are not emitted yet (lowering should \
                         have rejected them)"
                            .to_string(),
                    ));
                }
                // A tseq generator call — `RandomTxns(5)`. Emitted as a
                // direct lambda call (v1's tseq lambda). The result is a
                // `std::vector<Record>` assigned into a RecordSeq local.
                CallTarget::Tseq(n) => n.clone(),
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
                CallTarget::TransactorSelfMethod { transactor, method } => {
                    format!("{transactor}_{method}")
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
        Expr::Port(p) if p.lane.is_none() => port_signal(cx, p),
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

/// Width-method emission through the 1024-bit language limit. Casts ≤ 64
/// bits target `uint64_t`; 65..128-bit casts target v1's `_harc_u128`;
/// wider casts target `HarcWide<ceil(width/32)>`. Per kind:
/// - trunc: ≤64 masks (plain cast at width 64), 65..128 routes through
///   `harc_rt::harc_trunc_u128`, wider values through `harc_wide_trunc`.
/// - zext: scalar cast through 128, then width-normalized `harc_wide_zext`.
/// - sext: shift-fills (≤64) / `harc_rt::harc_sext_u128` (65..128) when
///   the source width is known and smaller; wider values use
///   `harc_wide_sext`.
/// - resize: narrows via trunc-shape, widens via plain cast (mask-narrow
///   when the source width is unknown); the same choice selects wide
///   truncation or zero-extension above 128.
fn width_cast_cpp(
    cx: &ECx<'_>,
    kind: WidthCastKind,
    width: u32,
    src_width: Option<u32>,
    inner: &Expr,
) -> Result<String, EmitError> {
    let e = expr_cpp(cx, inner)?;
    if let Some(words) = super::wide_scalar_words(width) {
        return match kind {
            WidthCastKind::Trunc => Ok(format!("harc_rt::harc_wide_trunc<{words}>({e}, {width})")),
            WidthCastKind::Zext => Ok(match src_width {
                Some(sw) => format!("harc_rt::harc_wide_zext<{words}>({e}, {sw})"),
                None => format!("harc_rt::harc_wide_zext<{words}>({e})"),
            }),
            WidthCastKind::Sext => Ok(match src_width {
                Some(sw) if sw < width => {
                    format!("harc_rt::harc_wide_sext<{words}>({e}, {sw}, {width})")
                }
                Some(sw) => format!("harc_rt::harc_wide_trunc<{words}>({e}, {sw})"),
                None => format!("harc_rt::harc_wide_zext<{words}>({e})"),
            }),
            WidthCastKind::Resize => Ok(match src_width {
                Some(sw) if width < sw => {
                    format!("harc_rt::harc_wide_trunc<{words}>({e}, {width})")
                }
                Some(sw) => format!("harc_rt::harc_wide_zext<{words}>({e}, {sw})"),
                None => format!("harc_rt::harc_wide_trunc<{words}>({e}, {width})"),
            }),
        };
    }
    // The destination C type, mirroring v1's `cpp_uint_for_width`:
    // `_harc_u128` for 65..128-bit casts, `uint64_t` otherwise.
    let c_unsigned = if width > 64 { "_harc_u128" } else { "uint64_t" };
    let mask = |w: u32| (1u64 << w) - 1;
    // Narrow-to-`width` shape (v1's trunc / resize-narrow path). The
    // sub-64 mask narrows to `uint64_t` *before* the `&`: a `HarcWide<N>`
    // receiver converts implicitly to both `uint64_t` and `_harc_u128`,
    // so masking it directly is an ambiguous `operator&`. Narrowing first
    // is value-identical for the `uint64_t` / `_harc_u128` receivers too,
    // since `width < 64` keeps the kept bits inside the low word.
    let trunc_shape = |e: &str| {
        if width > 64 {
            format!("harc_rt::harc_trunc_u128((_harc_u128)({e}), {width})")
        } else if width == 64 {
            format!("((uint64_t)({e}))")
        } else {
            format!("((uint64_t)(((uint64_t)({e}) & 0x{:X}ULL)))", mask(width))
        }
    };
    let plain_cast = |e: &str| format!("(({c_unsigned})({e}))");
    Ok(match kind {
        WidthCastKind::Trunc => trunc_shape(&e),
        WidthCastKind::Zext => plain_cast(&e),
        WidthCastKind::Sext => match src_width {
            Some(sw) if sw < width => {
                if width > 64 {
                    format!("harc_rt::harc_sext_u128((_harc_u128)({e}), {sw}, {width})")
                } else {
                    let shift = 64 - sw;
                    if width == 64 {
                        // `int64_t`, not `uint64_t`: a full-width fill is
                        // the one shape whose sign bit survives into the
                        // result, and v1 binds this expression to `auto`.
                        // Spelling it unsigned there made `auto` deduce
                        // `uint64_t` while TB-IR's own local is `int64_t`,
                        // so `p[7:0].sext<64>() > 0` came out true under v1
                        // and false under TB-IR.
                        format!("((int64_t)(((int64_t)((uint64_t)({e}) << {shift})) >> {shift}))")
                    } else {
                        format!(
                            "((uint64_t)(((int64_t)((uint64_t)({e}) << {shift})) >> {shift}) \
                             & 0x{:X}ULL)",
                            mask(width)
                        )
                    }
                }
            }
            // Narrow to `uint64_t` before the signed relabel: a
            // `HarcWide<N>` receiver converts implicitly to both
            // `uint64_t` and `_harc_u128`, so a bare `(int64_t)` on one is
            // an ambiguous conversion. Value-identical for the scalar
            // receivers, which reinterpret the same low 64 bits either way.
            _ if width <= 64 => format!("((int64_t)((uint64_t)({e})))"),
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

fn bit_not_cpp(e: &str, width: Option<u32>) -> String {
    match width {
        Some(w) if w < 64 => {
            let mask = (1u64 << w) - 1;
            format!("((uint64_t)((~({e}) & 0x{mask:X}ULL)))")
        }
        Some(64) | None => format!("~({e})"),
        Some(w) if w <= 128 => format!("harc_rt::harc_trunc_u128((~((_harc_u128)({e}))), {w})"),
        Some(w) => format!("harc_rt::harc_wide_mask_bits(~({e}), {w})"),
    }
}

fn expr_static_width(cx: &ECx<'_>, e: &Expr) -> Option<u32> {
    match e {
        Expr::Literal { ty, .. } => ir_type_width(ty),
        Expr::Local(id) => cx
            .func
            .locals
            .get(id.0 as usize)
            .and_then(|l| ir_type_width(&l.ty)),
        Expr::Port(p) => p.width,
        Expr::RecordField {
            local, field, path, ..
        } => cx
            .func
            .locals
            .get(local.index())
            .and_then(|l| {
                record_path_type(cx, l.ty.clone(), std::iter::once(field).chain(path.iter()))
            })
            .and_then(|ty| ir_type_width(&ty)),
        Expr::TbField(field) => owner_tb(cx)
            .and_then(|tb| tb.scalar_fields.iter().find(|f| f.name == *field))
            .and_then(|f| ir_type_width(&f.ty)),
        Expr::TransactorState { instance, field } => state_transactor(cx, instance)
            .and_then(|t| t.state_fields.iter().find(|f| f.name == *field))
            .and_then(|f| match &f.kind {
                crate::ir::StateFieldKind::Scalar { ty, .. } => ir_type_width(ty),
                _ => None,
            }),
        Expr::TransactorStateRecordField {
            instance,
            field,
            path,
        } => state_transactor(cx, instance)
            .and_then(|t| t.state_fields.iter().find(|f| f.name == *field))
            .and_then(|f| match f.kind {
                crate::ir::StateFieldKind::Record { record } => {
                    record_path_type(cx, crate::ir::IrType::Record(record), path.iter())
                }
                _ => None,
            })
            .and_then(|ty| ir_type_width(&ty)),
        Expr::ComponentField { base, field } => {
            let path: Vec<String> = field.split('.').map(str::to_string).collect();
            let root = path.first().map(String::as_str).unwrap_or_default();
            component_of_base(cx, base)
                .and_then(|c| c.fields.iter().find(|f| f.name == root))
                .and_then(|f| match f.kind {
                    crate::ir::ComponentFieldKind::Scalar { ref ty, .. } => ir_type_width(ty),
                    crate::ir::ComponentFieldKind::Record { record } => {
                        record_path_type(cx, crate::ir::IrType::Record(record), path[1..].iter())
                            .and_then(|ty| ir_type_width(&ty))
                    }
                    _ => None,
                })
        }
        Expr::ComponentVecElement { base, field, .. } => component_of_base(cx, base)
            .and_then(|c| c.fields.iter().find(|f| f.name == *field))
            .and_then(|f| match &f.kind {
                crate::ir::ComponentFieldKind::FixedVec(vec) => ir_type_width(&vec.elem),
                _ => None,
            }),
        Expr::ScoreboardQuery {
            sb,
            query: crate::ir::ScoreboardQuery::Scalar { scalar },
            ..
        } => cx
            .prog
            .and_then(|p| p.scoreboards.get(sb.index()))
            .and_then(|s| s.fields.iter().find(|f| f.name == *scalar))
            .and_then(|f| match &f.kind {
                crate::ir::ScoreboardFieldKind::Scalar { ty, .. } => ir_type_width(ty),
                _ => None,
            }),
        Expr::Binary(op, a, b) => match op {
            BinOp::Eq
            | BinOp::Ne
            | BinOp::Lt
            | BinOp::Le
            | BinOp::Gt
            | BinOp::Ge
            | BinOp::And
            | BinOp::Or => Some(1),
            _ => expr_static_width(cx, a).or_else(|| expr_static_width(cx, b)),
        },
        Expr::Unary(_, inner) => expr_static_width(cx, inner),
        Expr::Ternary(_, t, f) => expr_static_width(cx, t).or_else(|| expr_static_width(cx, f)),
        Expr::BitSlice { hi, lo, .. } => Some(hi - lo + 1),
        Expr::WidthCast { width, .. } => Some(*width),
        Expr::CycleCount => Some(64),
        _ => None,
    }
}

/// Conservative width propagation for shift operands. Unlike the ordinary
/// expression-width helper, this takes the maximum known operand/branch
/// width so a wide value cannot hide behind a narrow sibling and reach the
/// TB-IR emitter's 64-bit shift implementation. The one narrowing case is
/// `&`: a mask genuinely bounds the result (`wide & 0xFF` is 8 bits no
/// matter how wide the other operand is), so `&` takes the minimum of its
/// known operand bounds, with literal masks bounded by their significant
/// bits rather than their (widthless) type.
fn expr_shift_width(cx: &ECx<'_>, e: &Expr) -> Option<u32> {
    match e {
        Expr::WideLiteral(words) => words.len().checked_mul(32).map(|w| w as u32),
        Expr::Binary(BinOp::BitAnd, lhs, rhs) => {
            let bound = |e: &Expr| -> Option<u32> {
                if let Expr::Literal { value, .. } = e {
                    return Some((64 - value.leading_zeros()).max(1));
                }
                expr_shift_width(cx, e)
            };
            match (bound(lhs), bound(rhs)) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (w, None) | (None, w) => w,
            }
        }
        Expr::Binary(_, lhs, rhs) => expr_shift_width(cx, lhs).max(expr_shift_width(cx, rhs)),
        Expr::Ternary(_, then_expr, else_expr) => {
            expr_shift_width(cx, then_expr).max(expr_shift_width(cx, else_expr))
        }
        Expr::Unary(_, inner) => expr_shift_width(cx, inner),
        Expr::BitSlice { hi, lo, .. } => Some(hi - lo + 1),
        _ => expr_static_width(cx, e),
    }
}

fn expr_is_signed(cx: &ECx<'_>, e: &Expr) -> bool {
    match e {
        Expr::Literal { value, ty } => {
            matches!(ty, crate::ir::IrType::SInt(_))
                // Untyped literals are emitted as ordinary C++ integer
                // literals. Values that fit signed int64_t therefore
                // participate in signed usual-arithmetic conversions when
                // combined with a signed expression (for example
                // `(signed_value + 0) >> 1`).
                || matches!(ty, crate::ir::IrType::Unknown) && *value <= i64::MAX as u64
        }
        Expr::Local(id) => cx
            .func
            .locals
            .get(id.0 as usize)
            .is_some_and(|l| matches!(l.ty, crate::ir::IrType::SInt(_))),
        // Host-state member reads are real C++ struct members whose C
        // type already carries the declared signedness (`int64_t` for a
        // `sint` field — every host-state struct emission maps SInt so).
        // Resolve the declared type through the owning schema so the
        // shift emitter picks the arithmetic form v1's raw member access
        // gets (#524 adversarial-review finding 6 + residual: record
        // fields, `_tb` scalar fields, transactor state, component
        // fields).
        Expr::RecordField {
            local, field, path, ..
        } => {
            let ty = cx
                .func
                .locals
                .get(local.index())
                .map(|l| l.ty.clone())
                .unwrap_or(crate::ir::IrType::Unknown);
            record_path_is_sint(cx, ty, std::iter::once(field).chain(path.iter()))
        }
        Expr::TbField(field) => owner_tb(cx)
            .and_then(|tb| tb.scalar_fields.iter().find(|f| f.name == *field))
            .is_some_and(|f| matches!(f.ty, crate::ir::IrType::SInt(_))),
        Expr::TransactorState { instance, field } => state_transactor(cx, instance)
            .and_then(|t| t.state_fields.iter().find(|f| f.name == *field))
            .is_some_and(|f| {
                matches!(
                    f.kind,
                    crate::ir::StateFieldKind::Scalar {
                        ty: crate::ir::IrType::SInt(_),
                        ..
                    }
                )
            }),
        Expr::TransactorStateRecordField {
            instance,
            field,
            path,
        } => state_transactor(cx, instance)
            .and_then(|t| t.state_fields.iter().find(|f| f.name == *field))
            .is_some_and(|f| match f.kind {
                crate::ir::StateFieldKind::Record { record } => {
                    record_path_is_sint(cx, crate::ir::IrType::Record(record), path.iter())
                }
                _ => false,
            }),
        Expr::ComponentField { base, field } => {
            let path: Vec<String> = field.split('.').map(str::to_string).collect();
            let root = path.first().map(String::as_str).unwrap_or_default();
            component_of_base(cx, base)
                .and_then(|c| c.fields.iter().find(|f| f.name == root))
                .is_some_and(|f| match f.kind {
                    crate::ir::ComponentFieldKind::Scalar {
                        ty: crate::ir::IrType::SInt(_),
                        ..
                    } => true,
                    crate::ir::ComponentFieldKind::Record { record } => {
                        record_path_is_sint(cx, crate::ir::IrType::Record(record), path[1..].iter())
                    }
                    _ => false,
                })
        }
        Expr::ComponentVecElement { base, field, .. } => component_of_base(cx, base)
            .and_then(|c| c.fields.iter().find(|f| f.name == *field))
            .is_some_and(|f| matches!(
                &f.kind,
                crate::ir::ComponentFieldKind::FixedVec(crate::ir::FixedVecSchema {
                    elem: crate::ir::IrType::SInt(_), ..
                })
            )),
        Expr::ScoreboardQuery {
            sb,
            query: crate::ir::ScoreboardQuery::Scalar { scalar },
            ..
        } => cx
            .prog
            .and_then(|p| p.scoreboards.get(sb.index()))
            .and_then(|s| s.fields.iter().find(|f| f.name == *scalar))
            .is_some_and(|f| {
                matches!(
                    f.kind,
                    crate::ir::ScoreboardFieldKind::Scalar {
                        ty: crate::ir::IrType::SInt(_),
                        ..
                    }
                )
            }),
        Expr::Unary(_, inner) => expr_is_signed(cx, inner),
        Expr::Ternary(_, then_expr, else_expr) => {
            expr_is_signed(cx, then_expr) && expr_is_signed(cx, else_expr)
        }
        Expr::WidthCast { kind, .. } => matches!(kind, WidthCastKind::Sext),
        Expr::Binary(op, lhs, rhs) => match op {
            BinOp::Shl | BinOp::Shr => expr_is_signed(cx, lhs),
            BinOp::Div | BinOp::Mod => expr_is_signed(cx, lhs) && expr_is_signed(cx, rhs),
            _ => expr_is_signed(cx, lhs) && expr_is_signed(cx, rhs),
        },
        _ => false,
    }
}

/// Walk `segs` from `ty` through record-typed fields and report whether
/// the final leaf is a `sint`. Nested paths descend record fields; `Vec`
/// element reads use the element type carried in the field's `ty`.
fn record_path_is_sint<'a>(
    cx: &ECx<'_>,
    ty: crate::ir::IrType,
    segs: impl Iterator<Item = &'a String>,
) -> bool {
    record_path_type(cx, ty, segs).is_some_and(|ty| matches!(ty, crate::ir::IrType::SInt(_)))
}

fn record_path_type<'a>(
    cx: &ECx<'_>,
    mut ty: crate::ir::IrType,
    segs: impl Iterator<Item = &'a String>,
) -> Option<crate::ir::IrType> {
    let prog = cx.prog?;
    for seg in segs {
        let crate::ir::IrType::Record(rid) = ty else {
            return None;
        };
        let Some(f) = prog.records.get(rid.index()).and_then(|r| r.field(seg)) else {
            return None;
        };
        ty = f.ty.clone();
    }
    Some(ty)
}

/// The testbench schema owning the function being emitted, when known.
fn owner_tb<'a>(cx: &ECx<'a>) -> Option<&'a crate::ir::TestbenchSchema> {
    cx.prog?.testbenches.get(cx.func.owner?.index())
}

/// The transactor schema owning `instance`'s persistent state. An empty
/// instance means we are inside a shared method/responder body (the
/// state-receiver ABI) — resolve by the body's own function id instead
/// of the (not-yet-bound) instance name.
fn state_transactor<'a>(cx: &ECx<'a>, instance: &str) -> Option<&'a crate::ir::TransactorSchema> {
    let prog = cx.prog?;
    // Inside a shared method/responder body the state field belongs to
    // the transactor owning the body itself — resolve by function id.
    // This also covers a body whose reads carry a baked-in instance
    // name (single-instance bound targets): the body has no owning
    // testbench, so the named lookup below cannot apply there.
    let by_fn = prog.transactors.iter().find(|t| {
        t.methods.iter().any(|m| m.function == cx.func.id)
            || t.target_methods.iter().any(|m| m.function == cx.func.id)
    });
    if by_fn.is_some() || instance.is_empty() {
        return by_fn;
    }
    let tb = owner_tb(cx)?;
    // A stateful instance can be declared three ways: a transactor-typed
    // testbench field, a passive bound target actor (`let target : X
    // passive = bind mem`), or an unbound state-carrying instance.
    let tid = tb
        .transactor_fields
        .iter()
        .find(|(n, _)| n == instance)
        .map(|(_, t)| *t)
        .or_else(|| {
            tb.target_tlm_actors
                .iter()
                .find(|a| a.instance == *instance)
                .map(|a| a.transactor)
        })
        .or_else(|| {
            tb.unbound_state_actors
                .iter()
                .find(|(n, _)| n == instance)
                .map(|(_, t)| *t)
        })?;
    prog.transactors.get(tid.index())
}

/// The component schema a `ComponentField` read resolves against.
/// `SelfField` finds the component owning the method/handler body being
/// emitted (by function id); `Local` reads the component-typed param
/// local; `Path` roots at the owning testbench's component field and
/// descends `Sub` fields.
fn component_of_base<'a>(
    cx: &ECx<'a>,
    base: &crate::ir::ComponentBase,
) -> Option<&'a crate::ir::ComponentSchema> {
    let prog = cx.prog?;
    match base {
        crate::ir::ComponentBase::SelfField => prog.components.iter().find(|c| {
            c.methods.iter().any(|m| m.function == cx.func.id)
                || c.on_handlers.iter().any(|h| h.function == cx.func.id)
                || c.periodic_handlers.iter().any(|h| h.function == cx.func.id)
                || c.cycle_handlers.iter().any(|h| h.function == cx.func.id)
                || c.watchdog
                    .as_ref()
                    .is_some_and(|w| w.function == cx.func.id)
        }),
        crate::ir::ComponentBase::Local(l) => {
            let crate::ir::IrType::Component(cid) = cx.func.locals.get(l.index())?.ty else {
                return None;
            };
            prog.components.get(cid.index())
        }
        crate::ir::ComponentBase::Path(path) => {
            let (first, rest) = path.split_first()?;
            let test_root = owner_tb(cx).and_then(|tb| {
                tb.component_fields
                    .iter()
                    .find(|b| b.field == *first)
                    .and_then(|root| prog.components.get(root.component.index()))
            });
            let mut c = if let Some(root) = test_root {
                root
            } else if first == "self" {
                prog.components.iter().find(|c| {
                    c.methods.iter().any(|m| m.function == cx.func.id)
                        || c.on_handlers.iter().any(|h| h.function == cx.func.id)
                        || c.periodic_handlers.iter().any(|h| h.function == cx.func.id)
                        || c.cycle_handlers.iter().any(|h| h.function == cx.func.id)
                        || c.watchdog.as_ref().is_some_and(|w| w.function == cx.func.id)
                })?
            } else {
                return None;
            };
            for seg in rest {
                let f = c.fields.iter().find(|f| f.name == *seg)?;
                let crate::ir::ComponentFieldKind::Sub { component, .. } = f.kind else {
                    return None;
                };
                c = prog.components.get(component.index())?;
            }
            Some(c)
        }
    }
}

fn ir_type_width(ty: &crate::ir::IrType) -> Option<u32> {
    match ty {
        crate::ir::IrType::UInt(w) | crate::ir::IrType::SInt(w) => *w,
        crate::ir::IrType::Bool => Some(1),
        _ => None,
    }
}

fn signed_literal_cpp(value: u64) -> String {
    let value = value as i64;
    if value == i64::MIN {
        "((int64_t)(-9223372036854775807LL - 1))".to_string()
    } else {
        format!("((int64_t)({value}))")
    }
}

/// Render a composite-component access receiver. `SelfField` → `self`
/// (the method lambda's first parameter); `Path` → the dot-joined
/// test-scope path (`env.source`), all by-value struct members.
pub(super) fn comp_base_cpp(base: &crate::ir::ComponentBase) -> String {
    comp_base_cpp_subst(base, None)
}

/// As `comp_base_cpp`, but `self_subst = Some(inst)` renders a
/// `SelfField` base as the instance path `inst` instead of `self` — for
/// clause exprs lowered self-relatively but emitted in a `_checkers`
/// closure that has no `self` (watchdog/periodic period + max_idle).
///
/// A `ComponentBase::Local` only ever arises inside a method body (a
/// component-typed parameter receiver), where the cx-aware
/// [`comp_base_cpp_subst_cx`] is used to render it via the local-name
/// table. This name-less variant therefore never sees a `Local` and
/// renders it to a deliberately-invalid sentinel rather than threading a
/// names table everywhere — the call paths that can produce a `Local`
/// base all route through the cx-aware variant.
pub(super) fn comp_base_cpp_subst(
    base: &crate::ir::ComponentBase,
    self_subst: Option<&str>,
) -> String {
    match base {
        crate::ir::ComponentBase::SelfField => self_subst.unwrap_or("self").to_string(),
        // A `self`-rooted path (`self.sb`) is a self-relative sub-component
        // access lowered inside a component/handler body — re-root the
        // `self` head at the running instance via `self_subst` (the
        // periodic/cycle-handler poke form), exactly as `ScoreboardOp` does
        // for a `self`-rooted scoreboard path.
        crate::ir::ComponentBase::Path(path)
            if path.first().map(String::as_str) == Some("self") =>
        {
            let root = self_subst.unwrap_or("self");
            std::iter::once(root.to_string())
                .chain(path.iter().skip(1).cloned())
                .collect::<Vec<_>>()
                .join(".")
        }
        crate::ir::ComponentBase::Path(path) => path.join("."),
        // Unreachable: a `Local` base requires the local-name table; every
        // emission site that can produce one uses `comp_base_cpp_subst_cx`.
        crate::ir::ComponentBase::Local(l) => {
            format!("/*BUG:component-local l{} without names*/", l.0)
        }
    }
}

/// cx-aware component-base resolver: handles `ComponentBase::Local` by
/// rendering the parameter local's C++ name, and delegates every other
/// base shape to the name-less [`comp_base_cpp_subst`] (carrying the
/// `self_subst` from the emission context).
pub(super) fn comp_base_cpp_subst_cx(cx: &ECx<'_>, base: &crate::ir::ComponentBase) -> String {
    match base {
        crate::ir::ComponentBase::Local(l) => cx
            .names
            .get(l.index())
            .cloned()
            .unwrap_or_else(|| format!("/*BUG:l{}*/", l.0)),
        other => comp_base_cpp_subst(other, cx.self_subst),
    }
}

/// Render a record-field access chain as a C++ member path:
/// `base.field[.path…]`, inserting a `[idx]` element selection after any
/// segment carrying a mid-chain `Vec<Record, N>` index and after the leaf
/// when `index` is `Some` (a `Vec` element read/write). Shared by the
/// `Expr::RecordField` read and `Stmt::RecordFieldWrite` store emission,
/// so both sides render one chain shape (`tbl.entries[i].tag`).
pub(super) fn record_access_cpp(
    cx: &ECx<'_>,
    base: &str,
    field: &str,
    path: &[String],
    mid_indices: &[(usize, crate::ir::Expr)],
    index: Option<&crate::ir::Expr>,
) -> Result<String, EmitError> {
    let mut s = format!("{base}.{field}");
    for (pos, seg) in std::iter::once(None)
        .chain(path.iter().map(Some))
        .enumerate()
    {
        if let Some(seg) = seg {
            s.push('.');
            s.push_str(seg);
        }
        for (_, idx) in mid_indices.iter().filter(|(p, _)| *p == pos) {
            let i = expr_cpp(cx, idx)?;
            s.push('[');
            s.push_str(&i);
            s.push(']');
        }
    }
    if let Some(idx) = index {
        let i = expr_cpp(cx, idx)?;
        s.push('[');
        s.push_str(&i);
        s.push(']');
    }
    Ok(s)
}
