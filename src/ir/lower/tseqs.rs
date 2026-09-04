//! `tseq` (transaction-sequence) support.
//!
//! A `tseq` is a named generator of a sequence of transaction values:
//!
//! ```text
//! tseq RandomTxns(n: int) -> TSeq<RegOp>
//!     for _ in 0 .. n
//!         let t : RegOp
//!         randomize(t)
//!         yield t
//!     end for
//! end tseq RandomTxns
//! ```
//!
//! v1 (`cpp_tb::emit_tseq`) lowers it to a `[&]`-capturing lambda that
//! fills a `std::vector<T> _result` via `yield t` (`_result.push_back(t)`)
//! and returns it. A `for t in <TSeq>` then range-iterates that vector.
//!
//! In the TB-IR this maps to:
//!
//! - one `TbFunction` (`FunctionKind::Tseq { elem }`) whose `ret` slot
//!   is an `IrType::RecordSeq`/`IrType::Seq` accumulator (per element).
//!   `yield t` is a
//!   `Stmt::SeqPush { seq: ret, value: t }`; `randomize(t)` reuses the
//!   already-merged constraint-IR seam (`Terminator::Randomize`) exactly
//!   as in a test body — the solver problem table already catalogs tseq
//!   randomize sites (`problem_table::collect_tseq_randomize_sites`).
//! - a `CallTarget::Tseq(name)` for `let txns = RandomTxns(5)`, typing
//!   the local `RecordSeq` (see `FuncBuilder::lower_let`).
//! - a counted-loop lowering for `for t in <seq local>` (see
//!   `FuncBuilder::lower_for`), binding `t` to `seq[i]` per iteration
//!   (`Expr::SeqIndex`) over `0 .. seq.size()` (`Expr::SeqLen`).
//!
//! A `TSeq<scalar>` element (`TSeq<uint<N>>`/`TSeq<sint<N>>`/`TSeq<bool>`)
//! lowers the same way with an `IrType::Seq(scalar)` accumulator — `yield e`
//! pushes a scalar value, `for x in <seq>` binds `x` to a scalar.
//!
//! Out of subset (precise rejection, never mis-lowered): a tseq whose
//! `TSeq<T>` element `T` is neither a declared `transaction`/`struct`
//! record nor a primitive scalar, a tseq body using a construct the
//! test-body lowering does not support, and `yield` outside a tseq body.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::ast::{BuiltinTy, ExprKind, Item, SourceFile, TseqDecl, TypeArg, TypeExpr};
use crate::ir::{
    FunctionId, FunctionKind, IrType, RecordId, TbFunction, Terminator, TseqElem, TypedParam,
};

use super::helpers::{ir_type_of, HelperRegistry};
use super::{
    not_implemented, unsupported, FuncBuilder, LowerCtx, LowerDiagnosticRecorder, LowerError,
    SideTables, V1Status,
};

/// The element name of a `tseq`'s `-> TSeq<T>` return type when `T` is a
/// single identifier (record element). `None` for a missing/non-`TSeq`
/// return, a `TSeq<scalar>` (a `Builtin` inner type), or a named generic.
fn tseq_element_name(decl: &TseqDecl) -> Option<String> {
    let args = tseq_args(decl)?;
    match args.first()? {
        TypeArg::Type(inner) => match inner {
            TypeExpr::Named { name, .. } => name.segments.last().map(|s| s.name.clone()),
            _ => None,
        },
        // `TSeq<MyTxn>` parses the single-identifier element as Expr(Ident).
        TypeArg::Expr(e) => match &*e.kind {
            ExprKind::Ident(id) => Some(id.name.clone()),
            _ => None,
        },
        // Named generics (`#(...)`) are not a tseq element type.
        TypeArg::Named { .. } => None,
    }
}

/// The value element `IrType` of a `tseq`'s `-> TSeq<T>` return when the
/// inner type is a primitive scalar or scalar-leaf fixed vector. `None` for a named
/// record element or a non-`TSeq` return.
fn tseq_scalar_element(
    decl: &TseqDecl,
    record_ids: &HashMap<String, RecordId>,
) -> Option<IrType> {
    let args = tseq_args(decl)?;
    let TypeArg::Type(inner) = args.first()? else {
        return None;
    };
    if let Some(fixed) =
        super::components::fixed_vec_ir_type_with_records(inner, record_ids)
    {
        return Some(fixed);
    }
    if let TypeExpr::Builtin { name, .. } = inner {
        match name {
            // v1's tseq return renderer maps both to `uint64_t` storage.
            BuiltinTy::Int => return Some(IrType::UInt(Some(32))),
            BuiltinTy::Time => return Some(IrType::UInt(Some(64))),
            _ => {}
        }
    }
    match ir_type_of(Some(inner)) {
        // `ir_type_of` returns `Unknown` for a `Named` (record) inner —
        // those are handled by the record-element path, not here.
        ty @ (IrType::UInt(_) | IrType::SInt(_) | IrType::Bool | IrType::FixedVec { .. }) => {
            Some(ty)
        }
        _ => None,
    }
}

/// The `args` list of a well-formed `-> TSeq<...>` return type, or `None`.
fn tseq_args(decl: &TseqDecl) -> Option<&[TypeArg]> {
    match decl.return_ty.as_ref()? {
        TypeExpr::Builtin {
            name: BuiltinTy::TSeq,
            args,
            ..
        } => Some(args),
        _ => None,
    }
}

/// tseq name -> (element type, declared parameter NAMES, declared
/// parameter TYPES). Named because the triple is threaded through
/// `LowerCtx` and three lowering entry points.
pub(crate) type TseqTable = HashMap<String, (FunctionId, TseqElem, Vec<String>, Vec<IrType>)>;

/// Build the `tseq name → element type` map, validating each declaration
/// up front. A `tseq`'s element is either a declared record (`TSeq<Record>`,
/// → `TseqElem::Record`) or a supported value (`TSeq<uint<N>>` /
/// `TSeq<Vec<T, N>>`, → `TseqElem::Scalar`); both render as
/// `std::vector<T>`.
///
/// A `tseq` with NO return type at all defaults its element to a signed
/// 64-bit scalar, matching v1's `std::vector<int64_t>`. A `TSeq<Name>`
/// whose `Name` is not a declared record, and any other present-but-
/// unusable annotation, is rejected — absent and invalid are different
/// input classes and must not share an arm.
pub(crate) fn collect_tseq_records(
    file: &SourceFile,
    record_ids: &HashMap<String, RecordId>,
    diagnostics: &LowerDiagnosticRecorder,
    first_function: FunctionId,
) -> Result<TseqTable, LowerError> {
    let mut out = HashMap::new();
    let mut next_function = first_function.0;
    for (item_index, it) in file.items.iter().enumerate() {
        let Item::Tseq(decl) = it else { continue };
        if let Some((param, span)) = decl.params.iter().find_map(|param| {
            super::helpers::nested_string_type_span(param.ty.as_ref()).map(|span| (param, span))
        }) {
            diagnostics.record(file.item_source(item_index), span);
            return Err(unsupported(
                &format!(
                    "tseq `{}` parameter `{}` whose type contains `String`",
                    decl.name.name, param.name.name
                ),
                "String containers and aggregates are not supported in transaction sequences",
            ));
        }
        if let Some(span) = super::helpers::nested_string_type_span(decl.return_ty.as_ref()) {
            diagnostics.record(file.item_source(item_index), span);
            return Err(unsupported(
                &format!("tseq `{}` return type containing `String`", decl.name.name),
                "String sequence elements and nested String containers are not supported",
            ));
        }
        let elem = if let Some(scalar) = tseq_scalar_element(decl, record_ids) {
            TseqElem::Scalar(scalar)
        } else if let Some(name) = tseq_element_name(decl) {
            let Some(&rid) = record_ids.get(&name) else {
                // v1 prints the element type straight into the lambda's
                // return type — `-> std::vector<NoSuchType>` — naming a
                // type nothing declares, so the translation unit does not
                // compile.
                //
                // NOT the same as the missing-return-type site below, four
                // lines down and easy to mistake for this one: an ABSENT
                // annotation makes v1 substitute a working default, while a
                // PRESENT but unresolvable one makes it emit the bad name.
                // Absent and invalid are different input classes even
                // though one code path handles both.
                return Err(not_implemented(
                    &format!("`tseq {}` element type `{name}`", decl.name.name),
                    format!(
                        "only declared `transaction`/`struct` records, primitive scalars \
                         (`uint<N>`/`sint<N>`/`bool`), and scalar-leaf fixed vectors are lowered as tseq \
                         element types; v1 \
                         emits the name verbatim as `std::vector<{name}>`, which does not \
                         compile"
                    ),
                    V1Status::EmitsUncompilable,
                ));
            };
            TseqElem::Record(rid)
        } else if decl.return_ty.is_none() {
            // No `-> TSeq<T>` at all: default the element to a signed
            // 64-bit scalar, which is exactly what v1 does — it emits
            // `-> std::vector<int64_t>` where an annotated tseq gets
            // `std::vector<uint64_t>`. That is a signedness difference,
            // not a failure, so the sequence runs correctly under v1 and
            // this was a gap to CLOSE rather than a diagnostic to
            // reclassify.
            //
            // Deliberately not merged with the bad-element-type arm
            // above: an ABSENT annotation is defaultable, a PRESENT but
            // unresolvable one is not (v1 prints the bad name into a
            // type position, which does not compile). Same code path,
            // different input classes.
            TseqElem::Scalar(IrType::SInt(Some(64)))
        } else {
            // A present NON-`TSeq` return such as `-> Req` or `-> uint<8>`
            // is malformed declaration shape. A residual `TSeq<...>` is
            // different: record-leaf vectors and other aggregate elements can
            // reach this arm and remain an honest typed-IR subset boundary.
            if tseq_args(decl).is_none() {
                return Err(LowerError::Invalid(format!(
                    "`tseq {}` return type must be `TSeq<T>` with a declared record, primitive scalar (`uint<N>`/`sint<N>`/`bool`), or scalar-leaf fixed-vector element",
                    decl.name.name
                )));
            }
            return Err(unsupported(
                &format!("`tseq {}` element type", decl.name.name),
                "a `-> TSeq<T>` must name a declared `transaction`/`struct` record, a \
                 primitive scalar (`uint<N>`/`sint<N>`/`bool`), or scalar-leaf fixed vector",
            ));
        };
        let param_names: Vec<String> = decl.params.iter().map(|p| p.name.name.clone()).collect();
        // Declared parameter TYPES, for the call-site slot check. A note
        // here once claimed every reachable tseq parameter is a scalar,
        // so the slot was hard-coded to "not a record". That was false
        // for `TSeq<T>`: `tseq Wrap(xs: TSeq<Beat>)` is compiled by v1
        // (`[&](const std::vector<Beat>& xs) -> std::vector<Beat>`) and
        // the hard-coded slot rejected the correct call while letting
        // `Wrap(7)` — which v1 refuses — straight through.
        let param_tys: Vec<IrType> = decl
            .params
            .iter()
            .map(|p| super::helpers::slot_ir_type(p.ty.as_ref(), record_ids))
            .collect();
        let function = FunctionId(next_function);
        next_function += 1;
        if out
            .insert(
                decl.name.name.clone(),
                (function, elem, param_names, param_tys),
            )
            .is_some()
        {
            return Err(LowerError::Invalid(format!(
                "duplicate tseq declaration `{}`",
                decl.name.name
            )));
        }
    }
    Ok(out)
}

/// Lower one `tseq` declaration into a `TbFunction` (kind `Tseq`).
/// Locals `0..params.len()` mirror the params (verifier convention); the
/// `ret` slot is the `RecordSeq`/`Seq` accumulator (per `elem`) that
/// `yield`/`SeqPush` appends to and `Terminator::Return` returns.
pub(crate) fn lower_tseq<'a>(
    id: FunctionId,
    decl: &TseqDecl,
    elem: TseqElem,
    ctx: &'a LowerCtx,
    helpers: &'a HelperRegistry<'a>,
    side_tables: &'a RefCell<SideTables>,
) -> Result<TbFunction, LowerError> {
    let mut b = FuncBuilder::new(ctx, helpers, side_tables);
    let (_, _, _, param_tys) = ctx.tseqs.get(&decl.name.name).ok_or_else(|| {
        LowerError::Invalid(format!(
            "tseq `{}` is missing from the collected signature table",
            decl.name.name
        ))
    })?;
    if param_tys.len() != decl.params.len() {
        return Err(LowerError::Invalid(format!(
            "tseq `{}` has {} declared parameters but {} collected parameter types",
            decl.name.name,
            decl.params.len(),
            param_tys.len()
        )));
    }
    // Params first, so locals 0..nparams mirror them.
    let mut params = Vec::with_capacity(decl.params.len());
    for (p, ty) in decl.params.iter().zip(param_tys) {
        if super::helpers::is_string_tseq_type(p.ty.as_ref()) {
            b.record_error_span(p.span);
            return Err(LowerError::Invalid(format!(
                "tseq `{}` parameter `{}` cannot use `TSeq<String>`; String sequences are not supported",
                decl.name.name, p.name.name
            )));
        }
        if matches!(ty, IrType::String) {
            b.record_error_span(p.span);
            return Err(LowerError::Invalid(format!(
                "tseq `{}` parameter `{}` cannot use `String`; TSeq callables support numeric, boolean, record, and sequence parameters",
                decl.name.name, p.name.name
            )));
        }
        let local = b.declare(&p.name.name);
        b.set_local_type(local, ty.clone());
        params.push(TypedParam {
            name: p.name.name.clone(),
            ty: ty.clone(),
        });
    }
    // The sequence accumulator — the function's return value. Marked
    // `RecordSeq`/`Seq` (per element) so `yield`/`SeqPush` and the
    // backend's `std::vector<T>` declaration both resolve, and so the
    // verifier treats it as live from entry (always default-constructed).
    let acc = b.declare("__result");
    b.set_local_type(acc, elem.seq_type());
    b.set_tseq_result(acc);

    b.lower_block_stmts(&decl.body)?;
    if !b.is_terminated() {
        b.terminate(Terminator::Return);
    }
    // `yield` outside any tseq would have errored at lowering; here every
    // SeqPush is anchored to `acc`. The terminator returns `acc` (the
    // backend emits `return __result;` for a Tseq function).
    let mut f = b.finish(
        id,
        decl.name.name.clone(),
        FunctionKind::Tseq { elem },
        None,
    )?;
    f.params = params;
    f.ret = Some(acc);
    Ok(f)
}
