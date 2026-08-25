//! `transaction` / `struct` declaration lowering → `RecordSchema`.
//!
//! The value shape lowers as scalar fields with names, types, and literal
//! defaults, in declaration order — the shape v1's `emit_record_struct`
//! emits as a C++ value-record. Constraint metadata (`keep` clauses,
//! `with [...]` field attributes, the `!` non-random prefix) is carried
//! as inert, pretty-printed source text for dump-ir; the constraint-IR
//! layer (`src/constraints`) re-elaborates constraints from the AST when
//! `randomize` lands, so nothing downstream may interpret these strings
//! (see the `RecordSchema` doc in `src/ir/mod.rs`).
//!
//! A field whose type names another transaction/struct lowers to a
//! native nested record (`IrType::Record(rid)`, v1 parity) — copy,
//! equality, and TLM packing recurse through the inner struct. A
//! `Vec<T, N>` field lowers when `T` is a supported scalar or a
//! supported record (`IrType::Record` element + `vec_len`, v1's
//! `std::array<T, N>` member). Out-of-scope shapes are explicit
//! `Unsupported` rejections, never silent drops: `when` subtype blocks,
//! `Vec` of any other element (enums and `Vec`-of-`Vec`),
//! string/object/dynamic leaves,
//! widths above 64 bits, and non-literal field defaults. Recursive /
//! mutually-recursive nested records are rejected
//! (`check_no_record_cycles`). Scalar-element `list<T>` fields lower as
//! dynamic scalar sequences for declaration, equality, and randomization;
//! ordinary body-position list indexing/querying remains a separate slice.
//! Enum-typed fields lower as scalar variant indices (v1's `int64_t`
//! member shape).

use super::{not_implemented, unsupported, LowerError, V1Status};
use crate::ast::{
    BuiltinTy, ExprKind, Field, StructDecl, TransactionDecl, TxnBodyItem, TypeArg, TypeExpr,
};
use crate::ir::{IrType, RecordFieldSchema, RecordId, RecordSchema};
use std::collections::HashMap;

/// A record field whose leaf type TB-IR does not model. The arm serves
/// a dozen shapes and they take THREE different verdicts, decided by
/// what v1 does with the leaf rather than by the type name:
///
/// | field type | v1 emits | verdict |
/// |---|---|---|
/// | `list<uint<8>>`, `list<uint<256>>`, `list<bool>` | `std::vector<T>` + resize + per-element draw | likewise |
/// | `Vec<uint, 4>`, `Vec<sint, 4>`, `Vec<int, 4>` | `std::array<T, 4>` | modeled by `RecordFieldSchema::vec_len` |
/// | `Vec<Vec<uint<8>, 2>, 4>` | the nested `std::array` | likewise |
/// | `list<Vec<uint<8>, 2>>` | `std::vector<std::array<uint64_t, 2>>` — then `[_i] = 0` | `EmitsUncompilable` |
/// | `list<Inner>`, `list<string>` | `std::vector<uint64_t>`, and randomize skips the field | `SilentlyMisLowers` |
/// | `list<queue<uint<8>>>`, `list<int>` | `std::vector<uint64_t>` + `[_i] = 0` | `SilentlyMisLowers` |
/// | `Vec<queue<uint<8>>, 4>` | `std::array<uint64_t, 4>` — array survives, element does not | `SilentlyMisLowers` |
/// | `Vec<string, 4>`, `Vec<uint<8>, N>` | `uint64_t` — the whole array collapsed | `SilentlyMisLowers` |
/// | `queue<uint<8>>` / `string` / `event<T>` / `object` | a bare scalar | `SilentlyMisLowers` |
///
/// Every row is decided by the member type `txn_field_c_type` picks and
/// the draw `emit_field_random` writes, so this asks THOSE —
/// `cpp_tb::record_leaf_fate` sits next to both and recurses through
/// the same `list_elem_type` / `fixed_vec_type_args` helpers. Three
/// versions of this arm got it wrong by restating the rules instead:
/// one `queue` probe generalised to everything denied `list` its
/// working hatch; a hand-written type table called a 128-bit field a
/// flattening and `list<Inner>` a working hatch, both backwards; and
/// neither noticed that `list<Vec<…>>` makes v1's output stop
/// compiling.
/// A scalar leaf whose width slot holds something this compiler cannot
/// read as a plain decimal — a non-decimal literal (`uint<0x8>`,
/// `uint<0b1000>`) or a named constant (`uint<W>` against
/// `const W = 8`).
///
/// v1 cannot read it either, but it does not say so — it substitutes a
/// DIFFERENT fallback in each place that needs the width, so the field
/// means three things at once. Measured on `uint<0x8>` against `uint<8>`:
///
/// | site | `uint<8>` | `uint<0x8>` |
/// |---|---|---|
/// | pack | `harc_wide_write_bits(_packed, 0, 8, …)` | `…, 0, 64, …` |
/// | unpack | `harc_bits(_packed, 7, 0)` | `harc_bits(_packed, 63, 0)` |
/// | randomize | `harc_rng_uint(harc_rng_next, 8)` | `…, 32)` |
/// | problem table | `field data u8` | `field data u8` |
///
/// `uint<W>` behaves identically — v1 does not fold the constant into
/// the width either, and packs 64 while drawing 32.
///
/// It compiles and runs. That is a silent mis-lowering, so the leaf
/// must not carry a `--codegen v1` suggestion even though its MEMBER
/// type (`uint64_t`) is one v1 gets right.
fn unreadable_width_leaf(ty: &TypeExpr) -> bool {
    let TypeExpr::Builtin { name, args, .. } = ty else {
        return false;
    };
    if matches!(name, BuiltinTy::Vec) {
        // The element carries the width, and v1 substitutes the same
        // fallbacks per element: `Vec<uint<0x8>, 4>` packs four 64-bit
        // slots where `Vec<uint<8>, 4>` packs four 8-bit ones.
        return matches!(args.first(), Some(TypeArg::Type(elem)) if unreadable_width_leaf(elem));
    }
    // Only a builtin that HAS a width slot. `queue<T>` / `event<T>` /
    // `list<T>` take a payload in that position, and a RECORD payload
    // arrives as `TypeArg::Expr(Ident)` — the shape this arm otherwise
    // reads as an unreadable width. `cpp_tb.rs`'s own comment says so
    // ("`event<RegOp>` parses as `TypeArg::Expr(Ident)` at the type-arg
    // layer") and so does `fixed_vec_field`'s NOTE, so the set is
    // copied from `scalar_leaf_c_type` rather than inferred from the
    // argument shape.
    if !matches!(
        name,
        BuiltinTy::UInt
            | BuiltinTy::UIntCap
            | BuiltinTy::Bits
            | BuiltinTy::Int
            | BuiltinTy::SInt
            | BuiltinTy::SIntCap
    ) {
        return false;
    }
    matches!(args.first(), Some(TypeArg::Expr(_))) && declared_scalar_width(args).is_none()
}

fn non_scalar_record_leaf(
    kind: &str,
    owner: &str,
    fname: &str,
    ty: &TypeExpr,
    record_ids: &HashMap<String, RecordId>,
) -> LowerError {
    use crate::codegen::cpp_tb::RecordLeafFate;
    const SUBSET: &str = "only nonzero-width uint/sint/bits/bool/bit scalar fields, fixed \
                          `Vec<T, N>` arrays of such scalars or of supported \
                          struct/transaction records, and nested struct/transaction fields \
                          (whose leaves are themselves supported) are lowered";
    let what = format!(
        "{kind} field `{owner}.{fname}` with an unsupported (non-scalar) leaf type `{}`",
        type_expr_label(ty)
    );
    if unreadable_width_leaf(ty) {
        return not_implemented(
            &what,
            format!(
                "{SUBSET}; a width must be a plain decimal literal — v1 cannot read this \
                 one either and silently substitutes a different fallback in the pack, \
                 the unpack and the random draw"
            ),
            V1Status::SilentlyMisLowers,
        );
    }
    match crate::codegen::cpp_tb::record_leaf_fate(ty, &|n| record_ids.contains_key(n)) {
        RecordLeafFate::Models => unsupported(&what, SUBSET),
        RecordLeafFate::Flattens => not_implemented(
            &what,
            format!(
                "{SUBSET}; v1 flattens the field to a plain scalar and runs, so it means \
                 something other than what was written"
            ),
            V1Status::SilentlyMisLowers,
        ),
        RecordLeafFate::Uncompilable => not_implemented(
            &what,
            format!(
                "{SUBSET}; v1 keeps the container but has no per-element draw for the \
                 element, so its randomize body assigns `0` to a `std::array` and the \
                 emitted C++ does not compile"
            ),
            V1Status::EmitsUncompilable,
        ),
    }
}

/// A field `default` written as an integer literal this compiler cannot
/// represent — a value past `u64`. Sized literals that fit now lower
/// directly through `parse_sized_int_literal`; the line is the VALUE,
/// not the spelling. v1 folds a sized literal through
/// `cpp_tb::normalized_int_literal` and pastes the result into a member
/// TB-IR only ever gives 64 bits.
///
/// | default | v1 emits | outcome |
/// |---|---|---|
/// | `4'd3` | `uint64_t a = 3;` | folds correctly — a real escape hatch |
/// | `8'hFF` | `uint64_t a = 0xFF;` | likewise |
/// | `128'hFFFFFFFFFFFFFFFFFFFF` | `(((_harc_u128)0xFFFFULL << 64) \| …)` | `-Woverflow`, truncates to 64 bits |
/// | `99999999999999999999999` | the literal verbatim | `-Woverflow`, truncates |
///
/// So the guard normalizes exactly as v1 does and asks whether the
/// result fits — splitting on the apostrophe, which is what this did
/// first, puts the wide-hex row on the wrong side.
fn record_default_literal(kind: &str, owner: &str, fname: &str, lit: &str) -> LowerError {
    let what = format!("{kind} field default `{owner}.{fname} default {lit}`");
    not_implemented(
        &what,
        "the value does not fit the 64-bit member either backend gives this field; v1 \
         folds it in anyway — verbatim for a decimal, as a `_harc_u128` composite for a \
         wide hex — and g++ truncates it to the low 64 bits with only a warning",
        V1Status::SilentlyMisLowers,
    )
}

pub(crate) fn lower_transaction(
    t: &TransactionDecl,
    enum_names: &std::collections::HashSet<String>,
    record_ids: &HashMap<String, RecordId>,
    consts: &HashMap<String, super::ConstVal>,
) -> Result<RecordSchema, LowerError> {
    let txn = &t.name.name;
    if !t.params.is_empty() {
        // The fifth landing of the component-parameter construct, and it
        // agrees with the other four. v1 never reads a `#(...)` list, and
        // a transaction gives it three ways to matter:
        //
        //   * unused — a no-op.
        //   * a field default (`tag : uint<8> default N`) — emitted
        //     verbatim inside the struct, ahead of every file-scope
        //     `const`, so `N` is undeclared and it does not compile.
        //   * a `keep` constraint (`keep tag < N`) — this is the silent
        //     one, and it is silent in a worse way than the component
        //     arms. The constraint IR CONST-FOLDS `N` against a
        //     same-named file-scope `const`, so v1 emits
        //     `_s.add(z3::ult(_z_tag, _ctx.bv_val((uint64_t)5, 64)))`
        //     with the const's 5 baked in — not the parameter's default,
        //     not any `#(...)` argument, and with no `N` left in the
        //     output to notice. It compiles and randomizes to the wrong
        //     bound.
        //
        // The `keep` position was missed on the first pass by reading
        // the FAIL log line it also emits (`constraint \`tag < N\`
        // participated in the solve`) and concluding the reference only
        // reached a string. The solver line sits thirty lines above it.
        return Err(not_implemented(
            &format!("parameters on transaction `{txn}`"),
            "v1 drops the parameter list entirely: a field default referencing one emits \
             an undeclared name, and a `keep` constraint const-folds it against a \
             same-named file-scope `const`, randomizing to that bound instead",
            V1Status::SilentlyMisLowers,
        ));
    }
    let mut fields: Vec<RecordFieldSchema> = Vec::new();
    let mut keeps: Vec<String> = Vec::new();
    for item in &t.body {
        match item {
            TxnBodyItem::Field(f) => {
                lower_record_field(
                    "transaction",
                    txn,
                    f,
                    enum_names,
                    record_ids,
                    &mut fields,
                    consts,
                )?;
            }
            TxnBodyItem::Keep(k) => {
                keeps.push(crate::codegen::cpp_tb::expr_source_str(&k.expr));
            }
            TxnBodyItem::When(_) => {
                // This arm has now been measured three times and the
                // first two both looked at one shape each.
                //
                // Round one probed a program with `wait 1 cycle` for a
                // run body. With no `randomize` there is no solve site
                // for a guard to appear in, so it concluded v1 drops the
                // guard — right label, no evidence.
                //
                // Round two added `randomize(q)` and found v1 emitting
                // `if (q.op == 1) { … q.addr = _val_addr; }` — the guard
                // honoured — and flipped the arm to a real escape hatch.
                // That is true of the DIRECT shape only.
                //
                // Round three is the shape round two's own comment named
                // and did not run. Put the subtype inside another record
                // (`transaction Outer { tag : uint<8>; inner : Req }`)
                // and randomize the OUTER one, and v1 reaches `Req`
                // through `static void randomize_Req(Req*)`:
                //
                //     t->op   = harc_rng_uint(harc_rng_next, 8);
                //     t->addr = harc_rng_uint(harc_rng_next, 16);
                //
                // No guard — `op == 1` appears zero times in the whole
                // file — and the solver's problem table lists `tag` and
                // `inner.op` but not `inner.addr`, so the conditional
                // field is not in the solve at all. It compiles and
                // runs.
                //
                // An arm's status is the worst thing v1 does anywhere
                // under it, and that is this.
                return Err(not_implemented(
                    &format!("`when` subtype blocks in transaction `{txn}`"),
                    "v1 honours the guard when the transaction is randomized directly, but \
                     reaches a NESTED one through an unconditional `randomize_<T>` that \
                     drops the guard and leaves the conditional field out of the solve",
                    V1Status::SilentlyMisLowers,
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
/// `Expr::RecordField`). The structural subset includes scalar fields,
/// nested records, and fixed scalar/record `Vec<T, N>` fields; everything
/// else is an explicit rejection, never a silent drop:
///   - unsupported aggregate / >64-bit fields (e.g. nested `Vec<Vec<...>>`)
///     and non-literal defaults (the `field_ir_type` / default gate,
///     shared with transactions),
///   - `keep` clauses and `when` subtype blocks in the struct body
///     (a struct body may carry them syntactically; the constraint /
///     tagged-ADT machinery is deferred with `randomize`).
pub(crate) fn lower_struct(
    s: &StructDecl,
    enum_names: &std::collections::HashSet<String>,
    record_ids: &HashMap<String, RecordId>,
    consts: &HashMap<String, super::ConstVal>,
) -> Result<RecordSchema, LowerError> {
    let sname = &s.name.name;
    let mut fields: Vec<RecordFieldSchema> = Vec::new();
    let mut keeps: Vec<String> = Vec::new();
    // The parser builds `s.fields` as a filtered copy of the `Field`
    // items in `s.body` (it shares the transaction body grammar), so
    // lower from `s.fields` only — exactly v1's `emit_struct_record`,
    // which passes `&s.fields` to `emit_record_struct`. The body is
    // scanned solely to reject the non-field items a struct must not
    // carry in this subset.
    for f in &s.fields {
        lower_record_field(
            "struct",
            sname,
            f,
            enum_names,
            record_ids,
            &mut fields,
            consts,
        )?;
    }
    for item in &s.body {
        match item {
            TxnBodyItem::Field(_) => {}
            TxnBodyItem::Keep(k) => {
                keeps.push(crate::codegen::cpp_tb::expr_source_str(&k.expr));
            }
            TxnBodyItem::When(_) => {
                // Measured: v1's `struct Rec` is BYTE-IDENTICAL to the
                // same struct without the `when` block — the
                // conditional field is dropped entirely. Reading it
                // (`r.b`) then fails to compile ("'struct Rec' has no
                // member named 'b'"), but a program that only declares
                // it loses a field silently, and that is the worse of
                // the two.
                return Err(not_implemented(
                    &format!("`when` subtype blocks in struct `{sname}`"),
                    "v1 drops the conditional fields from the struct entirely — a program \
                     that declares one and never reads it loses it with no diagnostic",
                    V1Status::SilentlyMisLowers,
                ));
            }
        }
    }
    Ok(RecordSchema {
        name: sname.clone(),
        fields,
        keeps,
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
    consts: &HashMap<String, super::ConstVal>,
) -> Result<(), LowerError> {
    let fname = &f.name.name;
    if fields.iter().any(|x| x.name == *fname) {
        return Err(LowerError::Invalid(format!(
            "{kind} `{owner}` declares field `{fname}` more than once"
        )));
    }
    if zero_width_leaf(&f.ty) {
        // NOT `Invalid`, and the reason is a lesson about the whole
        // sweep. The first version rested on v1 PANICKING here —
        // "attempt to subtract with overflow" in `emit_unpack_bits`,
        // reached from `emit_record_pack_helpers`. That panic is a
        // DEBUG-BUILD artifact: Rust turns integer overflow checks off
        // under `--release`, which is how CI builds and how the `harc`
        // binary ships. In release v1 does not panic at all — it emits
        // a complete testbench that compiles clean:
        //
        //   uint<0>          uint64_t data = 0;
        //                    harc_wide_write_bits(_packed, 0, 0, value.data)
        //   Vec<uint<0>, 4>  std::array<uint64_t, 4> data = {};
        //                    …the same zero-width write, per element
        //
        // So the field is a full 64-bit member that carries no packed
        // bits, silently. `Invalid` claims no backend runs it in any
        // configuration, and a release-built v1 runs it.
        return Err(not_implemented(
            &format!(
                "{kind} `{owner}` field `{fname}` has a zero-width type; \
                 a scalar width must be greater than zero"
            ),
            "v1 emits a full-width member for it that carries no packed bits — a debug \
             build panics on the width arithmetic, but a release build, which is how it \
             ships, compiles and runs",
            V1Status::SilentlyMisLowers,
        ));
    }
    // A nested-record field: the field's type names another transaction or
    // struct. Lower it to `IrType::Record(rid)` with `vec_len = None` — a
    // real C++ struct member (v1 parity), so copy / `==` / pack recurse
    // natively. A `default` on a record-typed field is meaningless (its zero
    // value is its own member defaults), so reject one. Checked before the
    // scalar gate so a `Named` record type is not misreported as
    // "non-scalar". `Vec<Record, N>` is handled by `fixed_vec_field` below
    // (`IrType::Record` element + `vec_len = Some(N)`, v1's
    // `std::array<Inner, N>` member).
    if let Some(rid) = named_record_id(&f.ty, record_ids) {
        if f.default.is_some() {
            // v1 emits the initializer straight into the member:
            // `Inner i = 0;`, which g++ rejects — "could not convert
            // '0' from 'int' to 'Inner'". Measured with `-std=gnu++20`,
            // the standard `src/main.rs` builds with.
            return Err(not_implemented(
                &format!("a `default` on the nested-record field `{owner}.{fname}`"),
                "a record-typed field defaults to its own field defaults; v1 emits \
                 `Inner i = 0;` and g++ rejects the conversion",
                V1Status::EmitsUncompilable,
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
    // A dynamic `list<T>` field with a scalar element type is native
    // value-record state. The constraint backend already models its length
    // and elements and v1 emits it as `std::vector<T>`; represent that shape
    // with the existing scalar-sequence IR type so it cannot be mistaken for
    // either a scalar field or a fixed `Vec<T, N>` below.
    if let Some(elem) = record_list_scalar_ir_type(&f.ty) {
        if f.default.is_some() {
            return Err(not_implemented(
                &format!("a `default` on the dynamic list field `{owner}.{fname}`"),
                "a list is default-constructed empty; v1 emits `std::vector<T> field = 0` and the C++ conversion does not compile",
                V1Status::EmitsUncompilable,
            ));
        }
        if !f.attrs.is_empty() {
            return Err(not_implemented(
                &format!("randomization modifiers on the dynamic list field `{owner}.{fname}`"),
                "the shared v1 record randomizer returns from its list path before applying \
                 `range`, `dist`, or `unique`, and the constrained solver likewise omits \
                 those modifiers for list elements; accepting them would claim constraints \
                 participated while silently drawing unrestricted values",
                V1Status::SilentlyMisLowers,
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
            ty: IrType::Seq(Box::new(elem)),
            vec_len: None,
            default: None,
            non_random: f.non_random,
            attr_src,
        });
        return Ok(());
    }
    // A `Vec<T, N>` field is the one aggregate this slice lowers: a
    // fixed-size array of a scalar OR record element type (v1's
    // `std::array<T, N>` record member). `ty` then carries the *element*
    // type and `vec_len` the count; everything else (bit layout, C++
    // storage, access) is driven off those two. An unsupported element
    // type, a non-literal length, or a `Vec`-of-`Vec` is still rejected.
    let (ty, vec_len) = match fixed_vec_field(&f.ty, enum_names, record_ids) {
        Some((elem_ty, len)) => (elem_ty, Some(len)),
        None => {
            let scalar = field_ir_type(&f.ty, enum_names)
                .ok_or_else(|| non_scalar_record_leaf(kind, owner, fname, &f.ty, record_ids))?;
            (scalar, None)
        }
    };
    // A `Vec<T, N>` field has no scalar `default <lit>` form (its zero
    // value is the empty-brace array); reject a literal default on one.
    if vec_len.is_some() && f.default.is_some() {
        // Same shape as the nested-record default: v1 emits
        // `std::array<uint64_t, 4> v = 0;` and g++ rejects it —
        // "could not convert '0' from 'int' to
        // 'std::array<long unsigned int, 4>'".
        return Err(not_implemented(
            &format!("a `default` on the `Vec` field `{owner}.{fname}`"),
            "Vec record fields default to a zero-filled array; v1 emits \
             `std::array<T, N> v = 0;` and g++ rejects the conversion",
            V1Status::EmitsUncompilable,
        ));
    }
    // Folded through the file's constant table, like the component /
    // scoreboard / transactor-state field defaults (divergence 35). v1
    // emits a `const` default as `uint64_t a = K;` against its own
    // `static constexpr K` — correct, unlike the addrmap and regblock
    // offsets, which fold to ZERO (divergences 39 and 41). Same local
    // literals-only folder in all four places; different v1 behaviour
    // behind each, which is why each was probed separately.
    let default = match &f.default {
        None => None,
        Some(d) => Some(match &*d.kind {
            ExprKind::Int(s) => super::exprs::parse_int_literal(s)
                .or_else(|| super::exprs::parse_sized_int_literal(s))
                .ok_or_else(|| record_default_literal(kind, owner, fname, s))?,
            ExprKind::Bool(b) => *b as u64,
            _ => super::components::fold_field_default(
                d,
                Some(&f.ty),
                consts,
                &format!("{kind} field `{owner}.{fname}`"),
            )?,
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

/// Scalar list element shapes whose C++ storage and random draw v1 preserves.
/// `int` is intentionally excluded: v1 silently renders it unsigned.
fn record_list_scalar_ir_type(t: &TypeExpr) -> Option<IrType> {
    let TypeExpr::Named { name, generics, .. } = t else {
        return None;
    };
    if !matches!(
        name.segments.last().map(|segment| segment.name.as_str()),
        Some("list" | "List")
    ) {
        return None;
    }
    let elem = match generics.first()? {
        TypeArg::Type(elem) => elem,
        _ => return None,
    };
    let TypeExpr::Builtin { name, .. } = elem else {
        return None;
    };
    let ty = matches!(
        name,
        BuiltinTy::UInt
            | BuiltinTy::UIntCap
            | BuiltinTy::Bits
            | BuiltinTy::SInt
            | BuiltinTy::SIntCap
            | BuiltinTy::Bool
            | BuiltinTy::BoolLower
            | BuiltinTy::Bit
    )
    .then(|| super::components::scalar_field_ir_type(elem))
    .flatten()?;
    matches!(
        ty,
        IrType::Bool
            | IrType::UInt(Some(1..=crate::MAX_WIDTH_METHOD_BITS))
            | IrType::SInt(Some(1..=64))
    )
    .then_some(ty)
}

/// Recognize a `Vec<T, N>` field whose element `T` this slice can
/// lower — a supported scalar, or a transaction/struct record (whose
/// own fields were validated when IT lowered) — returning
/// `(element IrType, N)`. `None` for any non-`Vec` type, a non-literal
/// length, or an unsupported element type (those fall through to the
/// scalar gate and its rejection). A scalar element's width lives in
/// the returned `IrType`; it drives both the C++ storage type and the
/// packed-bit layout. A widthless integer spelling (`uint`/`UInt`/`bits`
/// or `sint`/`SInt`) is normalized to its
/// language-defined 64-bit host ABI here, keeping it distinct from an enum's
/// deliberately layout-less `SInt(None)`; builtin `int` carries its specified
/// 32-bit packed width while retaining the same `uint64_t` member carrier v1
/// emits. A record
/// element (`Vec<Record, N>`, v1's `std::array<Inner, N>` member)
/// returns `IrType::Record(rid)`; its layout recurses through the
/// element record's own schema.
fn fixed_vec_field(
    t: &TypeExpr,
    enum_names: &std::collections::HashSet<String>,
    record_ids: &HashMap<String, RecordId>,
) -> Option<(IrType, usize)> {
    let TypeExpr::Builtin {
        name: BuiltinTy::Vec,
        args,
        ..
    } = t
    else {
        return None;
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
    // `Vec<Record, N>`: a fixed array of a nested transaction/struct.
    // The element record's own lowering already validated every leaf
    // (an unsupported leaf failed THERE, naming its `Inner.field` path),
    // so a resolved record id is sufficient here. Checked before the
    // scalar mapping — record names are not scalars. NOTE the parser's
    // type-arg heuristic: a bare named element (`Vec<Entry, 4>`) parses
    // as `TypeArg::Expr(Ident)` (only builtin heads parse as
    // `TypeArg::Type`), so a record element is matched in BOTH shapes.
    let elem = match args.first()? {
        TypeArg::Type(ty) => {
            if let Some(rid) = named_record_id(ty, record_ids) {
                return Some((IrType::Record(rid), len));
            }
            ty
        }
        TypeArg::Expr(e) => {
            if let ExprKind::Ident(id) = &*e.kind {
                if let Some(&rid) = record_ids.get(id.name.as_str()) {
                    return Some((IrType::Record(rid), len));
                }
            }
            return None;
        }
        _ => return None,
    };
    let elem_ty = field_ir_type(elem, enum_names)?;
    match elem_ty {
        IrType::UInt(_) | IrType::SInt(_) | IrType::Bool => Some((elem_ty, len)),
        _ => None,
    }
}

/// A zero-width scalar (`uint<0>`) in a field's own type, or under a
/// `Vec` element. Those are the shapes whose width arithmetic v1 gets
/// wrong; nothing else here belongs to this arm, and the first version
/// reached much further:
///
/// | field | v1 | |
/// |---|---|---|
/// | `uint<0>`, `sint<0>`, `bits<0>` | a full-width member packed at width 0 (a DEBUG build panics in `emit_unpack_bits`; release does not) | `SilentlyMisLowers` |
/// | `Vec<uint<0>, 4>` | the same, per element | `SilentlyMisLowers` |
/// | `list<uint<0>>`, `queue<uint<0>>`, `event<uint<0>>` | `std::vector<uint64_t>` / `uint64_t`, and it COMPILES | not this arm's business |
/// | `uint<0x0>`, `uint<0b0>` | `uint64_t data = 0;`, and it COMPILES | likewise |
///
/// The `list`/`queue`/`event` rows were `Invalid` because this recursed
/// through every `TypeArg::Type`, including a payload argument v1 never
/// reads a width from. The hex and binary rows were `Invalid` because
/// this read the width with `parse_int_literal_expr`, which understands
/// `0x`/`0b`, while v1's `int_width_from_args` and TB-IR's own
/// `field_ir_type` both do a plain decimal `parse::<u32>()` — so `0x0`
/// is not a width to either of them and both fall back to 64 bits. The
/// rule was already written down twice and got reconstructed a third
/// time; it now reads through `declared_scalar_width`, which IS
/// `field_ir_type`'s reader.
///
/// Only the WIDTH slot counts: `Vec<uint<8>, 0>` is a zero-LENGTH
/// array, which v1 emits as `std::array<uint64_t, 0>` and g++ accepts.
fn zero_width_leaf(ty: &TypeExpr) -> bool {
    match ty {
        // A named generic list (`list<T>`) carries no width slot, and v1
        // does not panic on a zero-width payload under one.
        TypeExpr::Named { .. } => false,
        TypeExpr::Builtin { name, args, .. } => {
            if matches!(name, BuiltinTy::Vec) {
                // `Vec`'s first argument is its element; the second is a
                // length, not a width.
                return matches!(args.first(), Some(TypeArg::Type(elem)) if zero_width_leaf(elem));
            }
            // A `queue<T>` / `event<T>` payload is not a width slot
            // either, so only this type's own first argument is read.
            declared_scalar_width(args) == Some(0)
        }
    }
}

/// The declared bit width in a scalar builtin's type arguments, read the
/// way BOTH backends read it: a plain decimal integer literal with
/// underscores stripped. v1's `int_width_from_args` / `type_arg_width`
/// do exactly this, so `uint<0x0>` has no width to either backend and
/// falls back to 64 bits rather than being a zero-width type.
fn declared_scalar_width(args: &[TypeArg]) -> Option<u32> {
    match args.first() {
        Some(TypeArg::Expr(e)) => match &*e.kind {
            ExprKind::Int(s) => s.replace('_', "").parse::<u32>().ok(),
            _ => None,
        },
        _ => None,
    }
}

/// Scalar field-type mapping, mirroring v1's `txn_field_c_type` C-type
/// choices: uint/bits/int → unsigned, sint → signed, bool/bit →
/// bool. `None` for anything this slice does not lower (nested records,
/// enums, lists, vecs), and for
/// a width this compiler cannot read as a plain decimal — the same
/// reader `zero_width_leaf` uses, so the two cannot drift.
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
        Some(TypeArg::Expr(_)) => Some(declared_scalar_width(args)?),
        Some(_) => return None,
        None => None,
    };
    if width == Some(0) {
        return None;
    }
    match name {
        BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits => {
            Some(IrType::UInt(Some(width.unwrap_or(64))))
        }
        // v1 stores builtin `int` in the same uint64_t carrier as other
        // narrow unsigned fields, but its packed language width is 32 bits.
        BuiltinTy::Int => Some(IrType::UInt(Some(32))),
        BuiltinTy::SInt | BuiltinTy::SIntCap => Some(IrType::SInt(Some(width.unwrap_or(64)))),
        BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => Some(IrType::Bool),
        _ => None,
    }
}

/// `Some(rid)` when `t` is a bare `Named` type whose simple name is a known
/// transaction or struct (a nested-record field type). Enum names live in a
/// disjoint namespace, so a match here is unambiguously a record.
///
/// Resolution is by LAST segment, so a qualified spelling (`Pkg.Beat`)
/// names the same record as the bare one — the rule `tlm_ret_record_id`
/// and `field_ir_type` already use, and the reason this is shared rather
/// than restated at each caller.
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
    fn visit(i: usize, records: &[RecordSchema], color: &mut [Color]) -> Result<(), LowerError> {
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
