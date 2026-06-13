//! `transaction` declaration lowering → `RecordSchema`.
//!
//! Only the *structural* shape lowers: scalar fields with names,
//! types, and literal defaults, in declaration order — the shape v1's
//! `emit_record_struct` emits as a C++ value-record. Constraint
//! metadata (`keep` clauses, `with [...]` field attributes, the `!`
//! non-random prefix) is carried as inert, pretty-printed source text
//! for dump-ir; the constraint-IR layer (`src/constraints`)
//! re-elaborates constraints from the AST when `randomize` lands, so
//! nothing downstream may interpret these strings (see the
//! `RecordSchema` doc in `src/ir/mod.rs`).
//!
//! Out-of-scope shapes are explicit `Unsupported` rejections, never
//! silent drops: `when` subtype blocks, non-scalar field types
//! (nested records, lists, vecs), widths above 64 bits, and
//! non-literal field defaults. Enum-typed fields lower as scalar
//! variant indices (v1's `int64_t` member shape).

use super::{LowerError, unsupported};
use crate::ast::{BuiltinTy, ExprKind, TransactionDecl, TxnBodyItem, TypeArg, TypeExpr};
use crate::ir::{IrType, RecordFieldSchema, RecordSchema};

pub(crate) fn lower_transaction(
    t: &TransactionDecl,
    enum_names: &std::collections::HashSet<String>,
) -> Result<RecordSchema, LowerError> {
    let txn = &t.name.name;
    if !t.params.is_empty() {
        return Err(unsupported(
            &format!("parameters on transaction `{txn}`"),
            "",
        ));
    }
    let mut fields: Vec<RecordFieldSchema> = Vec::new();
    let mut keeps: Vec<String> = Vec::new();
    for item in &t.body {
        match item {
            TxnBodyItem::Field(f) => {
                let fname = &f.name.name;
                if fields.iter().any(|x| x.name == *fname) {
                    return Err(LowerError::Invalid(format!(
                        "transaction `{txn}` declares field `{fname}` more than once"
                    )));
                }
                let ty = field_ir_type(&f.ty, enum_names).ok_or_else(|| {
                    unsupported(
                        &format!("transaction field `{txn}.{fname}` with a non-scalar type"),
                        "only uint/sint/bits/bool/bit fields up to 64 bits are lowered",
                    )
                })?;
                let default = match &f.default {
                    None => None,
                    Some(d) => Some(match &*d.kind {
                        ExprKind::Int(s) => {
                            super::exprs::parse_int_literal(s).ok_or_else(|| {
                                unsupported(
                                    &format!(
                                        "transaction field default `{txn}.{fname} default {s}`"
                                    ),
                                    "not a plain integer literal",
                                )
                            })?
                        }
                        ExprKind::Bool(b) => *b as u64,
                        _ => {
                            return Err(unsupported(
                                &format!(
                                    "a non-literal default on transaction field `{txn}.{fname}`"
                                ),
                                "",
                            ));
                        }
                    }),
                };
                let mut attr_src = Vec::with_capacity(f.attrs.len());
                for a in &f.attrs {
                    let mut buf = String::new();
                    crate::pretty::print_attr(&mut buf, a);
                    attr_src.push(buf);
                }
                fields.push(RecordFieldSchema {
                    name: fname.clone(),
                    ty,
                    default,
                    non_random: f.non_random,
                    attr_src,
                });
            }
            TxnBodyItem::Keep(k) => {
                keeps.push(crate::codegen::cpp_tb::expr_source_str(&k.expr));
            }
            TxnBodyItem::When(_) => {
                return Err(unsupported(
                    &format!("`when` subtype blocks in transaction `{txn}`"),
                    "",
                ));
            }
        }
    }
    Ok(RecordSchema {
        name: txn.clone(),
        fields,
        keeps,
    })
}

/// Scalar field-type mapping, mirroring v1's `txn_field_c_type` C-type
/// choices for the ≤64-bit subset: uint/bits/int → unsigned, sint →
/// signed, bool/bit → bool. `None` for anything this slice does not
/// lower (nested records, enums, lists, vecs, >64-bit widths).
fn field_ir_type(t: &TypeExpr, enum_names: &std::collections::HashSet<String>) -> Option<IrType> {
    let TypeExpr::Builtin { name, args, .. } = t else {
        // Enum-typed field: v1 lowers it to an `int64_t` struct member
        // holding the variant index (`txn_field_c_type` Named → int64_t),
        // so the IR mirrors it as an unwidthed SInt scalar.
        if let TypeExpr::Named { name, .. } = t {
            let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
            if enum_names.contains(simple) {
                return Some(IrType::SInt(None));
            }
        }
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
        // v1 lowers `int` record fields through `cpp_uint_for_width`
        // (unsigned), so the IR mirrors that as UInt.
        BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits | BuiltinTy::Int => {
            Some(IrType::UInt(width))
        }
        BuiltinTy::SInt | BuiltinTy::SIntCap => Some(IrType::SInt(width)),
        BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => Some(IrType::Bool),
        _ => None,
    }
}
