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
#[derive(Clone, Copy)]
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
    /// Widths of the current concurrent check's temporal latch slots.
    /// Empty outside property/cover emission.
    pub temporal_widths: &'a [Option<u32>],
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

/// Resolve the C++ receiver for a transactor-state access. A non-empty source
/// instance (`a.calls`) maps through the owning testbench's explicit storage
/// table when collision hygiene renamed its object. An EMPTY instance is a
/// type-shared method-body placeholder (#494 P1b) and renders against the
/// per-call state receiver (`self_state`); reaching an empty instance with no
/// receiver in scope means a lowering/pass bug left a placeholder unbound.
pub(super) fn resolve_state_instance(cx: &ECx<'_>, instance: &str) -> Result<String, EmitError> {
    // A concrete emission context is authoritative. Shared target-responder
    // bodies are rendered once per actor; even malformed/pass-mutated IR that
    // retained a stale non-empty name must not alias another actor's state.
    if let Some(receiver) = cx.state_receiver {
        return Ok(receiver.to_string());
    }
    if !instance.is_empty() {
        if let Some(storage) = owner_tb(cx).and_then(|tb| {
            tb.unbound_state_actors
                .iter()
                .find(|actor| actor.field == instance)
                .map(|actor| actor.storage.clone())
        }) {
            return Ok(storage);
        }
        return Ok(instance.to_string());
    }
    Err(EmitError(format!(
        "tbir: empty-instance transactor-state access in {} with no state receiver \
             in scope (unfilled placeholder — lowering/pass bug)",
        cx.func.name
    )))
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
        // Fixed-vector testbench field element read — `_tb.mem[i]` (and
        // `_tb.mem[i][j]` for a nested `Vec<Vec<..>>`). Mirrors v1's
        // `_tb.<field>[i]` subscript on the `std::array` member.
        Expr::TbFieldVecElement {
            field,
            index,
            inner_index,
        } => {
            let mut member = format!("_tb.{field}[{}]", expr_cpp(cx, index)?);
            if let Some(inner) = inner_index {
                member = format!("{member}[{}]", expr_cpp(cx, inner)?);
            }
            member
        }
        // Latch readings render against the per-closure cells the
        // concurrent-check emitter declares (`func::emit_property_check`
        // / `emit_cover_check`). `_harc_ps<i>` is the `static` previous
        // value, `_harc_cur<i>` this cycle's — both scoped to the one
        // `_checkers` closure, so plain indexed names cannot collide
        // across checks the way v1's span-tagged names guard against.
        Expr::TemporalSlot { slot, kind } => match kind {
            crate::ir::TemporalFn::Past => format!("_harc_ps{slot}"),
            crate::ir::TemporalFn::Rose
                if cx
                    .temporal_widths
                    .get(*slot as usize)
                    .copied()
                    .flatten()
                    .is_some_and(|width| width > 128) =>
            {
                format!(
                    "(harc_rt::harc_wide_is_zero(_harc_ps{slot}) && !harc_rt::harc_wide_is_zero(_harc_cur{slot}))"
                )
            }
            crate::ir::TemporalFn::Fell
                if cx
                    .temporal_widths
                    .get(*slot as usize)
                    .copied()
                    .flatten()
                    .is_some_and(|width| width > 128) =>
            {
                format!(
                    "(!harc_rt::harc_wide_is_zero(_harc_ps{slot}) && harc_rt::harc_wide_is_zero(_harc_cur{slot}))"
                )
            }
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
            mid_indices,
            index,
        } => {
            let recv = format!("{}.{field}", resolve_state_instance(cx, instance)?);
            if path.is_empty() {
                let Some(index) = index.as_deref() else {
                    return Err(EmitError(format!(
                        "tbir: fixed-vector state access `{field}` lacks an index"
                    )));
                };
                let mut access = recv;
                for (_, mid) in mid_indices {
                    access.push_str(&format!("[{}]", expr_cpp(cx, mid)?));
                }
                format!("{access}[{}]", expr_cpp(cx, index)?)
            } else {
                record_access_cpp(
                    cx,
                    &recv,
                    &path[0],
                    &path[1..],
                    mid_indices,
                    index.as_deref(),
                )?
            }
        }
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
            let a_width = shift_lhs_width(cx, a);
            // A SIGNED wide ordered-comparison / division / modulo must
            // use the two's-complement helpers, not the carriers' native
            // operators (which are unsigned — `HarcWide<N>` and
            // `_harc_u128` both answer by magnitude). The runtime already
            // has them; this routes to them. Gated on BOTH operands
            // signed: a signed-vs-unsigned mix is a type error upstream,
            // and the mixed case is not this path's to guess at.
            let b_signed = expr_is_signed(cx, b);
            let signed_wide_cmp_or_div = a_signed
                && b_signed
                && matches!(
                    op,
                    BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Div | BinOp::Mod
                )
                && expr_binary_operand_width(cx, a)
                    .max(expr_binary_operand_width(cx, b))
                    .is_some_and(|w| w > 64);
            if signed_wide_cmp_or_div {
                return Ok(signed_wide_binary_cpp(cx, *op, a, b)?);
            }
            // Width-aware `==`/`!=` for WIDE operands. Two values are
            // equal iff their low-`width` bits are — the bits above the
            // declared width are padding, not value. The carriers do not
            // agree on that padding (a `harc_wide_sdiv` result masks it
            // to zero; the converting constructor `HarcWide<N>(-4)`
            // sign-extends it to ones), so a plain `operator==` over all
            // N words answered `q != -4` for a `q` that equals `-4`
            // (harc#657). Masking both sides to the width makes the
            // padding irrelevant, which is why signed wide storage does
            // not also need normalizing at every store site. The
            // >128-bit LITERAL case is already handled above by
            // `wide_eq_cpp`.
            // Width-aware `==`/`!=` ONLY when a genuinely signed-typed
            // wide operand is involved. A signed value's padding above
            // the declared width is inconsistent between producers — a
            // `harc_wide_sdiv` result masks it to zero, the converting
            // constructor `HarcWide<N>(-4)` sign-extends it to ones — so
            // a raw `operator==` over all carrier words answered
            // `q != -4` for a `q` that equals `-4` (harc#657). Masking
            // both sides to the width makes the padding irrelevant.
            //
            // UNSIGNED wide equality is left exactly as it was: unsigned
            // stores truncate and unsigned ops mask, so no producer
            // leaves stray high bits, and the carriers' own `==`
            // (word-wise, zero-extending the narrower) is already
            // correct — masking it would only churn output. A widthless
            // literal is not "signed-typed" here even though it promotes
            // as signed, so `u == 1` on an unsigned `u` keeps its form.
            let sint_typed = |e: &Expr| !matches!(e, Expr::Literal { .. }) && expr_is_signed(cx, e);
            if matches!(op, BinOp::Eq | BinOp::Ne) && (sint_typed(a) || sint_typed(b)) {
                let aw = expr_binary_operand_width(cx, a);
                let bw = expr_binary_operand_width(cx, b);
                if let Some(width) = aw.max(bw).filter(|w| *w > 64) {
                    // Each operand is brought to the common width on its
                    // own carrier — sign-extended if signed, zero-extended
                    // if not — which also masks off the padding above the
                    // width. A bare literal (`q == 4`) is coerced the same
                    // way, so no helper ever sees a raw `int`.
                    let a_cpp = wide_operand_to_width(cx, a, aw, width)?;
                    let b_cpp = wide_operand_to_width(cx, b, bw, width)?;
                    let eq = format!("({a_cpp} == {b_cpp})");
                    return Ok(if matches!(op, BinOp::Ne) {
                        format!("(!{eq})")
                    } else {
                        eq
                    });
                }
            }
            let mut a_cpp = expr_cpp(cx, a)?;
            let mut b_cpp = expr_cpp(cx, b)?;

            // `HarcWide<N>` deliberately converts to both native scalar
            // carriers for reads, which makes a mixed expression such as
            // `wide + 1` or `wide & 0xFF` ambiguous to C++. For unsigned
            // arithmetic/bitwise operators, promote both operands to the
            // widest word carrier before spelling the operator. This also
            // widens differently-sized HarcWide operands to one ABI.
            if matches!(
                op,
                BinOp::Add
                    | BinOp::Sub
                    | BinOp::Mul
                    | BinOp::Div
                    | BinOp::Mod
                    | BinOp::Eq
                    | BinOp::Ne
                    | BinOp::Lt
                    | BinOp::Le
                    | BinOp::Gt
                    | BinOp::Ge
                    | BinOp::BitAnd
                    | BinOp::BitOr
                    | BinOp::BitXor
            ) {
                (a_cpp, b_cpp) = coerce_unsigned_wide_pair(cx, a, b, a_cpp, b_cpp);
            }
            let shift_count = shift_count_cpp(cx, b, &b_cpp, a_width.unwrap_or(64));
            let wide_shift_count = expr_static_width(cx, b).is_some_and(|width| width > 64);
            match op {
                BinOp::And => format!(
                    "({} && {})",
                    truthy_cpp(cx, a, a_cpp),
                    truthy_cpp(cx, b, b_cpp)
                ),
                BinOp::Or => format!(
                    "({} || {})",
                    truthy_cpp(cx, a, a_cpp),
                    truthy_cpp(cx, b, b_cpp)
                ),
                BinOp::Shl if a_width.is_some_and(|w| w > 128) => {
                    let width = a_width.unwrap();
                    format!("harc_rt::harc_wide_mask_bits((({a_cpp}) << ({shift_count})), {width})")
                }
                BinOp::Shl if a_width.is_some_and(|w| w > 64) => {
                    let width = a_width.unwrap();
                    format!("harc_rt::harc_shl_u128((_harc_u128)({a_cpp}), (uint64_t)({shift_count}), {width})")
                }
                BinOp::Shl if wide_shift_count => {
                    let width = a_width.unwrap_or(64);
                    format!(
                        "((uint64_t)harc_rt::harc_shl_u128((_harc_u128)((uint64_t)({a_cpp})), {shift_count}, {width}))"
                    )
                }
                BinOp::Shl => format!("(((uint64_t)({a_cpp})) << {shift_count})"),
                BinOp::Shr if a_width.is_some_and(|w| w > 128) && a_signed => {
                    let width = a_width.unwrap();
                    format!("harc_rt::harc_wide_ashr(({a_cpp}), {shift_count}, {width})")
                }
                BinOp::Shr if a_width.is_some_and(|w| w > 128) => {
                    let width = a_width.unwrap();
                    format!("harc_rt::harc_wide_mask_bits((({a_cpp}) >> ({shift_count})), {width})")
                }
                BinOp::Shr if a_width.is_some_and(|w| w > 64) && a_signed => {
                    let width = a_width.unwrap();
                    format!(
                        "harc_rt::harc_ashr_u128((_harc_u128)({a_cpp}), (uint64_t)({shift_count}), {width})"
                    )
                }
                BinOp::Shr if a_width.is_some_and(|w| w > 64) => {
                    let width = a_width.unwrap();
                    format!("harc_rt::harc_shr_u128((_harc_u128)({a_cpp}), (uint64_t)({shift_count}), {width})")
                }
                BinOp::Shr if a_signed && wide_shift_count => {
                    let width = a_width.unwrap_or(64);
                    format!(
                        "((int64_t)harc_rt::harc_ashr_u128((_harc_u128)((uint64_t)({a_cpp})), {shift_count}, {width}))"
                    )
                }
                BinOp::Shr if wide_shift_count => {
                    let width = a_width.unwrap_or(64);
                    format!(
                        "((uint64_t)harc_rt::harc_shr_u128((_harc_u128)((uint64_t)({a_cpp})), {shift_count}, {width}))"
                    )
                }
                BinOp::Shr if a_signed => {
                    format!("(((int64_t)({a_cpp})) >> {shift_count})")
                }
                BinOp::Shr => format!("(((uint64_t)({a_cpp})) >> {shift_count})"),
                _ => format!("({a_cpp} {} {b_cpp})", bin_op_cpp(*op)),
            }
        }
        Expr::Unary(op, a) => {
            let width = expr_static_width(cx, a);
            let a = expr_cpp(cx, a)?;
            match op {
                UnOp::BitNot => bit_not_cpp(&a, width),
                UnOp::BitNotHost => format!("~({a})"),
                UnOp::Neg if width.is_some_and(|w| w > 128) => {
                    let width = width.unwrap();
                    format!("harc_rt::harc_wide_negate({a}, {width})")
                }
                UnOp::Not if width.is_some_and(|w| w > 128) => {
                    format!("harc_rt::harc_wide_is_zero({a})")
                }
                _ => format!("{}({a})", un_op_cpp(*op)),
            }
        }
        Expr::Ternary(c, t, e2) => {
            // v1 wraps the whole conditional in parens so it cannot
            // bind into a surrounding higher-precedence operator.
            let c = truthy_cpp(cx, c, expr_cpp(cx, c)?);
            let t_cpp = expr_cpp(cx, t)?;
            let e2_cpp = expr_cpp(cx, e2)?;
            let (t_cpp, e2_cpp) = coerce_unsigned_wide_pair(cx, t, e2, t_cpp, e2_cpp);
            format!("({c} ? {t_cpp} : {e2_cpp})")
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
        Expr::PortSnapshotLane {
            snapshot,
            port,
            index,
        } => {
            let sampled = &cx.names[snapshot.index()];
            let idx = expr_cpp(cx, index)?;
            match lane_width(cx, port) {
                Some(w) => {
                    format!("harc_rt::harc_vec_lane_read<{w}>({sampled}, (std::size_t)({idx}))")
                }
                None => format!("{sampled}[(std::size_t)({idx})]"),
            }
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
        Expr::ComponentVecElement {
            base,
            field,
            index_pos,
            index,
            inner_index,
        } => {
            let index = expr_cpp(cx, index)?;
            let mut member = indexed_member_cpp(field, *index_pos, &index);
            // Nested `v[i][j]`: append the inner subscript after the
            // outer member, matching v1's `self.v[i][j]`.
            if let Some(inner) = inner_index {
                member = format!("{member}[{}]", expr_cpp(cx, inner)?);
            }
            format!("{}.{}", comp_base_cpp_subst_cx(cx, base), member)
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
        Expr::DynamicListQuery { target, query } => {
            let recv = expr_cpp(cx, target)?;
            match query {
                crate::ir::DynamicListQuery::Size => {
                    format!("((uint64_t)({recv}).size())")
                }
                crate::ir::DynamicListQuery::Empty => format!("({recv}).empty()"),
            }
        }
        // Heartbeat-idle predicate on a component instance — mirrors v1's
        // `emit_idle_predicate`: compares `cycle_count` minus the
        // `_last_in_cycle`/`_last_out_cycle` stamp against the threshold.
        Expr::ComponentIdle {
            base,
            subpath,
            kind,
            n,
        } => {
            let mut recv = comp_base_cpp_subst_cx(cx, base);
            if !subpath.is_empty() {
                recv.push('.');
                recv.push_str(&subpath.join("."));
            }
            let n = bounded_count_expr_cpp(cx, n, u64::MAX)?;
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
        Expr::TransactorIdle {
            storage, kind, n, ..
        } => {
            let n = bounded_count_expr_cpp(cx, n, u64::MAX)?;
            match kind {
                crate::ir::IdleKind::In => {
                    format!(
                        "(((uint64_t)cycle_count - {storage}._last_in_cycle) >= (uint64_t)({n}))"
                    )
                }
                crate::ir::IdleKind::Out => format!(
                    "(((uint64_t)cycle_count - {storage}._last_out_cycle) >= (uint64_t)({n}))"
                ),
                crate::ir::IdleKind::Both => format!(
                    "((((uint64_t)cycle_count - {storage}._last_in_cycle) >= (uint64_t)({n})) \
                     && (((uint64_t)cycle_count - {storage}._last_out_cycle) >= (uint64_t)({n})))"
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
                CallTarget::Helper { name, .. } => helper_cpp_name(name),
                // Extern reference functions emit with the RAW symbol
                // name (no `harc_helper_` mangling) so the call binds to
                // the user's `extern "C"` definition supplied via
                // `--ref-src`; the forward decl is emitted file-scope.
                CallTarget::ExternFn { name, .. } => name.clone(),
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
                    let prog = cx.prog.ok_or_else(|| {
                        EmitError(format!(
                            "tbir: transactor call edge `{bus_field}.{method}` has no program \
                             context"
                        ))
                    })?;
                    let (schema, state_storage) = cx
                        .func
                        .owner
                        .and_then(|owner| prog.testbenches.get(owner.index()))
                        .and_then(|tb| {
                            tb.transactor_fields
                                .iter()
                                .find(|(field, _)| field == bus_field)
                                .map(|(_, xid)| {
                                    let storage = tb
                                        .unbound_state_actors
                                        .iter()
                                        .find(|actor| actor.field == *bus_field)
                                        .map(|actor| actor.storage.clone());
                                    (prog.transactor(*xid), storage)
                                })
                        })
                        .ok_or_else(|| {
                            EmitError(format!(
                                "tbir: expression-valued transactor call \
                                 `{bus_field}.{method}` does not resolve through the owner \
                                 testbench"
                            ))
                        })?;
                    let mut rendered = Vec::with_capacity(args.len() + 1);
                    if super::func::uses_state_receiver(schema) {
                        rendered.push(state_storage.ok_or_else(|| {
                            EmitError(format!(
                                "tbir: stateful expression-valued transactor call \
                                 `{bus_field}.{method}` has no receiver storage"
                            ))
                        })?);
                    }
                    for arg in args {
                        rendered.push(expr_cpp(cx, arg)?);
                    }
                    return Ok(format!(
                        "{}_{method}({})",
                        schema.name,
                        rendered.join(", ")
                    ));
                }
                CallTarget::TransactorSelfMethod { transactor, method } => {
                    let mut rendered = Vec::with_capacity(args.len() + 1);
                    if let Some(receiver) = cx.state_receiver {
                        rendered.push(receiver.to_string());
                    }
                    for arg in args {
                        rendered.push(expr_cpp(cx, arg)?);
                    }
                    return Ok(format!("{transactor}_{method}({})", rendered.join(", ")));
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
pub(super) fn wide_literal_cpp(words: &[u32]) -> String {
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

fn wide_operand_cpp(
    value: String,
    source_width: Option<u32>,
    source_signed: bool,
    target_width: u32,
) -> String {
    let words = target_width.div_ceil(32);
    match source_width {
        Some(width) if width == target_width => value,
        Some(width) if source_signed => {
            format!("harc_rt::harc_wide_sext<{words}>({value}, {width}, {target_width})")
        }
        Some(width) => format!("harc_rt::harc_wide_zext<{words}>({value}, {width})"),
        None if source_signed => {
            format!("harc_rt::harc_wide_sext<{words}>({value}, 64, {target_width})")
        }
        None => format!("harc_rt::harc_wide_zext<{words}>({value})"),
    }
}

/// A signed wide ordered-comparison (`< <= > >=`) or `/` `%`, routed to
/// the two's-complement runtime helpers.
///
/// The carriers are unsigned — `HarcWide<N>` and `_harc_u128` define the
/// six by magnitude, so the plain operator answers `-1 > 0` — but the
/// runtime already carries `harc_wide_slt`/`sdiv`/`smod` and their
/// `_u128` twins (proven by `wide_cast_cpp.rs`). Only `slt`/`sdiv`/`smod`
/// exist; the other three comparisons derive from `slt`:
///   `a <= b` == `!(b < a)`, `a > b` == `b < a`, `a >= b` == `!(a < b)`.
///
/// Both operands are sign-extended to their common width first — the
/// same normalization the unsigned pair does with zero-extension —
/// because the helpers compare at one declared `width`. Same-width
/// operands (the common case) sign-extend to themselves, i.e. unchanged.
fn signed_wide_binary_cpp(
    cx: &ECx<'_>,
    op: BinOp,
    a: &Expr,
    b: &Expr,
) -> Result<String, EmitError> {
    let aw = expr_binary_operand_width(cx, a);
    let bw = expr_binary_operand_width(cx, b);
    let width = aw.max(bw).expect("caller gated on a wide operand width");
    let a_cpp = wide_operand_to_width(cx, a, aw, width)?;
    let b_cpp = wide_operand_to_width(cx, b, bw, width)?;
    // `harc_wide_*` for a `HarcWide<N>` carrier (>128), the `_u128`
    // twin for the 65..=128 tier.
    let (lt, div, rem) = if width > 128 {
        (
            format!("harc_rt::harc_wide_slt({a_cpp}, {b_cpp}, {width})"),
            format!("harc_rt::harc_wide_sdiv({a_cpp}, {b_cpp}, {width})"),
            format!("harc_rt::harc_wide_smod({a_cpp}, {b_cpp}, {width})"),
        )
    } else {
        let a_u = format!("(_harc_u128)({a_cpp})");
        let b_u = format!("(_harc_u128)({b_cpp})");
        (
            format!("harc_rt::harc_slt_u128({a_u}, {b_u}, {width})"),
            format!("harc_rt::harc_sdiv_u128({a_u}, {b_u}, {width})"),
            format!("harc_rt::harc_smod_u128({a_u}, {b_u}, {width})"),
        )
    };
    // `lt(x, y)` is `x < y`; the reversed and negated spellings give the
    // other three comparisons.
    let lt_of = |x: &str, y: &str| {
        if width > 128 {
            format!("harc_rt::harc_wide_slt({x}, {y}, {width})")
        } else {
            format!("harc_rt::harc_slt_u128((_harc_u128)({x}), (_harc_u128)({y}), {width})")
        }
    };
    Ok(match op {
        BinOp::Lt => lt,
        BinOp::Gt => lt_of(&b_cpp, &a_cpp),
        BinOp::Le => format!("(!{})", lt_of(&b_cpp, &a_cpp)),
        BinOp::Ge => format!("(!{})", lt_of(&a_cpp, &b_cpp)),
        BinOp::Div => div,
        BinOp::Mod => rem,
        _ => unreachable!("signed_wide_binary_cpp called for a non-signed-wide operator"),
    })
}

/// Sign-extend one operand of a signed wide comparison/division to the
/// pair's common width. Same width → unchanged; narrower → `sext` on the
/// carrier tier the common width selects.
/// Bring one operand of a signed wide comparison / division / width-aware
/// equality to the pair's common width, on the carrier that width
/// selects — sign-extending a genuinely signed operand, zero-extending
/// otherwise. Both spellings mask off the bits above the width, so the
/// results compare and divide by value.
///
/// The sign decision is NOT `expr_is_signed` alone: that reports a
/// widthless POSITIVE literal as signed (it promotes as `int64_t` in C++
/// arithmetic), but such a literal's minimal width puts a 1 in its top
/// bit — `4` is `0b100`, width 3 — so sign-extending it from that width
/// would read it as negative and turn `q == 4` into `q == -4`. A bare
/// non-negative literal, and a logical-not (always 0/1), zero-extend.
/// The same carve-out `scalar_assignment_expr_cpp` makes for the store
/// side.
fn wide_operand_to_width(
    cx: &ECx<'_>,
    e: &Expr,
    source_width: Option<u32>,
    target_width: u32,
) -> Result<String, EmitError> {
    let cpp = expr_cpp(cx, e)?;
    let signed = match e {
        Expr::Literal {
            ty: crate::ir::IrType::Unknown,
            ..
        }
        | Expr::Unary(UnOp::Not, _) => false,
        _ => expr_is_signed(cx, e),
    };
    Ok(if signed {
        signed_wide_operand_cpp(cpp, source_width, target_width)
    } else {
        unsigned_wide_operand_cpp(cpp, source_width, target_width)
    })
}

/// Zero-extend one operand of a width-aware `==`/`!=` to the pair's
/// common width, on the carrier that width selects. The mirror of
/// `signed_wide_operand_cpp` for an unsigned operand — both mask off the
/// bits above the width, so the resulting carriers compare by value.
fn unsigned_wide_operand_cpp(
    value: String,
    source_width: Option<u32>,
    target_width: u32,
) -> String {
    if target_width > 128 {
        let words = target_width.div_ceil(32);
        return match source_width {
            Some(w) if w == target_width => {
                format!("harc_rt::harc_wide_mask_bits({value}, {target_width})")
            }
            Some(w) => format!("harc_rt::harc_wide_zext<{words}>({value}, {w})"),
            None => format!("harc_rt::harc_wide_zext<{words}>({value})"),
        };
    }
    format!("(((_harc_u128)({value})) & harc_rt::harc_mask_u128({target_width}))")
}

fn signed_wide_operand_cpp(value: String, source_width: Option<u32>, target_width: u32) -> String {
    match source_width {
        // Same width still MASKS to the width, not a pass-through: the
        // operand may carry unmasked padding above the width (an
        // unmasked `0 - q` subtraction, or the full-carrier sign
        // extension of the converting constructor), and a raw
        // `operator==` over all carrier words would see it. `slt`/`sdiv`
        // mask internally so this is only redundant, never wrong, on
        // that path.
        Some(w) if w == target_width && target_width > 128 => {
            format!("harc_rt::harc_wide_mask_bits({value}, {target_width})")
        }
        Some(w) if w == target_width => {
            format!("(((_harc_u128)({value})) & harc_rt::harc_mask_u128({target_width}))")
        }
        Some(w) if target_width > 128 => {
            let words = target_width.div_ceil(32);
            format!("harc_rt::harc_wide_sext<{words}>({value}, {w}, {target_width})")
        }
        Some(w) => format!("harc_rt::harc_sext_u128((_harc_u128)({value}), {w}, {target_width})"),
        None if target_width > 128 => {
            let words = target_width.div_ceil(32);
            format!("harc_rt::harc_wide_sext<{words}>({value}, 64, {target_width})")
        }
        None => format!("harc_rt::harc_sext_u128((_harc_u128)({value}), 64, {target_width})"),
    }
}

fn coerce_unsigned_wide_pair(
    cx: &ECx<'_>,
    lhs: &Expr,
    rhs: &Expr,
    lhs_cpp: String,
    rhs_cpp: String,
) -> (String, String) {
    let lhs_width = expr_binary_operand_width(cx, lhs);
    let rhs_width = expr_binary_operand_width(cx, rhs);
    let Some(target_width) = lhs_width.max(rhs_width).filter(|width| *width > 128) else {
        return (lhs_cpp, rhs_cpp);
    };
    let lhs_signed = expr_is_signed(cx, lhs);
    let rhs_signed = expr_is_signed(cx, rhs);
    let target_is_unsigned = lhs_width.is_some_and(|w| w == target_width && !lhs_signed)
        || rhs_width.is_some_and(|w| w == target_width && !rhs_signed);
    if !target_is_unsigned {
        return (lhs_cpp, rhs_cpp);
    }

    let needs_signed_extension = |expr: &Expr, signed: bool| {
        signed
            && !matches!(
                expr,
                Expr::Literal {
                    ty: crate::ir::IrType::Unknown,
                    ..
                }
            )
    };
    (
        wide_operand_cpp(
            lhs_cpp,
            lhs_width,
            needs_signed_extension(lhs, lhs_signed),
            target_width,
        ),
        wide_operand_cpp(
            rhs_cpp,
            rhs_width,
            needs_signed_extension(rhs, rhs_signed),
            target_width,
        ),
    )
}

fn truthy_cpp(cx: &ECx<'_>, expr: &Expr, rendered: String) -> String {
    if expr_static_width(cx, expr).is_some_and(|width| width > 128) {
        format!("!harc_rt::harc_wide_is_zero({rendered})")
    } else {
        rendered
    }
}

pub(super) fn truthy_expr_cpp(cx: &ECx<'_>, expr: &Expr) -> Result<String, EmitError> {
    Ok(truthy_cpp(cx, expr, expr_cpp(cx, expr)?))
}

/// Render an unsigned scalar for a host timing/count API. Wide carriers
/// are narrowed explicitly and saturate at the consumer's maximum instead
/// of relying on ambiguous C++ conversion operators or wrapping casts.
pub(super) fn bounded_count_expr_cpp(
    cx: &ECx<'_>,
    expr: &Expr,
    limit: u64,
) -> Result<String, EmitError> {
    let rendered = expr_cpp(cx, expr)?;
    Ok(match expr_static_width(cx, expr) {
        Some(width) if width > 128 => {
            format!("harc_rt::harc_wide_shift_count({rendered}, {limit}ULL)")
        }
        Some(width) if width > 64 => {
            format!("harc_rt::harc_u128_shift_count((_harc_u128)({rendered}), {limit}ULL)")
        }
        _ => rendered,
    })
}

/// Render a value into an explicitly declared scalar destination. This is
/// primarily needed for >128-bit unsigned storage: C++ construction from a
/// negative scalar zero-fills upper words, while HARC assignment uses the
/// destination width and therefore requires sign extension modulo 2^N.
pub(super) fn scalar_assignment_expr_cpp(
    cx: &ECx<'_>,
    value: &Expr,
    destination: &crate::ir::IrType,
) -> Result<String, EmitError> {
    let rendered = expr_cpp(cx, value)?;
    let crate::ir::IrType::UInt(Some(target_width)) = destination else {
        return Ok(rendered);
    };
    if *target_width <= 128 {
        return Ok(rendered);
    }
    let source_signed = match value {
        // Widthless positive literals are contextual unsigned values for an
        // unsigned destination. `expr_is_signed` classifies them as signed
        // for C++ arithmetic promotion, which is a different question.
        Expr::Literal {
            ty: crate::ir::IrType::Unknown,
            ..
        }
        | Expr::Unary(UnOp::Not, _) => false,
        _ => expr_is_signed(cx, value),
    };
    let coerced = wide_operand_cpp(
        rendered,
        expr_binary_operand_width(cx, value),
        source_signed,
        *target_width,
    );
    Ok(format!(
        "harc_rt::harc_wide_mask_bits({coerced}, {target_width})"
    ))
}

fn expr_binary_operand_width(cx: &ECx<'_>, value: &Expr) -> Option<u32> {
    if let Expr::Literal { value, ty } = value {
        return match ty {
            crate::ir::IrType::Unknown => Some((64 - value.leading_zeros()).max(1)),
            crate::ir::IrType::Bool => Some(1),
            crate::ir::IrType::UInt(Some(width)) | crate::ir::IrType::SInt(Some(width)) => {
                Some(*width)
            }
            crate::ir::IrType::UInt(None) | crate::ir::IrType::SInt(None) => Some(64),
            _ => None,
        };
    }
    expr_static_width(cx, value).max(expr_shift_width(cx, value))
}

fn shift_lhs_width(cx: &ECx<'_>, lhs: &Expr) -> Option<u32> {
    if let Expr::Literal { ty, .. } = lhs {
        // A widthless literal uses the scalar runtime carrier.  Its value's
        // significant-bit width is useful for arithmetic promotion, but not
        // for shift saturation: `1 << 3` must not clamp at one bit.
        return match ty {
            crate::ir::IrType::UInt(Some(width)) | crate::ir::IrType::SInt(Some(width)) => {
                Some(*width)
            }
            crate::ir::IrType::Bool => Some(1),
            _ => Some(64),
        };
    }
    expr_shift_width(cx, lhs)
}

fn shift_count_cpp(cx: &ECx<'_>, rhs: &Expr, rendered: &str, limit: u32) -> String {
    match expr_static_width(cx, rhs) {
        Some(width) if width > 128 => {
            format!("harc_rt::harc_wide_shift_count({rendered}, {limit})")
        }
        Some(width) if width > 64 => {
            format!("harc_rt::harc_u128_shift_count((_harc_u128)({rendered}), {limit})")
        }
        _ => rendered.to_string(),
    }
}

fn un_op_cpp(op: UnOp) -> &'static str {
    match op {
        UnOp::Neg => "-",
        UnOp::Not => "!",
        UnOp::BitNot | UnOp::BitNotHost => "~",
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

pub(super) fn expr_static_width(cx: &ECx<'_>, e: &Expr) -> Option<u32> {
    match e {
        Expr::Literal { ty, .. } => ir_type_width(ty),
        Expr::WideLiteral(words) => words.len().checked_mul(32).map(|width| width as u32),
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
            ..
        } => state_transactor(cx, instance)
            .and_then(|t| t.state_fields.iter().find(|f| f.name == *field))
            .and_then(|f| match f.kind {
                crate::ir::StateFieldKind::Record { record } => {
                    record_path_type(cx, crate::ir::IrType::Record(record), path.iter())
                }
                crate::ir::StateFieldKind::FixedVec {
                    ty: crate::ir::IrType::FixedVec { ref elem, .. },
                } if path.is_empty() => Some((**elem).clone()),
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
        Expr::ComponentVecElement {
            base,
            field,
            inner_index,
            ..
        } => component_vec_elem_type(cx, base, field, inner_index.is_some())
            .as_ref()
            .and_then(ir_type_width),
        Expr::TbFieldVecElement {
            field, inner_index, ..
        } => owner_tb(cx)
            .and_then(|tb| tb.scalar_fields.iter().find(|f| f.name == *field))
            .and_then(|f| fixed_vec_ir_elem(&f.ty, inner_index.is_some()))
            .as_ref()
            .and_then(ir_type_width),
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
        Expr::TemporalSlot {
            slot,
            kind: crate::ir::TemporalFn::Past,
        } => cx.temporal_widths.get(*slot as usize).copied().flatten(),
        Expr::TemporalSlot { .. } => Some(1),
        Expr::Binary(op, a, b) => match op {
            BinOp::Eq
            | BinOp::Ne
            | BinOp::Lt
            | BinOp::Le
            | BinOp::Gt
            | BinOp::Ge
            | BinOp::And
            | BinOp::Or => Some(1),
            BinOp::Shl | BinOp::Shr => shift_lhs_width(cx, a),
            _ => expr_static_width(cx, a).max(expr_static_width(cx, b)),
        },
        Expr::Unary(UnOp::Not, _) => Some(1),
        Expr::Unary(UnOp::BitNotHost, _) => None,
        Expr::Unary(_, inner) => expr_static_width(cx, inner),
        Expr::Ternary(_, t, f) => expr_static_width(cx, t).max(expr_static_width(cx, f)),
        Expr::BitSlice { hi, lo, .. } => Some(hi - lo + 1),
        Expr::WidthCast { width, .. } => Some(*width),
        Expr::Call(CallTarget::Helper { ret, .. } | CallTarget::ExternFn { ret, .. }, _) => {
            ir_type_width(ret)
        }
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
        Expr::Binary(
            BinOp::Eq
            | BinOp::Ne
            | BinOp::Lt
            | BinOp::Le
            | BinOp::Gt
            | BinOp::Ge
            | BinOp::And
            | BinOp::Or,
            _,
            _,
        ) => Some(1),
        Expr::Binary(BinOp::BitAnd, lhs, rhs) => {
            let bound = |e: &Expr| -> Option<u32> {
                if let Expr::Literal { value, ty } = e {
                    if matches!(ty, crate::ir::IrType::Unknown) {
                        return Some((64 - value.leading_zeros()).max(1));
                    }
                }
                expr_shift_width(cx, e)
            };
            match (bound(lhs), bound(rhs)) {
                (Some(a), Some(b)) if a < b && !expr_is_signed(cx, lhs) => Some(a),
                (Some(a), Some(b)) if b < a && !expr_is_signed(cx, rhs) => Some(b),
                (Some(a), Some(b)) => Some(a.max(b)),
                (w, None) | (None, w) => w,
            }
        }
        Expr::Binary(_, lhs, rhs) => expr_shift_width(cx, lhs).max(expr_shift_width(cx, rhs)),
        Expr::Ternary(_, then_expr, else_expr) => {
            expr_shift_width(cx, then_expr).max(expr_shift_width(cx, else_expr))
        }
        Expr::Unary(UnOp::Not, _) => Some(1),
        Expr::Unary(UnOp::BitNotHost, _) => None,
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
        Expr::Call(CallTarget::Helper { ret, .. } | CallTarget::ExternFn { ret, .. }, _) => {
            matches!(ret, crate::ir::IrType::SInt(_))
        }
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
            ..
        } => state_transactor(cx, instance)
            .and_then(|t| t.state_fields.iter().find(|f| f.name == *field))
            .is_some_and(|f| match f.kind {
                crate::ir::StateFieldKind::Record { record } => {
                    record_path_is_sint(cx, crate::ir::IrType::Record(record), path.iter())
                }
                crate::ir::StateFieldKind::FixedVec {
                    ty: crate::ir::IrType::FixedVec { ref elem, .. },
                } if path.is_empty() => matches!(**elem, crate::ir::IrType::SInt(_)),
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
        Expr::ComponentVecElement {
            base,
            field,
            inner_index,
            ..
        } => matches!(
            component_vec_elem_type(cx, base, field, inner_index.is_some()),
            Some(crate::ir::IrType::SInt(_))
        ),
        Expr::TbFieldVecElement {
            field, inner_index, ..
        } => owner_tb(cx)
            .and_then(|tb| tb.scalar_fields.iter().find(|f| f.name == *field))
            .and_then(|f| fixed_vec_ir_elem(&f.ty, inner_index.is_some()))
            .is_some_and(|ty| matches!(ty, crate::ir::IrType::SInt(_))),
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
        Expr::Unary(UnOp::Not, _) => false,
        // `BitNotHost` is v1's UNMASKED host `~`; its signedness is that of
        // its operand under C++ usual-arithmetic conversion, exactly like the
        // width-masked `BitNot` below — NOT unconditionally signed. Hardcoding
        // it signed flipped a following right-shift from logical to arithmetic
        // whenever the operand was an unsigned wide/port value (e.g.
        // `(~(port & sized)) >> k`), silently diverging from v1 (harc#630
        // family). A bare sized literal operand is `Literal { ty: Unknown }`,
        // which `expr_is_signed` already scores signed, so the pure
        // `~sized` case (and `~sized < 0`) is unchanged.
        Expr::Unary(UnOp::BitNotHost, inner) => expr_is_signed(cx, inner),
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

/// Peel a `FixedVec` `IrType` to the element type a `TbFieldVecElement`
/// selects — once for `mem[i]`, twice for a nested `mem[i][j]`. Unlike
/// `component_vec_elem_type` the receiver is already a plain
/// `IrType::FixedVec` (a testbench field's `ty`), so there is no
/// component-field indirection to resolve. Getting
/// this right decides the same two things the component path does: `>>`
/// arithmetic-vs-logical and whether a >64-bit element is truncated to
/// `uint64_t` before use.
fn fixed_vec_ir_elem(ty: &crate::ir::IrType, nested: bool) -> Option<crate::ir::IrType> {
    let outer = match ty {
        crate::ir::IrType::FixedVec { elem, .. } => elem.as_ref(),
        _ => return None,
    };
    if !nested {
        return Some(outer.clone());
    }
    match outer {
        crate::ir::IrType::FixedVec { elem, .. } => Some(elem.as_ref().clone()),
        _ => None,
    }
}

/// The ELEMENT type of the `Vec` a `ComponentVecElement` / -`Write`
/// selects from, for either shape its `field` can take.
///
/// `field` is a member SUFFIX, not always one name. A component field
/// declared `Vec<T, N>` gives the single name `own`; a `Vec<T, N>` LEAF
/// inside a component RECORD field gives a dotted path (`a.data`). The
/// arms here used to do `fields.iter().find(|f| f.name == *field)`,
/// which can never match a dotted one — so every dotted element read
/// answered "width unknown, not signed". That is not a missing
/// optimisation: it decided `>>` between an arithmetic and a logical
/// shift, and whether a >64-bit element was truncated to `uint64_t`
/// before use. Both compile; both silently disagree with v1.
fn component_vec_elem_type(
    cx: &ECx<'_>,
    base: &crate::ir::ComponentBase,
    field: &str,
    nested: bool,
) -> Option<crate::ir::IrType> {
    let path: Vec<String> = field.split('.').map(str::to_string).collect();
    let root = path.first()?;
    let f = component_of_base(cx, base)?
        .fields
        .iter()
        .find(|f| f.name == *root)?;
    match &f.kind {
        crate::ir::ComponentFieldKind::FixedVec(vec) if path.len() == 1 => {
            // A nested read `v[i][j]` descends one `FixedVec` to the
            // scalar leaf so width/sign decisions see the element, not
            // the inner array.
            match (nested, &vec.elem) {
                (true, crate::ir::IrType::FixedVec { elem, .. }) => Some((**elem).clone()),
                _ => Some(vec.elem.clone()),
            }
        }
        crate::ir::ComponentFieldKind::Record { record } => {
            // The leaf's own type. `record_path_type` reports a `Vec`
            // field as its element type (`vec_len` rides beside `ty` in
            // the schema), which is exactly what an element selection
            // yields.
            record_path_type(cx, crate::ir::IrType::Record(*record), path[1..].iter())
        }
        _ => None,
    }
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
                .find(|actor| actor.field == *instance)
                .map(|actor| actor.transactor)
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
                        || c.watchdog
                            .as_ref()
                            .is_some_and(|w| w.function == cx.func.id)
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
/// A `ComponentBase::Local` arises inside a method body (a
/// component-typed parameter receiver) for method calls and predicates. The
/// cx-aware [`comp_base_cpp_subst_cx`] is used to render it via the local-name
/// table. This name-less variant therefore never sees a `Local` and renders it
/// to a deliberately-invalid sentinel rather than threading a names table
/// everywhere — the call paths that can produce a `Local` base all route
/// through the cx-aware variant.
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
/// segment carrying a path index (including repeated nested-vector indexes
/// at the leaf) and after the leaf when `index` is `Some`. Shared by the
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

pub(super) fn indexed_member_cpp(field: &str, index_pos: usize, index: &str) -> String {
    field
        .split('.')
        .enumerate()
        .map(|(pos, seg)| {
            if pos == index_pos {
                format!("{seg}[{index}]")
            } else {
                seg.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}
