//! `scoreboard` declaration lowering → `ScoreboardSchema`.
//!
//! A scoreboard in the v0 subset is a *data-only* host-state record: a
//! testbench field holding scalar counters and typed FIFO queues. v1's
//! `emit_scoreboard` is the behavior reference — it emits a C++ struct
//! whose scalar fields are `uint64_t`/`int64_t`/`bool` members (with
//! their declared defaults) and whose `queue<T>` fields are
//! `harc_rt::HarcQueue<T>` members. The test body manipulates them
//! through `Stmt::ScoreboardOp` (push/pop/scalar-write) and
//! `Expr::ScoreboardQuery` (size/empty/scalar-read).
//!
//! Out-of-scope shapes are explicit rejections, never silent drops.
//! `Unsupported` (v1 runs the program):
//!   - scoreboard `hookable`/`function` methods (need per-instance state
//!     materialization — rejected at the call site, but the declaration
//!     itself is also rejected here so an unreferenced method-bearing
//!     scoreboard does not lower to a struct missing its methods);
//!   - queue element types that are not scalars ≤ 64 bits (e.g.
//!     `queue<SomeStruct>` — needs the record-payload-in-queue seam);
//!   - non-scalar / >64-bit scalar fields.
//!
//! `NotImplemented { v1: SilentlyMisLowers }` (v1 is not a way out):
//!   - component `parameters`, which v1 drops entirely;
//!   - `connect` / `on` event wiring, which v1 drops in a transactor
//!     container. See the measurement at each arm.

use super::{unsupported, LowerError};
use crate::ast::{BuiltinTy, ComponentDecl, ComponentItem, ExprKind, TypeArg, TypeExpr};
use crate::ir::{IrType, RecordId, ScoreboardFieldKind, ScoreboardFieldSchema, ScoreboardSchema};
use std::collections::HashMap;

pub(crate) fn lower_scoreboard(
    c: &ComponentDecl,
    record_ids: &HashMap<String, RecordId>,
    consts: &HashMap<String, super::ConstVal>,
) -> Result<ScoreboardSchema, LowerError> {
    let sb = &c.name.name;
    if !c.params.is_empty() {
        // The fourth landing of the component-parameter construct, and
        // it agrees with the other three.
        //
        // A first pass labelled this arm `EmitsUncompilable` on the
        // argument that a data-only scoreboard has no emission position
        // after v1's file-scope consts — only fields, whose defaults and
        // widths are emitted inside the struct, ahead of every const.
        // The argument is wrong at its first step. `scoreboard_is_
        // component` routes to the composite table on `Hookable` ALONE,
        // so a scoreboard carrying fields plus an `on` handler stays
        // data-only and reaches here — and this check runs before the
        // `on` rejection further down. v1 emits that handler's trigger
        // into a checker lambda ~110 lines AFTER the const, so
        // `on hits > N` becomes `(bool)(_tb.b.hits > N)` resolving to a
        // file-scope `const N = 5`: it compiles and the scoreboard runs
        // with 5. `#(7)` and `#(8)` emit byte-identically, so the
        // argument is provably invisible.
        //
        // The lesson is not about scoreboards. The claim "its only items
        // are fields" was read off the arms in THIS file, which reject
        // methods and `on` handlers, without checking the gate that
        // decides which file gets the declaration at all.
        return Err(super::not_implemented(
            &format!("parameters on scoreboard `{sb}`"),
            "v1 drops the parameter list entirely: an unused parameter vanishes along \
             with any `#(...)` argument at the instantiation, and a reference to one \
             either fails to resolve or silently picks up a same-named file-scope \
             `const`, depending on where in the emitted file the reference lands",
            super::V1Status::SilentlyMisLowers,
        ));
    }
    if c.bound_to.is_some() {
        return Err(unsupported(
            &format!("a `bound to` clause on scoreboard `{sb}`"),
            "",
        ));
    }
    let mut fields: Vec<ScoreboardFieldSchema> = Vec::new();
    for item in &c.items {
        match item {
            ComponentItem::Field(f) => {
                let fname = &f.name.name;
                if f.direction.is_some() {
                    return Err(unsupported(
                        &format!("a directional (port) field `{sb}.{fname}`"),
                        "scoreboards hold host-state data, not DUT ports",
                    ));
                }
                if f.bound_to.is_some() {
                    return Err(unsupported(
                        &format!("a `bound to` clause on scoreboard field `{sb}.{fname}`"),
                        "",
                    ));
                }
                if fields.iter().any(|x| x.name == *fname) {
                    return Err(LowerError::Invalid(format!(
                        "scoreboard `{sb}` declares field `{fname}` more than once"
                    )));
                }
                let kind = scoreboard_field_kind(sb, fname, &f.ty, record_ids)?;
                let kind = match kind {
                    ScoreboardFieldKind::Scalar { ty, .. } => {
                        let default = scalar_default(&f.default, sb, fname, &f.ty, consts)?;
                        ScoreboardFieldKind::Scalar { ty, default }
                    }
                    other => {
                        if f.default.is_some() {
                            return Err(unsupported(
                                &format!("a default on scoreboard queue field `{sb}.{fname}`"),
                                "queues default-construct empty",
                            ));
                        }
                        other
                    }
                };
                fields.push(ScoreboardFieldSchema {
                    name: fname.clone(),
                    kind,
                });
            }
            ComponentItem::Hookable(h) => {
                // Scoreboard methods mutate scoreboard instance state,
                // which the v0 subset does not materialize. Reject the
                // declaration so a method-bearing scoreboard never lowers
                // to a struct missing its methods (it would mis-lower at
                // a call site otherwise).
                return Err(unsupported(
                    &format!("a method (`{}`) on scoreboard `{sb}`", h.name.name),
                    "scoreboard methods need per-instance state materialization; \
                     mutate scoreboard fields directly from the test body instead",
                ));
            }
            // A hooked `on` in a scoreboard body is the same construct as
            // the hook arms in `components.rs`, and v1 does the same
            // thing to it: drops the hook and lowers the trigger alone,
            // byte-identically to the handler written without a hook
            // side. (Anchored: deleting the handler does change v1's
            // output, at both trigger shapes below.) So the requested
            // ordering is silently lost and `--codegen v1` is not an
            // escape hatch.
            ComponentItem::OnHandler(h) if h.hook.is_some() => {
                return Err(super::not_implemented(
                    &format!("a `pre`/`post` hook on an `on` handler on scoreboard `{sb}`"),
                    "scoreboards take no method hooks; v1 accepts a hook side, drops it and \
                     lowers the trigger as a plain handler",
                    super::V1Status::SilentlyMisLowers,
                ));
            }
            // The UNHOOKED half is left whole, and that is a decision
            // rather than an omission. It is mixed — `on w.note` makes
            // v1 emit `(bool)(w.note)` against a `struct Watcher` with
            // no `note` member, while `on hits > 0` makes it emit
            // `(bool)(_tb.b.hits > 0)`, which compiles and works — but
            // what separates them is NAME RESOLUTION in the emitted C++,
            // not the syntax of the trigger. `on dut.en` is a two-
            // segment path and compiles (`harc_read(dut->en)`);
            // `on w.seen > 0` is a bool expression and does NOT (no `w`
            // in the checker lambda's scope). A syntactic split was
            // written here on `is_v1_method_hook_shape` and reverted:
            // it called `on dut.en` uncompilable and let `on (w.note)`,
            // `on w.note cycles` and `on w.note phase post_eval`
            // through, because that predicate answers the hook
            // resolver's question, not this one. Classifying this arm
            // needs the scope analysis, not another shape test.
            // MEASURED across both containers a data-only scoreboard
            // can sit in, because what v1 does depends on the container
            // as well — which is the second reason the syntactic split
            // recorded above failed.
            //
            // As a TRANSACTOR field, v1 emits output byte-identical to
            // the same program with the `connect`/`on` deleted (608
            // lines either way; the only textual difference is a source
            // OFFSET baked into `_solver_site_<N>` names and an
            // auto-coverage plan literal). It silently DROPS the
            // wiring, so the scoreboard observes nothing and a test
            // that should catch a mismatch passes green.
            //
            // As a TESTBENCH field the same three inputs diverge three
            // ways:
            //   * `connect` emits `_tb.b.hits.push_back(...)` against a
            //     scalar member — g++: "request for member 'push_back'
            //     in '_tb.Tb::b.Board::hits', which is of non-class
            //     type 'uint64_t'". Uncompilable. (`uint64_t`, not the
            //     `uint<32>` the source declares: `cpp_uint_for_width`
            //     widens every scalar ≤ 64 bits, as the module header
            //     above already says.)
            //   * `on hits > 0` emits a `_checkers` closure around
            //     `(bool)(_tb.b.hits > 0)`, which compiles and works.
            //   * `on dut.rst` emits `(bool)(harc_rt::harc_read(
            //     dut->rst))`, which also compiles and works.
            //
            // So `--codegen v1` IS a real escape hatch for a
            // testbench-field `on`, and this seam does not offer it.
            // `lower_scoreboard` lowers a DECLARATION and does not
            // receive the container; the caller COULD supply it —
            // `lower_program` already builds `env_held_type_names` and
            // threads it into `transactor_is_component` for exactly
            // this kind of question — so this is a "does not", not a
            // "cannot".
            //
            // It is also not the whole fix. A container-split testbench
            // arm still spans `connect` (uncompilable) and
            // `on w.seen > 0` (uncompilable), so by the worst-under-arm
            // rule it would reach `EmitsUncompilable`, not
            // `Unsupported`. Recovering the suggestion for the rows
            // that deserve it needs the container AND the per-trigger
            // scope analysis named above — both, not either.
            //
            // Until then: an arm's status is the worst thing v1 does
            // anywhere under it, and a silent drop is the worst of the
            // three.
            ComponentItem::Connect(_) | ComponentItem::OnHandler(_) => {
                return Err(super::not_implemented(
                    &format!("event wiring (`connect`/`on`) on scoreboard `{sb}`"),
                    "as a transactor field v1 drops the wiring entirely and emits the same \
                     code it emits without it, so the scoreboard observes nothing and a \
                     check that should fail passes; as a testbench field it emits an \
                     uncompilable `connect` but a working `on`",
                    super::V1Status::SilentlyMisLowers,
                ));
            }
            ComponentItem::Lifecycle(..) => {}
            _ => {
                return Err(unsupported(
                    &format!("an unsupported item in scoreboard `{sb}`"),
                    "only scalar/queue fields are lowered",
                ));
            }
        }
    }
    Ok(ScoreboardSchema {
        name: sb.clone(),
        fields,
    })
}

/// Classify a scoreboard field type. Scalar fields mirror v1's
/// `scoreboard_field_c_type` → `txn_field_c_type` choices; `queue<T>`
/// maps to `harc_rt::HarcQueue<T>` when `T` is a scalar ≤ 64 bits or a
/// value-record (transaction/struct), mirroring v1's `HarcQueue<Struct>`.
/// The record-element resolution reuses the composite-component path's
/// `lower_queue_elem` so both queue seams agree on the `QueueElem` shape.
/// Errors (not `None`) so the record/enum rejection messages are precise.
fn scoreboard_field_kind(
    sb: &str,
    fname: &str,
    t: &TypeExpr,
    record_ids: &HashMap<String, RecordId>,
) -> Result<ScoreboardFieldKind, LowerError> {
    if let TypeExpr::Builtin {
        name: BuiltinTy::Queue,
        args,
        ..
    } = t
    {
        // Scalar ≤ 64 bits or a value-record element — resolved through the
        // shared component-path helper (don't fork the record-queue seam).
        let elem = super::components::lower_queue_elem(sb, fname, args.first(), record_ids)?;
        return Ok(ScoreboardFieldKind::Queue { elem });
    }
    let ty = scalar_ir_type(t).ok_or_else(|| {
        unsupported(
            &format!("scoreboard field `{sb}.{fname}` of an unsupported type"),
            "only scalar uint/sint/bits/bool fields up to 64 bits and \
             `queue<T>` of such a scalar element type or a `queue<transaction|struct>` \
             are lowered",
        )
    })?;
    Ok(ScoreboardFieldKind::Scalar { ty, default: 0 })
}

/// Scalar field-type mapping, mirroring v1's `txn_field_c_type` choices
/// for the ≤ 64-bit subset. `None` for non-scalar / >64-bit.
fn scalar_ir_type(t: &TypeExpr) -> Option<IrType> {
    let TypeExpr::Builtin { name, args, .. } = t else {
        return None;
    };
    let width = match args.first() {
        Some(TypeArg::Expr(e)) => match &*e.kind {
            ExprKind::Int(s) => Some(s.replace('_', "").parse::<u32>().ok()?),
            _ => return None,
        },
        Some(_) => return None,
        None => None,
    };
    if width.is_some_and(|w| w == 0 || w > 64) {
        return None;
    }
    match name {
        BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits | BuiltinTy::Int => {
            Some(IrType::UInt(width))
        }
        BuiltinTy::SInt | BuiltinTy::SIntCap => Some(IrType::SInt(width)),
        BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => Some(IrType::Bool),
        _ => None,
    }
}

/// A scoreboard field's `default` — same rule as the component form
/// (`components::scalar_default`): folded through the file's constant
/// table, because v1 emits the source text and silently degrades to
/// `= 0` for anything it cannot spell as a C++ initializer.
fn scalar_default(
    default: &Option<crate::ast::Expr>,
    sb: &str,
    fname: &str,
    ty: &crate::ast::TypeExpr,
    consts: &HashMap<String, super::ConstVal>,
) -> Result<u64, LowerError> {
    let Some(d) = default else { return Ok(0) };
    super::components::fold_field_default(
        d,
        Some(ty),
        consts,
        &format!("scoreboard field `{sb}.{fname}`"),
    )
}
