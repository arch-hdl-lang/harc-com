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
//! - one `TbFunction` (`FunctionKind::Tseq { record }`) whose `ret` slot
//!   is an `IrType::RecordSeq(record)` accumulator. `yield t` is a
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
//! Out of subset (precise rejection, never mis-lowered): a tseq whose
//! `TSeq<T>` element `T` is not a declared `transaction`/`struct`
//! record (the IR's sequence element model is a value-record), a tseq
//! body using a construct the test-body lowering does not support, and
//! `yield` outside a tseq body.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::ast::{BuiltinTy, ExprKind, SourceFile, Item, TseqDecl, TypeArg, TypeExpr};
use crate::ir::{
    ConstraintSite, FunctionId, FunctionKind, IrType, RecordId, TbFunction, Terminator, TypedParam,
};

use super::helpers::{ir_type_of, HelperRegistry};
use super::{unsupported, FuncBuilder, LowerCtx, LowerError};

/// The element record name of a `tseq`'s `-> TSeq<Record>` return type,
/// or `None` when the declaration is not a record-element sequence
/// (no return type, a non-`TSeq` return, or a `TSeq<scalar>`).
pub(crate) fn tseq_element_name(decl: &TseqDecl) -> Option<String> {
    let TypeExpr::Builtin {
        name: BuiltinTy::TSeq,
        args,
        ..
    } = decl.return_ty.as_ref()?
    else {
        return None;
    };
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

/// Build the `tseq name → element RecordId` map, validating each
/// declaration up front. A `tseq` whose element type is not a declared
/// record is an explicit `Unsupported` (the IR sequence-element model is
/// a value-record), so it is rejected here rather than dropped or
/// mis-lowered.
pub(crate) fn collect_tseq_records(
    file: &SourceFile,
    record_ids: &HashMap<String, RecordId>,
) -> Result<HashMap<String, RecordId>, LowerError> {
    let mut out = HashMap::new();
    for it in &file.items {
        let Item::Tseq(decl) = it else { continue };
        let Some(elem) = tseq_element_name(decl) else {
            return Err(unsupported(
                &format!("`tseq {}` without a `-> TSeq<Record>` return type", decl.name.name),
                "a tseq must yield a declared `transaction`/`struct` record element type",
            ));
        };
        let Some(&rid) = record_ids.get(&elem) else {
            return Err(unsupported(
                &format!("`tseq {}` element type `{elem}`", decl.name.name),
                "only declared `transaction`/`struct` record element types are lowered",
            ));
        };
        if out.insert(decl.name.name.clone(), rid).is_some() {
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
/// `ret` slot is the `RecordSeq` accumulator that `yield`/`SeqPush`
/// appends to and `Terminator::Return` returns.
pub(crate) fn lower_tseq<'a>(
    id: FunctionId,
    decl: &TseqDecl,
    record: RecordId,
    ctx: &'a LowerCtx,
    helpers: &'a HelperRegistry<'a>,
    constraint_sites: &'a RefCell<Vec<ConstraintSite>>,
) -> Result<TbFunction, LowerError> {
    let mut b = FuncBuilder::new(ctx, helpers, constraint_sites);
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
    // `RecordSeq` so `yield`/`SeqPush` and the backend's
    // `std::vector<Record>` declaration both resolve, and so the verifier
    // treats it as live from entry (always default-constructed).
    let acc = b.declare("__result");
    b.set_local_type(acc, IrType::RecordSeq(record));
    b.set_tseq_result(acc);

    b.lower_block_stmts(&decl.body)?;
    if !b.is_terminated() {
        b.terminate(Terminator::Return);
    }
    // `yield` outside any tseq would have errored at lowering; here every
    // SeqPush is anchored to `acc`. The terminator returns `acc` (the
    // backend emits `return __result;` for a Tseq function).
    let mut f = b.finish(id, decl.name.name.clone(), FunctionKind::Tseq { record }, None)?;
    f.params = params;
    f.ret = Some(acc);
    Ok(f)
}
