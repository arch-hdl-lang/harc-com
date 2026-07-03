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
//! A field whose type names another transaction/struct lowers to a
//! native nested record (`IrType::Record(rid)`, v1 parity) — copy,
//! equality, and TLM packing recurse through the inner struct. Out-of-
//! scope shapes are explicit `Unsupported` rejections, never silent
//! drops: `when` subtype blocks, `Vec` of a non-scalar element,
//! widthless-scalar leaves, string/object/dynamic leaves, widths above
//! 64 bits, and non-literal field defaults. Recursive / mutually-
//! recursive nested records are rejected (`check_no_record_cycles`).
//! Enum-typed fields lower as scalar variant indices (v1's `int64_t`
//! member shape).

use super::{unsupported, LowerError};
use crate::ast::{
    BuiltinTy, ExprKind, Field, StructDecl, TransactionDecl, TxnBodyItem, TypeArg, TypeExpr,
};
use crate::ir::{IrType, RecordFieldSchema, RecordId, RecordSchema};
use std::collections::HashMap;

pub(crate) fn lower_transaction(
    t: &TransactionDecl,
    enum_names: &std::collections::HashSet<String>,
    record_ids: &HashMap<String, RecordId>,
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
                lower_record_field("transaction", txn, f, enum_names, record_ids, &mut fields)?;
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

/// `struct` declaration → `RecordSchema`. A struct is the same shared
/// value-record shape as a transaction (v1's `emit_struct_record`
/// routes through the very same `emit_record_struct`), so it lowers
/// into the same `records` table and reuses every record-local
/// statement/expression (`RecordInit` / `RecordFieldWrite` /
/// `Expr::RecordField`). Only the structural, scalar (≤64-bit) subset
/// lowers; everything else is an explicit rejection, never a silent
/// drop:
///   - non-scalar / >64-bit fields (e.g. `Vec<...>`, nested structs)
///     and non-literal defaults (the `field_ir_type` / default gate,
///     shared with transactions),
///   - `keep` clauses and `when` subtype blocks in the struct body
///     (a struct body may carry them syntactically; the constraint /
///     tagged-ADT machinery is deferred with `randomize`).
pub(crate) fn lower_struct(
    s: &StructDecl,
    enum_names: &std::collections::HashSet<String>,
    record_ids: &HashMap<String, RecordId>,
) -> Result<RecordSchema, LowerError> {
    let sname = &s.name.name;
    let mut fields: Vec<RecordFieldSchema> = Vec::new();
    // The parser builds `s.fields` as a filtered copy of the `Field`
    // items in `s.body` (it shares the transaction body grammar), so
    // lower from `s.fields` only — exactly v1's `emit_struct_record`,
    // which passes `&s.fields` to `emit_record_struct`. The body is
    // scanned solely to reject the non-field items a struct must not
    // carry in this subset.
    for f in &s.fields {
        lower_record_field("struct", sname, f, enum_names, record_ids, &mut fields)?;
    }
    for item in &s.body {
        match item {
            TxnBodyItem::Field(_) => {}
            TxnBodyItem::Keep(_) => {
                return Err(unsupported(
                    &format!("`keep` constraints in struct `{sname}`"),
                    "constraint metadata lands with `randomize`",
                ));
            }
            TxnBodyItem::When(_) => {
                return Err(unsupported(
                    &format!("`when` subtype blocks in struct `{sname}`"),
                    "",
                ));
            }
        }
    }
    Ok(RecordSchema {
        name: sname.clone(),
        // Structs carry no constraint metadata in this subset.
        fields,
        keeps: Vec::new(),
    })
}

/// Lower one record field (shared by `transaction` and `struct`): map
/// the scalar (≤64-bit) type, fold a literal default, and carry the
/// `with [...]` attribute source text inert for dump-ir. `kind` is the
/// construct label ("transaction"/"struct") for precise diagnostics.
fn lower_record_field(
    kind: &str,
    owner: &str,
    f: &Field,
    enum_names: &std::collections::HashSet<String>,
    record_ids: &HashMap<String, RecordId>,
    fields: &mut Vec<RecordFieldSchema>,
) -> Result<(), LowerError> {
    let fname = &f.name.name;
    if fields.iter().any(|x| x.name == *fname) {
        return Err(LowerError::Invalid(format!(
            "{kind} `{owner}` declares field `{fname}` more than once"
        )));
    }
    // A nested-record field: the field's type names another transaction or
    // struct. Lower it to `IrType::Record(rid)` with `vec_len = None` — a
    // real C++ struct member (v1 parity), so copy / `==` / pack recurse
    // natively. A `default` on a record-typed field is meaningless (its zero
    // value is its own member defaults), so reject one. Checked before the
    // scalar gate so a `Named` record type is not misreported as
    // "non-scalar". `Vec<Record, N>` stays out of scope: `fixed_vec_field`
    // only accepts scalar elements, so a `Vec` of a record falls through to
    // the scalar gate's rejection below.
    if let Some(rid) = named_record_id(&f.ty, record_ids) {
        if f.default.is_some() {
            return Err(unsupported(
                &format!("a `default` on the nested-record field `{owner}.{fname}`"),
                "a record-typed field defaults to its own field defaults",
            ));
        }
        let mut attr_src = Vec::with_capacity(f.attrs.len());
        for a in &f.attrs {
            let mut buf = String::new();
            crate::pretty::print_attr(&mut buf, a);
            attr_src.push(buf);
        }
        fields.push(RecordFieldSchema {
            name: fname.clone(),
            ty: IrType::Record(rid),
            vec_len: None,
            default: None,
            non_random: f.non_random,
            attr_src,
        });
        return Ok(());
    }
    // A `Vec<T, N>` field is the one aggregate this slice lowers: a
    // fixed-size array of a scalar element type (v1's `std::array<T, N>`
    // record member). `ty` then carries the *element* scalar type and
    // `vec_len` the count; everything else (bit layout, C++ storage,
    // access) is driven off those two. A non-scalar element type, a
    // non-literal length, or a nested aggregate is still rejected.
    let (ty, vec_len) = match fixed_vec_field(&f.ty, enum_names) {
        Some((elem_ty, len)) => (elem_ty, Some(len)),
        None => {
            let scalar = field_ir_type(&f.ty, enum_names).ok_or_else(|| {
                unsupported(
                    &format!(
                        "{kind} field `{owner}.{fname}` with an unsupported (non-scalar) \
                         leaf type `{}`",
                        type_expr_label(&f.ty)
                    ),
                    "only uint/sint/bits/bool/bit scalar fields up to 64 bits, fixed \
                     `Vec<T, N>` arrays of such scalars, and nested struct/transaction \
                     fields (whose leaves are themselves supported) are lowered",
                )
            })?;
            (scalar, None)
        }
    };
    // A `Vec<T, N>` field has no scalar `default <lit>` form (its zero
    // value is the empty-brace array); reject a literal default on one.
    if vec_len.is_some() && f.default.is_some() {
        return Err(unsupported(
            &format!("a `default` on the `Vec` field `{owner}.{fname}`"),
            "Vec record fields default to a zero-filled array",
        ));
    }
    let default = match &f.default {
        None => None,
        Some(d) => Some(match &*d.kind {
            ExprKind::Int(s) => super::exprs::parse_int_literal(s).ok_or_else(|| {
                unsupported(
                    &format!("{kind} field default `{owner}.{fname} default {s}`"),
                    "not a plain integer literal",
                )
            })?,
            ExprKind::Bool(b) => *b as u64,
            _ => {
                return Err(unsupported(
                    &format!("a non-literal default on {kind} field `{owner}.{fname}`"),
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
        vec_len,
        default,
        non_random: f.non_random,
        attr_src,
    });
    Ok(())
}

/// Recognize a `Vec<T, N>` field whose element `T` is a scalar this
/// slice can lower, returning `(element IrType, N)`. `None` for any
/// non-`Vec` type, a non-literal length, or a non-scalar / nested
/// element type (those fall through to the scalar gate and its
/// rejection). The element width lives in the returned `IrType`; it
/// drives both the C++ storage type and the packed-bit layout, so a
/// `Vec` element with no explicit width (which would have no defined
/// packed width) is rejected here.
fn fixed_vec_field(
    t: &TypeExpr,
    enum_names: &std::collections::HashSet<String>,
) -> Option<(IrType, usize)> {
    let TypeExpr::Builtin {
        name: BuiltinTy::Vec,
        args,
        ..
    } = t
    else {
        return None;
    };
    let elem = match args.first()? {
        TypeArg::Type(ty) => ty,
        _ => return None,
    };
    let len = match args.get(1)? {
        TypeArg::Expr(e) => match &*e.kind {
            ExprKind::Int(s) => s.replace('_', "").parse::<usize>().ok()?,
            _ => return None,
        },
        _ => return None,
    };
    if len == 0 {
        return None;
    }
    let elem_ty = field_ir_type(elem, enum_names)?;
    // The packed layout needs a defined element width. A widthless
    // scalar (`uint` with no `<N>`) maps to `IrType::UInt(None)`, which
    // has no packed width — reject rather than guess.
    match elem_ty {
        IrType::UInt(Some(_)) | IrType::SInt(Some(_)) | IrType::Bool => Some((elem_ty, len)),
        _ => None,
    }
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

/// `Some(rid)` when `t` is a bare `Named` type whose simple name is a known
/// transaction or struct (a nested-record field type). Enum names live in a
/// disjoint namespace, so a match here is unambiguously a record.
fn named_record_id(t: &TypeExpr, record_ids: &HashMap<String, RecordId>) -> Option<RecordId> {
    let TypeExpr::Named { name, .. } = t else {
        return None;
    };
    let simple = name.segments.last().map(|s| s.name.as_str())?;
    record_ids.get(simple).copied()
}

/// A short, human-readable label for a field's declared type, for the
/// "unsupported leaf type" diagnostic. Best-effort: names the builtin/Named
/// head so the message points at the offending type.
fn type_expr_label(t: &TypeExpr) -> String {
    match t {
        TypeExpr::Named { name, .. } => name
            .segments
            .last()
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "<named>".to_string()),
        TypeExpr::Builtin { name, .. } => format!("{name:?}"),
    }
}

/// Reject recursive / mutually-recursive nested records. A record whose
/// fields transitively reach itself would emit an infinite C++ struct
/// (a by-value member cannot hold its own containing type), so this is a
/// hard error rather than a codegen crash. DFS with a gray/black coloring
/// over `IrType::Record` field edges.
pub(crate) fn check_no_record_cycles(records: &[RecordSchema]) -> Result<(), LowerError> {
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }
    let mut color = vec![Color::White; records.len()];
    // Explicit stack DFS carrying the containing record for the diagnostic.
    fn visit(
        i: usize,
        records: &[RecordSchema],
        color: &mut [Color],
    ) -> Result<(), LowerError> {
        color[i] = Color::Gray;
        for f in &records[i].fields {
            if let IrType::Record(rid) = f.ty {
                let j = rid.index();
                match color.get(j).copied() {
                    Some(Color::Gray) => {
                        // Structurally invalid, NOT a TB-IR-subset gap: a
                        // by-value recursive struct is an infinitely-sized
                        // C++ type in every backend (v1 stack-overflows on
                        // it). Use `Invalid` so the diagnostic does NOT
                        // suggest `re-run with --codegen v1` — that path
                        // crashes rather than accepting the program.
                        return Err(LowerError::Invalid(format!(
                            "recursive nested record `{}`: field `{}` \
                             transitively contains `{}` by value — a record \
                             cannot contain itself (the generated C++ struct \
                             would be infinitely sized). Break the cycle \
                             (e.g. use a handle/index instead of a by-value \
                             field).",
                            records[i].name, f.name, records[j].name
                        )));
                    }
                    Some(Color::White) => visit(j, records, color)?,
                    _ => {}
                }
            }
        }
        color[i] = Color::Black;
        Ok(())
    }
    for i in 0..records.len() {
        if color[i] == Color::White {
            visit(i, records, &mut color)?;
        }
    }
    Ok(())
}
