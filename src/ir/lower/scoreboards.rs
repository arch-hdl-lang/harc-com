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

use super::{not_implemented, unsupported, LowerError, V1Status};
use crate::ast::{BuiltinTy, ComponentDecl, ComponentItem, ExprKind, TypeArg, TypeExpr};
use crate::codegen::cpp_tb::RecordLeafFate;
use crate::ir::{IrType, RecordId, ScoreboardFieldKind, ScoreboardFieldSchema, ScoreboardSchema};
use std::collections::HashMap;

pub(crate) fn lower_scoreboard(
    c: &ComponentDecl,
    record_ids: &HashMap<String, RecordId>,
    // `record_ids` restricted to transactions and structs. By the time
    // scoreboards lower, `record_ids` also holds every regblock's mirror
    // record, and v1's `Emitter::is_record_type` does not — see the
    // capture site in `lower_program`.
    declared_records: &std::collections::HashSet<String>,
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
        // The fourth copy of the same rule, and it carried the same
        // wrong verdict for the same reason: it was measured on a
        // scoreboard with no handler ("BYTE-IDENTICAL to the same
        // scoreboard without the clause"), which shows only that a
        // declaration with nothing to bind has no binding to perform.
        // Give it `on bus.w.handshake(d)` and v1 emits
        // `(bool)(bus.w.handshake(d))` with `bus` declared nowhere —
        // g++ "'bus' was not declared in this scope". Same evidence,
        // same label, and the same detail as the env/agent/sequencer
        // arm in `components.rs`.
        return Err(not_implemented(
            &format!("a `bound to` clause on scoreboard `{sb}`"),
            "v1 does three different things with this clause. A \
             `thread bus.<method>(...)` responder COMPILES and is silently dropped — the \
             target never answers. An `on <ev>` handler body OR a `hookable` body, on an \
             instance bound at a `let x : C = bind <bus>` site, emits a working driver. \
             A cycle trigger, a periodic handler, an `on bus.<ch>.handshake(...)` \
             monitor, or either working shape instantiated as a plain testbench field, \
             emit `bus` verbatim into a scope that declares no such name",
            V1Status::SilentlyMisLowers,
        ));
    }
    let mut fields: Vec<ScoreboardFieldSchema> = Vec::new();
    for item in &c.items {
        match item {
            ComponentItem::Field(f) => {
                let fname = &f.name.name;
                if f.direction.is_some() {
                    // v1 emits `uint64_t p;` — no direction, and no
                    // initializer either, so the field reads
                    // indeterminate rather than as a port.
                    return Err(not_implemented(
                        &format!("a directional (port) field `{sb}.{fname}`"),
                        "scoreboards hold host-state data, not DUT ports; v1 emits an \
                         UNINITIALIZED plain scalar and drops the direction",
                        V1Status::SilentlyMisLowers,
                    ));
                }
                if f.bound_to.is_some() {
                    // Same measurement as the declaration-level clause
                    // above, taken separately: byte-identical output.
                    return Err(not_implemented(
                        &format!("a `bound to` clause on scoreboard field `{sb}.{fname}`"),
                        "v1 discards the clause — its emitted struct is byte-identical to \
                         the unbound one, so the binding silently does not happen",
                        V1Status::SilentlyMisLowers,
                    ));
                }
                if fields.iter().any(|x| x.name == *fname) {
                    return Err(LowerError::Invalid(format!(
                        "scoreboard `{sb}` declares field `{fname}` more than once"
                    )));
                }
                let kind = scoreboard_field_kind(sb, fname, &f.ty, record_ids, declared_records)?;
                let kind = match kind {
                    ScoreboardFieldKind::Scalar { ty, .. } => {
                        let default = scalar_default(&f.default, sb, fname, &f.ty, consts)?;
                        ScoreboardFieldKind::Scalar { ty, default }
                    }
                    other => {
                        if f.default.is_some() {
                            // v1 emits `harc_rt::HarcQueue<uint64_t> q = 0;`
                            // and g++ rejects it: "could not convert '0'
                            // from 'int' to 'harc_rt::HarcQueue<long
                            // unsigned int>'" (`-std=gnu++20`).
                            return Err(not_implemented(
                                &format!("a default on scoreboard queue field `{sb}.{fname}`"),
                                "queues default-construct empty; v1 emits \
                                 `HarcQueue<T> q = 0;`, which has no such constructor",
                                V1Status::EmitsUncompilable,
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
                // UNREACHABLE, and provably so: `lower_program` routes a
                // scoreboard to the composite-component table when
                // `components::scoreboard_is_component` holds, and that
                // predicate is `any(ComponentItem::Hookable(_))` — the
                // exact condition of this arm. Replacing the body with
                // `unreachable!()` leaves the whole suite green.
                //
                // The comment that used to sit here described an intent
                // ("reject the declaration so a method-bearing scoreboard
                // never lowers to a struct missing its methods") that the
                // routing gate has since made moot; it is deleted rather
                // than kept above this one. Kept as an invariant
                // guard, and `Invalid` rather than a v1 suggestion: if it
                // ever did fire, the routing above would be broken, which
                // is not something `--codegen v1` can help with.
                return Err(LowerError::Invalid(format!(
                    "internal: method-bearing scoreboard `{sb}` reached the data-only \
                     lowering path (method `{}`)",
                    h.name.name
                )));
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
            // As a TRANSACTOR field, v1 emits output byte-identical
            // to the same program with the `connect`/`on` deleted. It
            // silently DROPS the wiring, so the scoreboard observes
            // nothing and a test that should catch a mismatch passes
            // green.
            //
            // "Byte-identical" is exact on the program the test uses
            // (`a_transactor_held_scoreboard_has_its_wiring_dropped_by_
            // v1`, string equality, both wiring forms). A first probe
            // reported it as 608 lines either way with a source OFFSET
            // left in `_solver_site_<N>` names and an auto-coverage
            // plan literal — which is not byte-identity, and was a
            // property of that probe's unrelated randomize sites rather
            // than of the wiring.
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
    declared_records: &std::collections::HashSet<String>,
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
        // Same question as the record-field arm, asked with the same
        // predicate rather than a second copy of it: what does v1 do
        // with this leaf? Measured here too, because a scoreboard's
        // supported set differs (a `queue<T>` IS a scoreboard field)
        // even though the rule does not:
        //
        //   list<uint<8>>  ->  std::vector<uint64_t> l;   a real hatch
        //   string         ->  int64_t s;                 uninitialized
        //   event<uint<8>> ->  uint64_t e;                uninitialized
        //
        // The two flattened forms compile and read indeterminate, which
        // is worse than not compiling and is why they lose the
        // suggestion.
        //
        // Only `Flattens` does, though. A scoreboard emits no
        // `randomize_*` body, so the third fate — a container v1 keeps
        // and then assigns `0` to — cannot arise on this path, and that
        // leaf's MEMBER is correct, which is all a scoreboard field
        // uses. Measured: `list<Vec<uint<8>, 2>>` gives
        // `std::vector<std::array<uint64_t, 2>> l;` and compiles, while
        // the same leaf in a transaction does not.
        const SUBSET: &str = "only scalar uint/sint/bits/bool fields up to 64 bits and \
                              `queue<T>` of such a scalar element type or a \
                              `queue<transaction|struct>` are lowered";
        let what = format!("scoreboard field `{sb}.{fname}` of an unsupported type");
        let fate = crate::codegen::cpp_tb::record_leaf_fate(t, &|n| declared_records.contains(n));
        if fate == RecordLeafFate::Flattens {
            return not_implemented(
                &what,
                format!(
                    "{SUBSET}; v1 emits an UNINITIALIZED plain scalar for it, so the \
                     field reads indeterminate rather than as what was written"
                ),
                V1Status::SilentlyMisLowers,
            );
        }
        unsupported(&what, SUBSET)
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
