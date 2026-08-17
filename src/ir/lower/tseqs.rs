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
    not_implemented, unsupported, FuncBuilder, LowerCtx, LowerError, SideTables, V1Status,
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

/// The scalar element `IrType` of a `tseq`'s `-> TSeq<scalar>` return when
/// the inner type is a primitive builtin (`uint<N>`/`sint<N>`/`bool`/…).
/// `None` for a non-builtin inner (record element) or a non-`TSeq` return.
fn tseq_scalar_element(decl: &TseqDecl) -> Option<IrType> {
    let args = tseq_args(decl)?;
    let TypeArg::Type(inner) = args.first()? else {
        return None;
    };
    match ir_type_of(Some(inner)) {
        // `ir_type_of` returns `Unknown` for a `Named` (record) inner —
        // those are handled by the record-element path, not here.
        ty @ (IrType::UInt(_) | IrType::SInt(_) | IrType::Bool) => Some(ty),
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

/// Build the `tseq name → element type` map, validating each declaration
/// up front. A `tseq`'s element is either a declared record (`TSeq<Record>`,
/// → `TseqElem::Record`) or a primitive scalar (`TSeq<uint<N>>`, →
/// `TseqElem::Scalar`); both render as `std::vector<T>`. A `tseq` with no
/// `TSeq<...>` return, or a `TSeq<Name>` whose `Name` is not a declared
/// record, is an explicit `Unsupported` — rejected here, never mis-lowered.
pub(crate) fn collect_tseq_records(
    file: &SourceFile,
    record_ids: &HashMap<String, RecordId>,
) -> Result<HashMap<String, TseqElem>, LowerError> {
    let mut out = HashMap::new();
    for it in &file.items {
        let Item::Tseq(decl) = it else { continue };
        let elem = if let Some(scalar) = tseq_scalar_element(decl) {
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
                        "only declared `transaction`/`struct` records and primitive scalars \
                         (`uint<N>`/`sint<N>`/`bool`) are lowered as tseq element types; v1 \
                         emits the name verbatim as `std::vector<{name}>`, which does not \
                         compile"
                    ),
                    V1Status::EmitsUncompilable,
                ));
            };
            TseqElem::Record(rid)
        } else {
            return Err(unsupported(
                &format!(
                    "`tseq {}` without a `-> TSeq<T>` return type",
                    decl.name.name
                ),
                "a tseq must yield a declared `transaction`/`struct` record or a primitive \
                 scalar (`uint<N>`/`sint<N>`/`bool`) element type",
            ));
        };
        if out.insert(decl.name.name.clone(), elem).is_some() {
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
    // Params first, so locals 0..nparams mirror them.
    let mut params = Vec::with_capacity(decl.params.len());
    for p in &decl.params {
        let ty = ir_type_of(p.ty.as_ref());
        let local = b.declare(&p.name.name);
        b.set_local_type(local, ty.clone());
        params.push(TypedParam {
            name: p.name.name.clone(),
            ty,
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
