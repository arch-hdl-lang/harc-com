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
//! Out-of-scope shapes are explicit `Unsupported` rejections, never
//! silent drops:
//!   - scoreboard `hookable`/`function` methods (need per-instance state
//!     materialization — rejected at the call site, but the declaration
//!     itself is also rejected here so an unreferenced method-bearing
//!     scoreboard does not lower to a struct missing its methods);
//!   - `connect` / `on` event wiring (gates on the agent/env/event
//!     slices);
//!   - queue element types that are not scalars ≤ 64 bits (e.g.
//!     `queue<SomeStruct>` — needs the record-payload-in-queue seam);
//!   - non-scalar / >64-bit scalar fields.

use super::{unsupported, LowerError};
use crate::ast::{BuiltinTy, ComponentDecl, ComponentItem, ExprKind, TypeArg, TypeExpr};
use crate::ir::{IrType, ScoreboardFieldKind, ScoreboardFieldSchema, ScoreboardSchema};

pub(crate) fn lower_scoreboard(c: &ComponentDecl) -> Result<ScoreboardSchema, LowerError> {
    let sb = &c.name.name;
    if !c.params.is_empty() {
        return Err(unsupported(&format!("parameters on scoreboard `{sb}`"), ""));
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
                let kind = scoreboard_field_kind(&f.ty).ok_or_else(|| {
                    unsupported(
                        &format!("scoreboard field `{sb}.{fname}` of an unsupported type"),
                        "only scalar uint/sint/bits/bool fields up to 64 bits and \
                         `queue<T>` of such a scalar element type are lowered",
                    )
                })?;
                let kind = match kind {
                    ScoreboardFieldKind::Scalar { ty, .. } => {
                        let default = scalar_default(&f.default, sb, fname)?;
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
            ComponentItem::Connect(_) | ComponentItem::OnHandler(_) => {
                return Err(unsupported(
                    &format!("event wiring (`connect`/`on`) on scoreboard `{sb}`"),
                    "",
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
/// maps to `harc_rt::HarcQueue<T>` when `T` is a scalar ≤ 64 bits.
/// `None` for anything out of the lowered subset.
fn scoreboard_field_kind(t: &TypeExpr) -> Option<ScoreboardFieldKind> {
    let TypeExpr::Builtin { name, args, .. } = t else {
        return None;
    };
    if matches!(name, BuiltinTy::Queue) {
        // Element type must be a scalar ≤ 64 bits in this subset.
        let elem = match args.first() {
            Some(TypeArg::Type(ty)) => ty.clone(),
            // `queue<uint<32>>` can parse the inner as a type-arg-expr in
            // some positions; only the explicit `Type` form is in scope.
            _ => return None,
        };
        let signed = match scalar_ir_type(&elem)? {
            IrType::SInt(_) => true,
            IrType::UInt(_) | IrType::Bool => false,
            _ => return None,
        };
        return Some(ScoreboardFieldKind::Queue {
            elem: crate::ir::QueueElem::Scalar { signed },
        });
    }
    let ty = scalar_ir_type(t)?;
    Some(ScoreboardFieldKind::Scalar { ty, default: 0 })
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

fn scalar_default(
    default: &Option<crate::ast::Expr>,
    sb: &str,
    fname: &str,
) -> Result<u64, LowerError> {
    match default {
        None => Ok(0),
        Some(d) => match &*d.kind {
            ExprKind::Int(s) => super::exprs::parse_int_literal(s).ok_or_else(|| {
                unsupported(
                    &format!("scoreboard field default `{sb}.{fname} default {s}`"),
                    "not a plain integer literal",
                )
            }),
            ExprKind::Bool(b) => Ok(*b as u64),
            _ => Err(unsupported(
                &format!("a non-literal default on scoreboard field `{sb}.{fname}`"),
                "",
            )),
        },
    }
}
