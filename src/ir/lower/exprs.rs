//! Expression lowering. Tree-shaped, no flattening; `Expr::Port` nodes
//! survive only in port-allowed positions (wait predicates, format
//! args, DutRead/DutWrite operands, assert conditions) — everywhere
//! else `lower_expr_no_ports` hoists DUT reads into `DutRead` temps.

use super::{not_implemented, unsupported, FuncBuilder, LowerError, V1Status};
use crate::ast::{
    BinaryOp, BuiltinTy, CallArg, Expr as AstExpr, ExprKind, TypeArg, TypeExpr, UnaryOp,
};
use crate::ir::{
    BinOp, Expr, IrType, LocalId, PortAccess, PortRef, RecordId, Stmt, UnOp, WidthCastKind,
};

pub(super) fn common_expr_type(lhs: Option<IrType>, rhs: Option<IrType>) -> Option<IrType> {
    let (lhs, rhs) = match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => (lhs, rhs),
        (lhs, rhs) => return lhs.or(rhs),
    };
    if lhs == rhs {
        return Some(lhs);
    }
    if matches!(lhs, IrType::Unknown) {
        return Some(rhs);
    }
    if matches!(rhs, IrType::Unknown) {
        return Some(lhs);
    }
    let scalar = |ty: &IrType| match ty {
        IrType::UInt(width) => Some((*width, false)),
        IrType::SInt(width) => Some((*width, true)),
        IrType::Bool => Some((Some(1), false)),
        _ => None,
    };
    let (Some((lw, ls)), Some((rw, rs))) = (scalar(&lhs), scalar(&rhs)) else {
        return Some(lhs);
    };
    // Widthless integers use a 64-bit host ABI. Identical widthless types
    // returned above retain their source-level `None`; once composed with a
    // different scalar, their concrete contribution is 64 bits.
    let width = Some(lw.unwrap_or(64).max(rw.unwrap_or(64)));
    Some(if ls && rs {
        IrType::SInt(width)
    } else {
        IrType::UInt(width)
    })
}

fn narrowest_scalar_type(lhs: Option<IrType>, rhs: Option<IrType>) -> Option<IrType> {
    let scalar = |ty: IrType| match ty {
        IrType::UInt(width) => Some((width, false)),
        IrType::SInt(width) => Some((width, true)),
        IrType::Bool => Some((Some(1), false)),
        IrType::Unknown => None,
        _ => None,
    };
    let (lhs, rhs) = (lhs.and_then(&scalar), rhs.and_then(scalar));
    let (width, signed) = match (lhs, rhs) {
        (Some((lw, ls)), Some((rw, rs))) => {
            let (lhs_width, rhs_width) = (lw.unwrap_or(64), rw.unwrap_or(64));
            let selected = if lhs_width < rhs_width && !ls {
                lhs_width
            } else if rhs_width < lhs_width && !rs {
                rhs_width
            } else {
                lhs_width.max(rhs_width)
            };
            let width = if lw.is_none() && rw.is_none() {
                None
            } else {
                Some(selected)
            };
            (width, ls && rs)
        }
        (Some(value), None) | (None, Some(value)) => value,
        (None, None) => return None,
    };
    Some(if signed {
        IrType::SInt(width)
    } else {
        IrType::UInt(width)
    })
}

/// Resolution of a (possibly nested) record field-access chain
/// `ident.f1.f2...fn` rooted at a record-typed local. `field` is the
/// first-level field (`f1`) on the local's record; `path` is the further
/// nested field names (`f2..fn`); the leaf is the last of `[field] ++ path`.
pub(crate) struct RecordFieldChain {
    pub local: LocalId,
    pub field: String,
    pub path: Vec<String>,
    /// Element selections already present in the target chain. A non-leaf
    /// entry traverses one `Vec<Record, N>` element; entries at the leaf
    /// consume outer layers of a nested fixed vector before the caller adds
    /// the final index.
    pub mid_indices: Vec<(usize, Expr)>,
    /// `Some(N)` when the leaf is a `Vec<T, N>` field.
    pub leaf_vec_len: Option<usize>,
    /// The leaf field's element/scalar/record type.
    pub leaf_ty: IrType,
    /// Dotted `Rec.f1.f2` spelling — names the FIELD (record type +
    /// path), which is what a construct name wants.
    pub dotted: String,
    /// The path as the user WROTE it (`r.f1.f2`), for a suggestion.
    /// `dotted` cannot serve: `Rec.f1[i]` is not an expression anyone
    /// can type, and a detail that says "index the field element-wise
    /// (`Rec.data[i]`)" hands back something that does not parse.
    pub spelled: String,
}

/// Resolution of a subfield access onto a bound-to target responder's
/// whole-record state field (`last.addr` / `responder.last.addr`).
/// `instance` is the bound testbench-field instance (empty placeholder in
/// a responder body, filled at test-binding); `field` is the record state
/// field; `path` is the nested subfield chain (length ≥ 1).
pub(crate) struct TransactorStateRecordChain {
    pub instance: String,
    pub field: String,
    pub path: Vec<String>,
    pub mid_indices: Vec<(usize, Expr)>,
    pub leaf_index: Option<Expr>,
    /// `Some(N)` when the leaf is a `Vec<T, N>` field. Only the
    /// `==`/`!=` landing reads one; see `vec_read_ok`.
    pub leaf_vec_len: Option<usize>,
    /// The leaf field's element/scalar/record type — the other half of
    /// the shape the equality pairing has to match.
    pub leaf_ty: IrType,
}

/// A fully-selected direct fixed-vector state element (`lanes[i]` or
/// `matrix[i][j]`). Outer selections use the existing empty-path
/// `mid_indices` representation; `index` is the final selection.
pub(crate) struct TransactorStateVecElement {
    pub instance: String,
    pub field: String,
    pub mid_indices: Vec<(usize, Expr)>,
    pub index: Expr,
    pub elem_ty: IrType,
}

pub(crate) struct IndexedComponentRecordChain {
    pub base: crate::ir::ComponentBase,
    pub field: String,
    pub index_pos: usize,
    pub index: Expr,
    pub leaf_ty: IrType,
}

/// Value type after selecting `selections` fixed-vector layers from a record
/// field. The outer layer is split across `vec_len` and `ty`; further layers
/// are recursive `IrType::FixedVec` values.
fn selected_record_leaf_type(
    vec_len: Option<usize>,
    ty: &IrType,
    selections: usize,
) -> Option<IrType> {
    if selections == 0 {
        return vec_len.is_none().then(|| ty.clone());
    }
    vec_len?;
    let mut selected = ty.clone();
    for _ in 1..selections {
        selected = match selected {
            IrType::FixedVec { elem, .. } => *elem,
            _ => return None,
        };
    }
    Some(selected)
}

pub(crate) fn dynamic_record_list_value(path: &str) -> LowerError {
    not_implemented(
        &format!("a dynamic record list `{path}` used as an ordinary scalar value"),
        "use the list only as a whole-list copy/equality operand, or query it with --codegen v1",
        V1Status::EmitsUncompilable,
    )
}

pub(crate) fn dynamic_record_list_index(path: &str) -> LowerError {
    not_implemented(
        &format!("indexing dynamic record list `{path}` in an ordinary body"),
        "record lists are default-constructed empty; unchecked v1 indexing can access outside the list",
        V1Status::SilentlyMisLowers,
    )
}

/// Flatten a record-shaped path into its root, declaration-order field
/// segments, and the index attached to each segment. The index position is
/// relative to `segs` (not including the root).
fn indexed_path_parts<'a>(
    e: &'a AstExpr,
) -> Option<(
    &'a crate::ast::Ident,
    Vec<String>,
    Vec<(usize, &'a AstExpr)>,
)> {
    let mut segs = Vec::new();
    let mut raw_indices = Vec::new();
    // Indexes are collected outer-to-inner while walking the AST, then
    // attached (inner-to-outer) to the next field segment. Keeping every
    // pending index is what makes `rec.matrix[i][j]` distinguishable from
    // an unrelated indexed expression.
    let mut pending_indices = Vec::new();
    let mut cur = e;
    let root = loop {
        match &*cur.kind {
            ExprKind::Field { target, name } => {
                for idx in pending_indices.drain(..).rev() {
                    raw_indices.push((segs.len(), idx));
                }
                segs.push(name.name.clone());
                cur = target;
            }
            ExprKind::Index { target, index } => {
                pending_indices.push(index);
                cur = target;
            }
            ExprKind::Ident(root) if pending_indices.is_empty() => break root,
            _ => return None,
        }
    };
    if segs.is_empty() {
        return None;
    }
    let total = segs.len();
    segs.reverse();
    let mut indices = Vec::with_capacity(raw_indices.len());
    for (raw_pos, idx) in raw_indices {
        indices.push((total - 1 - raw_pos, idx));
    }
    indices.sort_by_key(|(position, _)| *position);
    Some((root, segs, indices))
}

fn record_path_index_is_static_literal(e: &AstExpr) -> bool {
    let mut e = e;
    while let ExprKind::Paren(inner) = &*e.kind {
        e = inner;
    }
    match &*e.kind {
        ExprKind::Int(text) => {
            parse_int_literal(text).is_some()
                || parse_sized_int_literal(text).is_some()
                || sized_int_literal_overflows_u64(text)
                || matches!(
                    parse_int_literal_checked(text),
                    Err(IntLiteralErr::Overflows)
                )
        }
        ExprKind::Unary {
            op: UnaryOp::Neg,
            expr,
        } => record_path_index_is_static_literal(expr),
        _ => false,
    }
}

impl super::FuncBuilder<'_> {
    /// Lower an index carried by a record path. Sized integer literals are
    /// already validated by the parser and are side-effect-free, so retain
    /// their declared width here without enabling them in general value
    /// positions (where sized-literal inference remains a separate gap).
    fn lower_record_path_index(&mut self, e: &AstExpr) -> Result<Expr, LowerError> {
        let mut terminal = e;
        let mut negative = false;
        loop {
            match &*terminal.kind {
                ExprKind::Paren(inner) => terminal = inner,
                ExprKind::Unary {
                    op: UnaryOp::Neg,
                    expr,
                } => {
                    negative = !negative;
                    terminal = expr;
                }
                _ => break,
            }
        }
        if let ExprKind::Int(text) = &*terminal.kind {
            let fitting = parse_int_literal(text)
                .map(|value| (None, value))
                .or_else(|| {
                    parse_sized_int_literal_with_width(text)
                        .map(|(width, value)| (Some(width), value))
                });
            if let Some((width, value)) = fitting {
                if negative && value != 0 {
                    return Err(LowerError::Invalid(format!(
                        "negative record `Vec` index ending in `{text}` is out of bounds"
                    )));
                }
                return Ok(Expr::Literal {
                    value,
                    ty: width.map_or(IrType::Unknown, |width| IrType::UInt(Some(width))),
                });
            }
            if sized_int_literal_overflows_u64(text)
                || matches!(
                    parse_int_literal_checked(text),
                    Err(IntLiteralErr::Overflows)
                )
            {
                return Err(LowerError::Invalid(format!(
                    "record `Vec` index `{text}` exceeds the supported host index range and is out of bounds"
                )));
            }
        }
        self.lower_expr(e)
    }

    pub(crate) fn as_indexed_component_record_field(
        &mut self,
        e: &AstExpr,
    ) -> Result<Option<IndexedComponentRecordChain>, LowerError> {
        let Some((root, segs, raw_indices)) = indexed_path_parts(e) else {
            return Ok(None);
        };
        if raw_indices.len() != 1
            || raw_indices[0].0 + 1 == segs.len()
            || self.lookup(&root.name).is_some()
        {
            return Ok(None);
        }
        let mut full: Vec<String> = std::iter::once(root.name.clone()).chain(segs).collect();
        let mut indexed_full_pos = raw_indices[0].0 + 1;
        if self.ctx.tb_field.as_deref() == Some(root.name.as_str()) {
            full.remove(0);
            indexed_full_pos = indexed_full_pos.saturating_sub(1);
        }

        let resolve_record_path = |this: &Self,
                                   record: RecordId,
                                   suffix: &[String],
                                   index_pos: usize|
         -> Result<(IrType, usize), LowerError> {
            let mut rid = record;
            let mut selected_len = None;
            for (pos, seg) in suffix.iter().enumerate() {
                let schema = &this.ctx.records[rid.index()];
                let fld = schema.field(seg).ok_or_else(|| {
                    LowerError::Invalid(format!("record `{}` has no field `{seg}`", schema.name))
                })?;
                let indexed = pos == index_pos;
                if indexed {
                    selected_len = Some(fld.vec_len.ok_or_else(|| {
                        not_implemented(
                            &format!(
                                "indexing the non-`Vec` record field `{}.{seg}`",
                                schema.name
                            ),
                            "only `Vec<T, N>` record fields are indexable",
                            V1Status::EmitsUncompilable,
                        )
                    })?);
                }
                if pos + 1 == suffix.len() {
                    let len = selected_len.ok_or_else(|| {
                        LowerError::Invalid("indexed component path has no selection".into())
                    })?;
                    return Ok((fld.ty.clone(), len));
                }
                match fld.ty {
                    IrType::Record(next) if fld.vec_len.is_none() && !indexed => rid = next,
                    IrType::Record(next) if fld.vec_len.is_some() && indexed => rid = next,
                    IrType::Record(_) if fld.vec_len.is_some() => {
                        return Err(not_implemented(
                            &format!(
                                "traversing the `Vec` record field `{}.{seg}` without an element index",
                                schema.name
                            ),
                            format!("select one element first (`{seg}[i]`)"),
                            V1Status::EmitsUncompilable,
                        ));
                    }
                    _ => {
                        return Err(not_implemented(
                            &format!(
                                "field access on `{}.{seg}`, which is not a nested record",
                                schema.name
                            ),
                            "only nested struct/transaction fields can be traversed further",
                            V1Status::EmitsUncompilable,
                        ));
                    }
                }
            }
            Err(LowerError::Invalid(
                "indexed component record path has no field".into(),
            ))
        };

        if let Some(cid) = self.self_component {
            if let Some(schema) = self.ctx.components[cid.index()].field(&full[0]) {
                if let crate::ir::ComponentFieldKind::Record { record } = schema.kind {
                    let suffix = &full[1..];
                    if indexed_full_pos >= 1 && indexed_full_pos < full.len() {
                        let index_pos = indexed_full_pos;
                        let (leaf_ty, len) =
                            resolve_record_path(self, record, suffix, index_pos - 1)?;
                        let index = self.lower_record_path_index(raw_indices[0].1)?;
                        check_literal_vec_index_bounds(&full.join("."), &index, len)?;
                        return Ok(Some(IndexedComponentRecordChain {
                            base: crate::ir::ComponentBase::SelfField,
                            field: full.join("."),
                            index_pos,
                            index,
                            leaf_ty,
                        }));
                    }
                }
            }
        }

        let Some((head_cid, mut base, tail, _)) = self.component_path_head(&full) else {
            return Ok(None);
        };
        for recv_len in 0..tail.len() {
            let Ok(cid) = self.resolve_component_recv(head_cid, &tail[..recv_len]) else {
                break;
            };
            let Some(schema) = self.ctx.components[cid.index()].field(&tail[recv_len]) else {
                continue;
            };
            let crate::ir::ComponentFieldKind::Record { record } = schema.kind else {
                continue;
            };
            let suffix = &tail[recv_len + 1..];
            let suffix_start = full.len() - tail.len() + recv_len;
            if indexed_full_pos <= suffix_start || indexed_full_pos >= full.len() {
                continue;
            }
            let index_pos = indexed_full_pos - suffix_start;
            let (leaf_ty, len) = resolve_record_path(self, record, suffix, index_pos - 1)?;
            base.extend_from_slice(&tail[..recv_len]);
            let index = self.lower_record_path_index(raw_indices[0].1)?;
            check_literal_vec_index_bounds(&full.join("."), &index, len)?;
            return Ok(Some(IndexedComponentRecordChain {
                base: crate::ir::ComponentBase::Path(base),
                field: tail[recv_len..].join("."),
                index_pos,
                index,
                leaf_ty,
            }));
        }
        Ok(None)
    }

    /// Whether `e` is the exact whole-vector operand/RHS currently granted
    /// access. Parentheses are transparent, but nested expressions have a
    /// different span and therefore cannot inherit the permission.
    pub(crate) fn whole_vec_read_allowed(&self, e: &crate::ast::Expr) -> bool {
        self.vec_read_ok && self.vec_read_span == Some(unparen_expr(e).span)
    }

    /// The `(len, element type)` of a whole-`Vec` record-field read, in
    /// whichever lane spells one — a record LOCAL
    /// (`r.data`), a bound responder's record STATE field (`t.ba.data`),
    /// a COMPONENT record field (`a.data` in an agent method), or a direct
    /// COMPONENT fixed-vector field (`table.words`). The
    /// lanes are tried in the order `lower_expr`'s `Field` arm resolves
    /// them, so the shape reported is the one that would be lowered.
    ///
    /// `None` for anything else, errors included: a regblock access like
    /// `regs.DMACR.RS` makes the record-local resolver fail, and
    /// propagating that turned a working program into a hard error
    /// before its own lowering path ever saw it. Whether an operand
    /// lowers at all is decided later, by whoever owns it.
    pub(crate) fn whole_vec_leaf(&mut self, e: &crate::ast::Expr) -> Option<(usize, String)> {
        // PURE DOTTED PATHS ONLY, checked before anything else, because
        // `try_record_field_chain` is not an oracle — it LOWERS every
        // mid-chain index it walks past, pushing statements and
        // allocating temps. Running it speculatively on both operands of
        // every `==` and discarding the result therefore duplicated any
        // side effect in an index: `r.kids[bump(dut)].p == 0` inlined
        // `bump` TWICE, driving the DUT twice and burning an extra clock
        // cycle, with no diagnostic and compiling C++ on both sides.
        // Measured at 4 emitted inlines against the parent commit's 2.
        //
        // `dotted_path` accepts only `Ident`/`Field`/`Paren`, so an
        // operand that passes it has no index for a resolver to lower
        // and every call below is inert. A `Vec` element selection in
        // the path (`r.tbl[0].data == s.tbl[1].data`) is refused by this
        // gate rather than permitted — v1 compiles that one, so it keeps
        // an honest `--codegen v1` suggestion, which is the right side to
        // err on against silently running a helper twice.
        super::components::dotted_path(e)?;
        // `dotted_path` sees through parentheses; two of the three
        // resolvers below do not. Peeling first keeps the answer a
        // property of the PATH rather than of how it was written, so
        // `(r.data) == s.data` pairs with `r.data == s.data` instead of
        // falling back to a refusal on a spelling difference.
        let mut e = e;
        while let ExprKind::Paren(inner) = &*e.kind {
            e = inner;
        }
        let (len, ty) = if let Ok(Some(c)) = self.as_transactor_state_record_field(e) {
            (c.leaf_vec_len?, c.leaf_ty)
        } else if let Ok(Some((_, _, vec))) = self.as_component_vec_field(e) {
            (vec.len, vec.elem)
        } else if let Ok(Some(rf)) = self.as_component_record_field(e) {
            rf.leaf_vec?
        } else if let Ok(Some(l)) = self.try_record_field_chain(e) {
            (l.leaf_vec_len?, l.leaf_ty)
        } else {
            return None;
        };
        // The C++ CLASS, not the `IrType`. Both backends declare the
        // member as `std::array<elem, N>` and collapse every unsigned
        // scalar of 64 bits or fewer to `uint64_t`, so
        // `Vec<uint<8>, 4> == Vec<uint<32>, 4>` compiles and compares
        // element-wise — v1 emits it and g++ accepts, measured. Pairing
        // on `IrType` equality refused that program and, because the
        // read refusal claims `EmitsUncompilable`, told the user no
        // backend runs it. The write arm one file over
        // (`stmts.rs`) had already written this rule down; it is called
        // from there now rather than restated.
        Some((len, crate::codegen::cpp_tb::ir_vec_elem_class(&ty)?))
    }

    /// The C++ element class of a whole dynamic-list record-field read.
    /// Resolution stays policy-free because whole-list copies and Eq/Ne
    /// are valid C++; scalar consumption, indexing, and queries make
    /// different v1-safety promises and diagnose at their own sinks.
    pub(crate) fn whole_seq_leaf(&mut self, e: &crate::ast::Expr) -> Option<String> {
        let dotted = super::components::dotted_path(e).is_some();
        if !dotted {
            let (_, _, indices) = indexed_path_parts(e)?;
            if indices
                .iter()
                .any(|(_, index)| !record_path_index_is_static_literal(index))
            {
                return None;
            }
        }
        let mut e = e;
        while let ExprKind::Paren(inner) = &*e.kind {
            e = inner;
        }
        let ty = if !dotted {
            if let Ok(Some(c)) = self.as_indexed_component_record_field(e) {
                c.leaf_ty
            } else if let Ok(Some(c)) = self.as_transactor_state_record_field(e) {
                c.leaf_ty
            } else if let Ok(Some(l)) = self.try_record_field_chain(e) {
                l.leaf_ty
            } else {
                return None;
            }
        } else if let Ok(Some(c)) = self.as_transactor_state_record_field(e) {
            c.leaf_ty
        } else if let Ok(Some(rf)) = self.as_component_record_field(e) {
            rf.leaf_ty
        } else if let Ok(Some(l)) = self.try_record_field_chain(e) {
            l.leaf_ty
        } else {
            return None;
        };
        let IrType::Seq(elem) = ty else {
            return None;
        };
        crate::codegen::cpp_tb::ir_vec_elem_class(&elem)
    }

    pub(crate) fn whole_seq_copy_rhs(
        &mut self,
        dst_elem_class: &str,
        value: &crate::ast::Expr,
    ) -> Result<Option<Expr>, LowerError> {
        if !path_expr(value) {
            return Ok(None);
        }
        let saved = self.vec_read_ok;
        let saved_span = self.vec_read_span;
        self.vec_read_ok = true;
        self.vec_read_span = Some(unparen_expr(value).span);
        let value = self.lower_expr_no_ports(value);
        self.vec_read_ok = saved;
        self.vec_read_span = saved_span;
        let value = value?;
        let is_matching_seq = matches!(self.expr_type(&value), Some(IrType::Seq(ref elem))
            if crate::codegen::cpp_tb::ir_vec_elem_class(elem).as_deref() == Some(dst_elem_class));
        Ok(is_matching_seq.then_some(value))
    }

    /// Lower the RHS of a whole-`Vec` field WRITE, or `None` when it is
    /// not a whole-`Vec` field read of the same shape as `dst_shape`.
    ///
    /// One helper for every write lane (record local, responder record
    /// state field, component record field), asking the same
    /// question `==`/`!=` asks: do the two fields render as the same
    /// `std::array<elem, N>` member? A copy between two that do is what
    /// v1 emits and g++ accepts; anything else it emits, g++ refuses.
    ///
    /// The RHS is lowered with the read permission ON, because a whole-
    /// `Vec` read is exactly what an admissible RHS is. Restoring the
    /// flag afterwards keeps the permission from reaching the next
    /// statement.
    pub(crate) fn whole_vec_copy_rhs(
        &mut self,
        dst_shape: (usize, String),
        value: &crate::ast::Expr,
    ) -> Result<Option<Expr>, LowerError> {
        let Some(e) = self.whole_vec_value_rhs(value)? else {
            return Ok(None);
        };
        if self.ir_whole_vec_shape(&e) != Some(dst_shape) {
            return Ok(None);
        }
        Ok(Some(e))
    }

    /// Lower one whole-vector value, retaining its exact `IrType` for ABI
    /// slot validation. Named paths use the guarded aggregate-read lane;
    /// pure helper calls are already typed aggregate expressions and may be
    /// composed directly into another helper argument or method return.
    pub(crate) fn whole_vec_value_rhs(
        &mut self,
        value: &crate::ast::Expr,
    ) -> Result<Option<Expr>, LowerError> {
        let is_path = path_expr(value);
        let helper_call_ty = self.fixed_vec_helper_call_type(value);
        if !is_path && helper_call_ty.is_none() {
            return Ok(None);
        }
        let e = if is_path {
            // The permission is granted only for a path, which has no other
            // sub-expression that could accidentally inherit it.
            let saved = self.vec_read_ok;
            let saved_span = self.vec_read_span;
            self.vec_read_ok = true;
            self.vec_read_span = Some(unparen_expr(value).span);
            let e = self.lower_expr_no_ports(value);
            self.vec_read_ok = saved;
            self.vec_read_span = saved_span;
            e?
        } else {
            // The helper-call lowering validates each aggregate argument
            // through this same function, so no broad vec-read permission is
            // needed around the composed call expression.
            self.lower_expr_no_ports(value)?
        };
        // The shape comes from the LOWERED expression, not the AST.
        // The read-side pairing has to ask the AST (both operands are
        // lowered again afterwards, so resolving one speculatively would
        // duplicate any side effect in an index) and therefore refuses
        // an indexed path outright. Here the RHS is lowered exactly
        // once, so `b.tbl[0].data` — which v1 copies happily — can be
        // asked about after the fact instead of being refused for its
        // spelling.
        Ok(Some(e))
    }

    /// A precise refusal for an indexed component-record or
    /// responder-state-record path that no typed resolver claimed —
    /// `Ok(())` when `e` is not one.
    ///
    /// The typed resolvers now carry supported mid-path and nested-vector
    /// selections. Reaching this fence means the remaining indexed shape is
    /// still outside their representable record/component paths; v1 remains
    /// the honest fallback for that residue.
    pub(crate) fn reject_indexed_component_record_path(
        &self,
        e: &crate::ast::Expr,
        what: &str,
    ) -> Result<(), LowerError> {
        let Some(segs) = path_segments(e) else {
            return Ok(());
        };
        // An index-free spelling that DOES resolve, plus at least one
        // `[` somewhere in the original, is exactly this family.
        if path_expr_is_index_free(e) {
            return Ok(());
        }
        // Identify the family by its ROOT rather than by re-resolving a
        // synthesised path: a local shadows everything (and record
        // locals DO carry mid-path selections, via
        // `try_record_field_chain`), so anything rooted at one is not
        // this family.
        let root: &str = segs.first().map(String::as_str).unwrap_or_default();
        if segs.len() < 2 || self.lookup(root).is_some() {
            return Ok(());
        }
        let stripped = self.strip_tb_prefix(&segs);
        let root: &str = stripped.first().map(String::as_str).unwrap_or_default();
        let is_state_record = matches!(
            self.target_state_fields.get(root),
            Some(crate::ir::StateFieldKind::Record { .. })
        ) || self.ctx.target_state.get(root).is_some_and(|f| {
            f.values()
                .any(|k| matches!(k, crate::ir::StateFieldKind::Record { .. }))
        });
        let is_component_record = self
            .self_component
            .and_then(|cid| self.ctx.components.get(cid.index()))
            .and_then(|c| c.field(root))
            .is_some_and(|f| matches!(f.kind, crate::ir::ComponentFieldKind::Record { .. }))
            || self.ctx.component_fields.contains_key(root);
        if !is_state_record && !is_component_record {
            return Ok(());
        }
        let segs = stripped.to_vec();
        Err(unsupported(
            &format!("{what} an element selection inside `{}`", segs.join(".")),
            "this indexed component/state record shape is outside the typed path resolver; \
             use a supported fixed-vector leaf or `Vec<Record, N>` traversal",
        ))
    }

    /// The exact element type and length of a LOWERED whole-`Vec` field read,
    /// in each of the three lanes that produce one. `None` for anything else,
    /// including an indexed element read (that is a scalar, not an array).
    pub(crate) fn ir_whole_vec_type(&self, e: &Expr) -> Option<IrType> {
        let (len, ty) = match e {
            Expr::Local(local) => {
                let ty = self.local_type(*local).clone();
                return matches!(ty, IrType::FixedVec { .. }).then_some(ty);
            }
            Expr::Call(crate::ir::CallTarget::Helper { ret, .. }, _)
                if matches!(ret, IrType::FixedVec { .. }) =>
            {
                return Some(ret.clone());
            }
            Expr::TbField(field) => {
                let ty = self.ctx.tb_scalar_fields.get(field)?.clone();
                return matches!(ty, IrType::FixedVec { .. }).then_some(ty);
            }
            Expr::TransactorState { instance, field } => {
                let kind = if instance.is_empty() {
                    self.target_state_fields.get(field)?
                } else {
                    self.ctx.target_state.get(instance)?.get(field)?
                };
                let crate::ir::StateFieldKind::FixedVec { ty } = kind else {
                    return None;
                };
                return Some(ty.clone());
            }
            Expr::RecordField {
                local,
                field,
                path,
                mid_indices,
                index: None,
            } => {
                let mut cur = self.record_of_local(*local)?;
                let segs: Vec<&String> = std::iter::once(field).chain(path.iter()).collect();
                let last = segs.len() - 1;
                let mut leaf = None;
                for (i, seg) in segs.iter().enumerate() {
                    let fld = self.ctx.records.get(cur.index())?.field(seg)?;
                    if i == last {
                        leaf = Some((fld.vec_len?, fld.ty.clone()));
                        break;
                    }
                    let indexed = mid_indices.iter().any(|(p, _)| *p == i);
                    match fld.ty {
                        IrType::Record(r) if fld.vec_len.is_none() == !indexed => cur = r,
                        _ => return None,
                    }
                }
                leaf?
            }
            Expr::ComponentField { base, field } => {
                let cid = self.component_base_id(base)?;
                let mut segs = field.split('.');
                let root = segs.next()?;
                let root_kind = &self.ctx.components.get(cid.index())?.field(root)?.kind;
                if let crate::ir::ComponentFieldKind::FixedVec(vec) = root_kind {
                    return segs.next().is_none().then(|| IrType::FixedVec {
                        elem: Box::new(vec.elem.clone()),
                        len: vec.len,
                    });
                }
                let crate::ir::ComponentFieldKind::Record { record } = root_kind else {
                    return None;
                };
                let mut cur = *record;
                let mut leaf = None;
                for seg in segs {
                    let fld = self.ctx.records.get(cur.index())?.field(seg)?;
                    leaf = Some((fld.vec_len, fld.ty.clone()));
                    if let IrType::Record(r) = fld.ty {
                        cur = r;
                    }
                }
                let (vl, ty) = leaf?;
                (vl?, ty)
            }
            Expr::TransactorStateRecordField {
                instance,
                field,
                path,
                mid_indices,
                index,
            } => {
                if index.is_some() || !mid_indices.is_empty() {
                    return None;
                }
                let kind = if instance.is_empty() {
                    self.target_state_fields.get(field)
                } else {
                    self.ctx.target_state.get(instance)?.get(field)
                };
                let Some(crate::ir::StateFieldKind::Record { record }) = kind else {
                    return None;
                };
                let mut cur = *record;
                let mut leaf = None;
                for seg in path {
                    let fld = self.ctx.records.get(cur.index())?.field(seg)?;
                    leaf = Some((fld.vec_len, fld.ty.clone()));
                    if let IrType::Record(r) = fld.ty {
                        cur = r;
                    }
                }
                let (vl, ty) = leaf?;
                (vl?, ty)
            }
            _ => return None,
        };
        Some(IrType::FixedVec {
            elem: Box::new(ty),
            len,
        })
    }

    /// The `(len, C++ element class)` of a LOWERED whole-`Vec` field read.
    /// Event payload matching uses the exact type above because distinct
    /// widths can share one C++ carrier.
    pub(crate) fn ir_whole_vec_shape(&self, e: &Expr) -> Option<(usize, String)> {
        let IrType::FixedVec { elem, len } = self.ir_whole_vec_type(e)? else {
            unreachable!("whole-vector type is fixed-vector typed")
        };
        Some((len, crate::codegen::cpp_tb::ir_vec_elem_class(&elem)?))
    }

    /// Whether both operands are whole-`Vec` record-field reads of the
    /// SAME length and element type — the pairing `==`/`!=` needs before
    /// the read is permitted at all.
    ///
    /// A `false` restores the verdict each operand had before this
    /// permission existed; it never turns a lowering program into a
    /// rejected one.
    fn same_whole_vec_shape(&mut self, lhs: &crate::ast::Expr, rhs: &crate::ast::Expr) -> bool {
        match (self.whole_vec_leaf(lhs), self.whole_vec_leaf(rhs)) {
            (Some(l), Some(r)) => l == r,
            _ => false,
        }
    }

    fn same_whole_seq_shape(&mut self, lhs: &crate::ast::Expr, rhs: &crate::ast::Expr) -> bool {
        match (self.whole_seq_leaf(lhs), self.whole_seq_leaf(rhs)) {
            (Some(l), Some(r)) => l == r,
            _ => false,
        }
    }
}

/// The index-free spelling of a path expression: `a.tbl[0].data` →
/// `["a", "tbl", "data"]`. `None` when `e` is not a path.
pub(crate) fn path_segments(e: &crate::ast::Expr) -> Option<Vec<String>> {
    match &*e.kind {
        ExprKind::Ident(id) => Some(vec![id.name.clone()]),
        ExprKind::Paren(inner) => path_segments(inner),
        ExprKind::Index { target, .. } => path_segments(target),
        ExprKind::Field { target, name } => {
            let mut segs = path_segments(target)?;
            segs.push(name.name.clone());
            Some(segs)
        }
        _ => None,
    }
}

/// Whether a path expression contains no `[` at all.
fn path_expr_is_index_free(e: &crate::ast::Expr) -> bool {
    match &*e.kind {
        ExprKind::Ident(_) => true,
        ExprKind::Paren(inner) | ExprKind::Field { target: inner, .. } => {
            path_expr_is_index_free(inner)
        }
        ExprKind::Index { .. } => false,
        _ => true,
    }
}

/// A pure PATH expression: `Ident`, `.field`, `[index]`, parens. The
/// index sub-expression may be anything — it is lowered exactly once,
/// by whoever lowers the path.
///
/// Wider than `dotted_path`, which stops at `Index`. The two are used
/// in different positions: `dotted_path` gates a SPECULATIVE resolve
/// whose result is thrown away, so it must exclude anything that would
/// be lowered twice; this gates a permission on an expression that is
/// lowered once.
fn path_expr(e: &crate::ast::Expr) -> bool {
    match &*e.kind {
        ExprKind::Ident(_) => true,
        ExprKind::Paren(inner) => path_expr(inner),
        ExprKind::Field { target, .. } => path_expr(target),
        ExprKind::Index { target, .. } => path_expr(target),
        _ => false,
    }
}

/// `pop()` in a nested expression, at any of the five queue spellings.
///
/// The verdict is `Unsupported` and it is not a placeholder: v1 evaluates
/// the call where it is written, and TB-IR has no way to. Lowering an
/// expression-position call means hoisting it into a statement before
/// the expression, and for a MUTATING call that is only equivalent when
/// the surrounding expression evaluates it unconditionally, exactly once.
///
/// Short-circuit operators break that, and C++ preserves them. Measured —
/// `assert (guard == 1 && sb.q.pop() == 7) || sb.q.size() == 1` emits from
/// v1 as `if (!((guard == 1 && _tb.sb.q.pop() == 7) || _tb.sb.q.size() == 1))`,
/// so with `guard == 0` the pop NEVER RUNS and the queue keeps its
/// element. A hoisted lowering would pop first, empty the queue, and
/// fail an assert v1 passes — a silent behavioural divergence, which is
/// the one outcome worth refusing a program over.
///
/// Repeated evaluation is an obstacle at ONE of the two looping
/// constructs, and an earlier version of this note said it was neither.
///
///   * `while` is fine. `lower_while` opens the header block before
///     lowering the condition, so a hoisted call lands IN the header and
///     the back-edge targets it — measured on a `while tick() < 3` over
///     a testbench method, whose `b1` holds the inlined body and whose
///     `b5` jumps back to `b1`. A pop there would re-run each iteration,
///     as it should. (A pure file-scope helper does not CFG-inline, so
///     it is not a witness for this; the testbench method is.)
///   * `wait until` is NOT. `lower_wait_until` lowers the predicate into
///     the block PRECEDING the terminator, so a hoisted call runs
///     exactly once — while v1 emits the predicate as a lambda
///     (`wait_until_timeout(_slot, [&]{ return _tb.sb.q.pop() == 7; }, …)`)
///     and re-runs it every cycle. That function already says as much
///     about transactor edges thirty lines up: "hoisting would run it
///     exactly once, the wrong semantics".
///
/// So implementing this needs BOTH: `&&`/`||` lowered to branches when a
/// side-effecting call sits under them, and a `wait until` predicate that
/// re-evaluates rather than hoists. `wait until sb.q.pop() == 7` contains
/// no short-circuit at all, so the first alone would not reach it.
fn queue_pop_in_expression_position(what: &str) -> LowerError {
    unsupported(
        &format!("{what} in a nested expression"),
        "bind it to its own `let` first — `pop` mutates the queue, and TB-IR would have \
         to hoist the call out of the expression, which changes whether a short-circuited \
         one runs at all",
    )
}

/// A queue method in EXPRESSION position that no query arm claimed.
///
/// `size` and `empty` lower here and `pop` has its own arm immediately
/// above, so what reaches this is either `push` — which returns void —
/// or a name `harc_rt::HarcQueue` never declares. Both are program
/// errors rather than subset gaps, which is the difference from
/// [`super::stmts::queue_method_in_statement_position`]: there the
/// value is DISCARDED, so `size`/`empty` make a legal no-op that v1
/// compiles and runs, and only they keep the `--codegen v1` suggestion.
///
/// Measured at all five landings — testbench-owned field, scoreboard
/// queue, component queue, bare target-state field, instance-qualified
/// target-state field — rather than four inferred from one. v1 emits
/// `uint64_t z = <recv>.<name>(...);` at every one, and g++ rejects
/// every one:
///
/// | call | g++ |
/// |---|---|
/// | `q.push(3)` | "void value not ignored as it ought to be" |
/// | `q.push()` | "no matching function for call to `HarcQueue<...>::push()`" |
/// | `q.front()` / `q.clear()` / a typo | "has no member named `front`" |
///
/// (`q.size()` compiles, which is why it never reaches this arm.)
fn queue_method_in_expression_position(what: &str, method: &str) -> LowerError {
    if method == "push" {
        return LowerError::Invalid(format!(
            "{what} in expression position: `push` returns no value"
        ));
    }
    LowerError::Invalid(format!(
        "{what} in expression position: `HarcQueue` has only `push`, `pop`, `size` and \
         `empty`"
    ))
}

impl FuncBuilder<'_> {
    /// Lower with `Expr::Port` allowed in the result.
    pub(crate) fn lower_expr(&mut self, e: &AstExpr) -> Result<Expr, LowerError> {
        // Inside a concurrent check body, a `past`/`rose`/`fell`/`stable`
        // occurrence resolves to its pre-assigned latch slot rather than
        // recursing into the operand — the operand is latched once per
        // cycle by the check closure, not re-evaluated per reference.
        // Mirrors v1's span-keyed `prop_subs` hook at the head of
        // `emit_expr_with_arrow`.
        if !self.temporal_slots.is_empty() {
            if let Some(&(slot, kind)) = self.temporal_slots.get(&(e.span.start, e.span.end)) {
                return Ok(Expr::TemporalSlot { slot, kind });
            }
        }
        match &*e.kind {
            ExprKind::Int(s) => {
                if let Some(value) = parse_int_literal(s) {
                    return Ok(Expr::Literal {
                        value,
                        ty: IrType::Unknown,
                    });
                }
                // Coverpoints and record Vec indices already use this parser
                // with declared-width semantics. General value positions can
                // share its numeric decoding when the value fits the native
                // scalar carrier, while matching v1's host-scalar behavior.
                // The declared width does not change v1's general-position
                // host-scalar value: `128'h1` is still the ordinary integer
                // one. Only the value overflowing this carrier needs the
                // wide-word path below.
                if let Some((1.., value)) = parse_sized_int_literal_with_width(s) {
                    return Ok(Expr::Literal {
                        value,
                        // v1 emits a general sized literal as an ordinary
                        // signed host integer. Width-aware literals remain in
                        // the cover/index-specific paths; using the host type
                        // here preserves v1 behavior through composition.
                        ty: IrType::Unknown,
                    });
                }
                // Hex literals wider than 64 bits lower to LSB-first
                // 32-bit word lists (v1's `c_wide_lit_words` shape).
                if let Some(words) = parse_wide_hex_literal(s) {
                    return Ok(Expr::WideLiteral(words));
                }
                if let Some(words) = parse_wide_sized_hex_literal(s) {
                    return Ok(Expr::WideLiteral(words));
                }
                Err(unsupported(
                    "integer literal",
                    format!("`{s}` is not a plain literal"),
                ))
            }
            ExprKind::Bool(b) => Ok(Expr::Literal {
                value: *b as u64,
                ty: IrType::Bool,
            }),
            ExprKind::Ident(id) => {
                if let Some(local) = self.lookup(&id.name) {
                    return Ok(Expr::Local(local));
                }
                self.reject_inactive_target_state_root(e)?;
                // Whole transaction/struct-typed testbench field read
                // (`cur`) from an inlined testbench method. Ordinary
                // lookup is intentionally fenced at inline-helper
                // boundaries, but shared testbench record state must remain
                // visible so calls like `drv.drive(cur)` pass the persistent
                // record object rather than looking for a caller local.
                if let Some(local) = self.lookup_tb_record_field_in_capture_scope(&id.name) {
                    if self.record_of_local(local).is_some() {
                        return Ok(Expr::Local(local));
                    }
                }
                // The framework cycle counter (`cycle_count`), conventionally
                // referenced from `${cycle_count}` in a watchdog/log
                // diagnostic. A local of the same name shadows it (checked
                // above). v1 emits the in-scope `cycle_count` variable.
                if id.name == "cycle_count" {
                    return Ok(Expr::CycleCount);
                }
                // The framework error counter (`errors`), referenced from
                // `assert errors == 0` / `${errors}` after a walk like
                // `bitbash(regs)`. Locals shadow (checked above), and
                // codegen emits the framework counter directly.
                if id.name == "errors" {
                    return Ok(Expr::ErrorCount);
                }
                // Persistent state field of a bound-to target responder
                // body — a bare ident (locals shadow, checked above).
                // `instance` is a placeholder; the test-binding stage
                // fills it once the passive instance is resolved. A bare
                // read is only valid for a SCALAR field; a queue field is
                // read via its ops (`.size()`/`.empty()`/`.pop()`), so a
                // bare queue ident is rejected precisely.
                if let Some(kind) = self.target_state_fields.get(&id.name) {
                    return match kind {
                        // A bare read of a scalar OR a whole-record state
                        // field is `TransactorState` (`instance.<field>`);
                        // for a record this is a by-value struct read
                        // (copied into a `let`, pushed onto a queue, …).
                        crate::ir::StateFieldKind::Scalar { .. }
                        | crate::ir::StateFieldKind::Record { .. }
                        | crate::ir::StateFieldKind::FixedVec { .. } => Ok(Expr::TransactorState {
                            instance: String::new(),
                            field: id.name.clone(),
                        }),
                        // Not a `--codegen v1` escape: v1 emits the bare
                        // read into a scalar slot and g++ refuses "cannot
                        // convert `harc_rt::HarcQueue<...>` to `uint64_t`"
                        // (measured). Read via the queue ops instead.
                        crate::ir::StateFieldKind::Queue { .. } => Err(not_implemented(
                            &format!("a bare read of the `queue` state field `{}`", id.name),
                            "read a queue state field via `.size()` / `.empty()` / `.pop()`",
                            V1Status::EmitsUncompilable,
                        )),
                    };
                }
                if self.is_dut_name(&id.name) {
                    // v1 emits the DUT POINTER into an integer slot
                    // (`int64_t x = dut;`), which does not compile.
                    return Err(not_implemented(
                        "a bare DUT reference",
                        "DUT access must name a port (`dut.<port>`)",
                        V1Status::EmitsUncompilable,
                    ));
                }
                // A bare enum-variant name declared by more than one
                // enum has no correct index as a value: `consts` folded it
                // first-wins, so substituting would silently pick one enum's
                // numbering (harc#666). Reject instead. This is value
                // position only — constraint lowering resolves variants
                // through its own path, so a use inside a `keep` still
                // lowers under the documented first-wins rule. A local of
                // the same name shadowed this above, so a shadowing binder
                // is unaffected.
                if let Some(owners) = self.ctx.ambiguous_variants.get(&id.name) {
                    return Err(LowerError::Invalid(format!(
                        "enum variant `{name}`: it is declared by more than one \
                         enum (`{owners}`), so no single index is correct for a \
                         bare `{name}`. HARC has no qualified `Enum.VARIANT` form, \
                         so rename one of them.",
                        name = id.name,
                        owners = owners,
                    )));
                }
                // File-scope `const` / enum-variant substitution
                // (locals shadow — checked above; v1's constexpr /
                // variant-index emission is value-identical).
                if let Some(v) = self.ctx.consts.get(&id.name) {
                    return Ok(Expr::Literal {
                        value: *v,
                        ty: if self
                            .ctx
                            .const_signed
                            .get(&id.name)
                            .copied()
                            .unwrap_or(false)
                        {
                            IrType::SInt(None)
                        } else {
                            IrType::UInt(None)
                        },
                    });
                }
                // Self-relative component field read inside a method body
                // (`count` → `self.count`). Locals shadow (checked above).
                if let Some(ce) = self.as_component_field_read(e)? {
                    return Ok(ce);
                }
                // Whole composite-component value read — a self sub-component
                // field passed by value as a method arg (`sb.observe(addr,
                // model)`). Locals shadow (checked above).
                if let Some(cv) = self.as_component_value_read(e)? {
                    return Ok(cv);
                }
                // Scalar testbench host state (`expected_checks`) and
                // promoted test-scope lets live on `_tb`. Bare access is
                // allowed only from the test/check/hook body itself or from
                // an inlined `_tb.<method>` frame; free helpers stay fenced.
                if let Some(field) = self.tb_scalar_field_in_capture_scope(&id.name) {
                    // A fixed-vector host field is scalar-shaped storage but
                    // a whole-`Vec` value; its element access lowers in the
                    // indexed lane (`as_tb_vec_field`). A bare whole-`Vec`
                    // read is admitted only at an explicitly typed aggregate
                    // landing such as an event or method parameter.
                    let is_fixed = matches!(
                        self.ctx.tb_scalar_fields.get(&field),
                        Some(IrType::FixedVec { .. })
                    );
                    if !is_fixed || self.whole_vec_read_allowed(e) {
                        return Ok(Expr::TbField(field));
                    }
                }
                if self.in_check && self.ctx.test_scope_lets.contains(&id.name) {
                    return Err(unsupported(
                        &format!("test-scope `let {}` referenced in the check phase", id.name),
                        "test-scope lets lower as run-function locals; run and check are \
                         separate functions in the IR, so v1's shared-capture scoping is \
                         not representable",
                    ));
                }
                // v1 has no rejection for an unresolved name: it emits
                // the identifier verbatim (`int64_t x = nosuchthing;`),
                // which the C++ compiler rejects as undeclared.
                Err(not_implemented(
                    &format!("the unresolved name `{}`", id.name),
                    "",
                    V1Status::EmitsUncompilable,
                ))
            }
            ExprKind::Field { target, name } => {
                self.reject_inactive_target_state_root(target)?;
                if let Some(port) = self.as_port_ref(e)? {
                    return Ok(Expr::Port(port));
                }
                if let Some(cov_bin) = self.as_cov_bin(e)? {
                    return Ok(cov_bin);
                }
                // Whole fixed-vector testbench field (`_tb.values`) at an
                // explicitly typed aggregate landing (event/method arg).
                if self.whole_vec_read_allowed(e) {
                    if let Some((field, _)) = self.as_tb_vec_field(e) {
                        return Ok(Expr::TbField(field));
                    }
                }
                // Scalar testbench field read (`_tb.expected`).
                if let Some(field) = self.as_tb_scalar_field(e) {
                    return Ok(Expr::TbField(field));
                }
                // Whole transaction/struct-typed testbench field read
                // (`_tb.cur`) — used when passing shared record state to
                // helpers, monitors, or scoreboards. Field-level reads
                // (`_tb.cur.value`) are handled below by the record-field
                // path.
                if let Some(local) = self.record_target_local(e) {
                    if self.record_of_local(local).is_some() {
                        return Ok(Expr::Local(local));
                    }
                }
                // Subfield read of a bound-to target responder's whole-
                // record state field (`last.data` in a responder body /
                // `responder.last.data` from the test). Checked before the
                // whole-record `as_transactor_state` lane, which only fires
                // when there is NO further subfield.
                if let Some(chain) = self.as_transactor_state_record_field(e)? {
                    if matches!(chain.leaf_ty, IrType::Seq(_)) && !self.whole_vec_read_allowed(e) {
                        let dotted = format!("{}.{}", chain.field, chain.path.join("."));
                        return Err(dynamic_record_list_value(&dotted));
                    }
                    // Same allow-list as the other two whole-`Vec` read
                    // lanes: only an `==`/`!=` against a matching shape
                    // gets through, and only because both backends emit
                    // `target.ba.data == target.bb.data`, which g++
                    // accepts. See `vec_read_ok`.
                    if chain.leaf_vec_len.is_some() && !self.whole_vec_read_allowed(e) {
                        let dotted = format!("{}.{}", chain.field, chain.path.join("."));
                        return Err(not_implemented(
                            &format!("a whole-`Vec` read of record state field `{dotted}`"),
                            // Real names: this detail was a plain string,
                            // so its `{field}.{vec}` placeholders printed
                            // the braces at the user verbatim.
                            format!("read the field element-wise (`{dotted}[i]`)"),
                            V1Status::EmitsUncompilable,
                        ));
                    }
                    return Ok(Expr::TransactorStateRecordField {
                        instance: chain.instance,
                        field: chain.field,
                        path: chain.path,
                        mid_indices: chain.mid_indices,
                        index: chain.leaf_index.map(Box::new),
                    });
                }
                // Test-scope read of a bound-to target responder's
                // persistent state (`target.read_count`, or a whole-record
                // `target.last`).
                if let Some((instance, field)) = self.as_transactor_state(e) {
                    return Ok(Expr::TransactorState { instance, field });
                }
                // Scoreboard scalar-counter read (`sb.writes` /
                // `_tb.sb.writes` after impl-form desugaring).
                if let Some((sb, field, nested_path)) = self.scoreboard_root(target) {
                    let scalar = self.scoreboard_scalar_field(sb, &name.name)?;
                    return Ok(Expr::ScoreboardQuery {
                        sb,
                        field,
                        query: crate::ir::ScoreboardQuery::Scalar { scalar },
                        nested_path,
                    });
                }
                // Regblock-binding access in expression position. The
                // mirror IS a record local, so `regs.NAME` would
                // otherwise fall into the record-field path below and
                // silently read the mirror — but a RW/RO register read
                // must go to the bus (v1's frontdoor + read-predict).
                // Register reads are only lowered in `let`-RHS position
                // (`let v = regs.NAME`), so any register read reaching
                // here sits in a value position the IR can't represent
                // without a hoist that changes the bus-read count.
                if let Some((binding, reg)) = self.as_regblock_register(e) {
                    return self.lower_regblock_read_expr(&binding, &reg);
                }
                // Field-level read in expression position
                // (`regs.REG.FIELD` in an assert/format arg). Same
                // read-count semantics as the whole-register form.
                if let Some((binding, reg, fld)) = self.as_regblock_subfield(e) {
                    return self.lower_regblock_subfield_read_expr(&binding, &reg.name, &fld.name);
                }
                // Addrmap access in expression position
                // (`chip.inst.REG[.FIELD]`).
                if let Some(ax) = self.lower_addrmap_read_expr(e)? {
                    return Ok(ax);
                }
                self.reject_out_of_subset_regblock_access(e, "read")?;
                self.reject_out_of_subset_addrmap_access(e, "read")?;
                if let Some(chain) = self.as_indexed_component_record_field(e)? {
                    if matches!(chain.leaf_ty, IrType::Seq(_)) && !self.whole_vec_read_allowed(e) {
                        return Err(dynamic_record_list_value(&chain.field));
                    }
                    return Ok(Expr::ComponentVecElement {
                        base: chain.base,
                        field: chain.field,
                        index_pos: chain.index_pos,
                        index: Box::new(chain.index),
                        inner_index: None,
                    });
                }
                // Composite-component scalar field read via a test-scope
                // path (`env.sb.count`).
                if let Some(ce) = self.as_component_field_read(e)? {
                    return Ok(ce);
                }
                // `r.field` read on a `recv()`-captured payload local
                // (`let r = bus.<ch>.recv(); ... r.data`). Each payload
                // signal was captured into its own local at recv time;
                // resolve the named field to that local. v1 reads the
                // field off the captured payload struct.
                if let ExprKind::Ident(root) = &*target.kind {
                    if let Some(local) = self.lookup(&root.name) {
                        if let Some(fields) = self.recv_payloads.get(&local) {
                            return match fields.iter().find(|(f, _)| f == &name.name) {
                                Some((_, fid)) => Ok(Expr::Local(*fid)),
                                None => Err(LowerError::Invalid(format!(
                                    "recv payload `{}` has no field `{}` (valid: {})",
                                    root.name,
                                    name.name,
                                    fields
                                        .iter()
                                        .map(|(f, _)| f.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ))),
                            };
                        }
                    }
                }
                // `t.field` read on a record-typed local (and nested
                // `t.a.b`). Resolve the field chain to its leaf schema.
                if let Some(chain) = self.try_record_field_chain(e)? {
                    if matches!(chain.leaf_ty, IrType::Seq(_)) && !self.whole_vec_read_allowed(e) {
                        return Err(dynamic_record_list_value(&chain.spelled));
                    }
                    // A whole-`Vec` leaf read has no SCALAR value: in a
                    // scalar/format context the emitter would put the raw
                    // `std::array` member where an integer is expected,
                    // which fails as a raw g++ error rather than a
                    // structured HARC diagnostic. So the read is refused
                    // by default and permitted per landing.
                    //
                    // Two landings are sanctioned. `==`/`!=` against a
                    // matching-shape field is permitted right here, via
                    // `vec_read_ok`. A `dst.field = src.field` copy is
                    // permitted by the write arms in `stmts.rs`, which
                    // all route their RHS through `whole_vec_copy_rhs`
                    // — that is what turns the flag on for it, and
                    // therefore what brings the RHS back through this
                    // very arm. Element access
                    // (`rec.data[i]`) never reaches this arm at all — the
                    // `Index` arm handles it. A whole nested-RECORD leaf
                    // read (`let d = s.inner`) is unrelated and always
                    // allowed: it yields the nested struct value.
                    if chain.leaf_vec_len.is_some() && !self.whole_vec_read_allowed(e) {
                        // What v1 does with the read depends on where it
                        // LANDS, and the equality landing is now
                        // implemented rather than refused with the rest:
                        // `assert r.data == s.data` emits
                        // `r.data == s.data` from BOTH backends and
                        // compiles (`std::array` has `operator==`, and v1
                        // generates one for a record element type, so
                        // `Vec<Kid, N>` compares too).
                        //
                        // The others still do not. `let d = r.data` and
                        // `${r.data}` each emit C++ that g++ refuses,
                        // under both backends — measured by permitting
                        // the read everywhere and compiling the result.
                        // So the permission is an ALLOW-list keyed on the
                        // landing (`vec_read_ok`), not a deny-list: a
                        // landing nobody enumerated keeps this
                        // diagnostic instead of silently emitting code
                        // that does not build.
                        // Every landing that still reaches here fails
                        // under v1 too, measured: `let d = r.data` gives
                        // "cannot convert `std::array<…,4>` to `int64_t`",
                        // `${r.data}` an invalid `static_cast`, and a
                        // mismatched comparison "no match for `operator==`".
                        // The one landing where v1 genuinely compiled is
                        // the equality above, which is implemented — so
                        // the `--codegen v1` suggestion this carried is
                        // no longer true anywhere it is still printed.
                        return Err(not_implemented(
                            &format!("a whole-`Vec` read of record field `{}`", chain.dotted),
                            // The path the USER wrote, not `dotted`.
                            // `dotted` is rooted at the record TYPE, so
                            // it would suggest `Rec.data[i]` — not an
                            // expression anyone can type. (The original
                            // bug here was worse: a `format!`-shaped
                            // sentence in a plain `&str` slot printed
                            // `{rec}.{field}` braces and all.)
                            format!("index the field element-wise (`{}[i]`)", chain.spelled),
                            V1Status::EmitsUncompilable,
                        ));
                    }
                    return Ok(Expr::RecordField {
                        local: chain.local,
                        field: chain.field,
                        path: chain.path,
                        mid_indices: chain.mid_indices,
                        index: None,
                    });
                }
                // Bus-bound signal access (`<bind>.<sig>`, `<bind>.<ch>.<sig>`).
                if let Some(port) = self.as_bus_port_ref(e)? {
                    return Ok(Expr::Port(port));
                }
                // Every field-access shape either backend implements has
                // been tried by here. v1 has no rejection for the
                // leftovers: it passes the access straight through as
                // C++ member syntax, so `let y = x.foo` on a scalar
                // local emits `int64_t y = x.foo;` — a member access on
                // an integer, which the C++ compiler rejects.
                self.reject_indexed_component_record_path(e, "a read through")?;
                Err(not_implemented(
                    &format!("field access on a non-DUT value ending in `.{}`", name.name),
                    "",
                    V1Status::EmitsUncompilable,
                ))
            }
            ExprKind::Paren(inner) => self.lower_expr(inner),
            ExprKind::Unary { op, expr } => {
                let inner = self.lower_expr(expr)?;
                if matches!(self.expr_type(&inner), Some(IrType::FixedVec { .. })) {
                    return Err(not_implemented(
                        "a scalar unary operator applied to a fixed-vector local",
                        "select a scalar lane before applying the operator",
                        V1Status::EmitsUncompilable,
                    ));
                }
                if matches!(self.expr_type(&inner), Some(IrType::Seq(_))) {
                    return Err(not_implemented(
                        "a scalar unary operator applied to a dynamic-list local",
                        "index or query the list before applying the operator",
                        V1Status::EmitsUncompilable,
                    ));
                }
                let op = match op {
                    UnaryOp::Neg => UnOp::Neg,
                    UnaryOp::Not | UnaryOp::NotKw => UnOp::Not,
                    UnaryOp::BitNot if ast_expr_contains_sized_literal(expr) => UnOp::BitNotHost,
                    UnaryOp::BitNot => UnOp::BitNot,
                };
                Ok(Expr::Unary(op, Box::new(inner)))
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let ir_op = lower_bin_op(*op)?;
                // `==`/`!=` is the one landing a whole-`Vec` read works
                // in — see `vec_read_ok`. It is set for BOTH operands
                // and restored after, which scopes the permission to
                // this comparison: an ENCLOSING expression's remaining
                // parts, and any statement lowered after it, see the
                // saved value again. What keeps a nested expression
                // INSIDE an operand from inheriting it is a different
                // mechanism — the pairing check below admits pure
                // dotted paths only, which have no nested expression to
                // inherit anything.
                // …and only when BOTH sides are the same `Vec` shape.
                // Permitting the read on an equality alone let
                // `r.data == s.kids` (scalar elements vs record ones) and
                // `r.data == s.n` (a `Vec` against a scalar) through,
                // where both backends emit a comparison g++ refuses —
                // trading a clean diagnostic for an uncompilable one.
                // Before this arm existed they were refused by the read
                // itself, so the permission had to carry the pairing
                // check the read no longer does.
                let eq = matches!(op, BinaryOp::Eq | BinaryOp::Ne)
                    && (self.same_whole_vec_shape(lhs, rhs) || self.same_whole_seq_shape(lhs, rhs));
                let saved = self.vec_read_ok;
                let saved_span = self.vec_read_span;
                self.vec_read_ok = eq;
                self.vec_read_span = eq.then(|| unparen_expr(lhs).span);
                let l = self.lower_expr(lhs);
                let r = if l.is_ok() {
                    self.vec_read_span = eq.then(|| unparen_expr(rhs).span);
                    self.lower_expr(rhs)
                } else {
                    Ok(Expr::Literal {
                        value: 0,
                        ty: IrType::Unknown,
                    })
                };
                self.vec_read_ok = saved;
                self.vec_read_span = saved_span;
                let (l, r) = (l?, r?);
                let (lty, rty) = (self.expr_type(&l), self.expr_type(&r));
                if matches!(lty, Some(IrType::FixedVec { .. }))
                    || matches!(rty, Some(IrType::FixedVec { .. }))
                {
                    let matching_equality = matches!(ir_op, BinOp::Eq | BinOp::Ne)
                        && lty.is_some()
                        && lty == rty;
                    if !matching_equality {
                        return Err(not_implemented(
                            "a scalar binary operator applied to a fixed-vector local",
                            "only same-shape `==`/`!=`, whole-value copies, and queue transfers \
                             are lowered for fixed-vector locals",
                            V1Status::EmitsUncompilable,
                        ));
                    }
                }
                if matches!(lty, Some(IrType::Seq(_))) || matches!(rty, Some(IrType::Seq(_))) {
                    let matching_equality = matches!(ir_op, BinOp::Eq | BinOp::Ne)
                        && lty.is_some()
                        && lty == rty;
                    if !matching_equality {
                        return Err(not_implemented(
                            "a scalar binary operator applied to a dynamic-list local",
                            "only same-element-type `==`/`!=`, whole-value copies, and queue \
                             transfers are lowered for dynamic-list locals",
                            V1Status::EmitsUncompilable,
                        ));
                    }
                }
                let (l, r) = self.zext_mixed_width_unsigned_operands(ir_op, l, r);
                self.reject_unbuildable_wide_operator(*op, ir_op, &l, &r)?;
                let inner = Expr::Binary(ir_op, Box::new(l), Box::new(r));
                // Wrapping arithmetic `+% -% *%` (harc#473): mask the result
                // to `max(W(lhs), W(rhs))` bits, matching ARCH's
                // `AddWrap/SubWrap/MulWrap` (result width = wider operand, no
                // widening). The mask is a `WidthCast::Trunc`, which codegen
                // lowers to `(expr) & ((1<<W)-1)`. Non-wrapping ops pass
                // through unchanged.
                if matches!(
                    op,
                    BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap
                ) {
                    return self.wrap_to_operand_width(*op, lhs, rhs, inner);
                }
                Ok(inner)
            }
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => {
                // Lowered to the IR ternary, emitted as the C++ `?:`
                // operator — the not-taken arm stays lazily skipped,
                // exactly v1's emission. (Port reads hoisted out of a
                // ternary by `lower_expr_no_ports` become eager, but a
                // DUT port read is side-effect-free and untraced, so
                // the difference is unobservable.)
                let c = self.lower_expr(cond)?;
                self.validate_truth_expr(&c, "ternary condition")?;
                let t = self.lower_expr(then_branch)?;
                let e = self.lower_expr(else_branch)?;
                let (tty, ety) = (self.expr_type(&t), self.expr_type(&e));
                if (matches!(tty, Some(IrType::FixedVec { .. }))
                    || matches!(ety, Some(IrType::FixedVec { .. })))
                    && (tty.is_none() || tty != ety)
                {
                    return Err(not_implemented(
                        "a ternary mixing a fixed-vector local with another value shape",
                        "both ternary arms must have the same fixed-vector type",
                        V1Status::EmitsUncompilable,
                    ));
                }
                if (matches!(tty, Some(IrType::Seq(_))) || matches!(ety, Some(IrType::Seq(_))))
                    && (tty.is_none() || tty != ety)
                {
                    return Err(not_implemented(
                        "a ternary mixing a dynamic-list local with another value shape",
                        "both ternary arms must have the same dynamic-list type",
                        V1Status::EmitsUncompilable,
                    ));
                }
                Ok(Expr::Ternary(Box::new(c), Box::new(t), Box::new(e)))
            }
            ExprKind::Call { callee, args } => {
                let what = match &*callee.kind {
                    ExprKind::Ident(id) => {
                        if self.in_testbench_method_frame()
                            && self.ctx.tb_methods.contains_key(&id.name)
                        {
                            return self.lower_tb_method_call(&id.name, args);
                        }
                        if let Some(call) = self.lower_transactor_self_call(&id.name, args, true)? {
                            return Ok(call);
                        }
                        if self.helpers.contains(&id.name) {
                            return self.lower_helper_call(&id.name, args);
                        }
                        if self.ctx.extern_fns.contains_key(&id.name) {
                            return self.lower_extern_fn_call(&id.name, args);
                        }
                        // `past(x)` / `rose(x)` / `fell(x)` / `stable(x)`
                        // written as a plain call. Legal only inside a
                        // concurrent check CONDITION, where the slot map
                        // intercepts it before this arm; the three
                        // no-slot-map positions inside such a check (a
                        // latch operand, another temporal's operand, and
                        // the `else fail(...)` message) land here along
                        // with every position outside one. All of them
                        // lack a per-cycle latch to read, and v1 emits
                        // NOTHING for a temporal outside a property check
                        // (`emit_expr` has no arm for it), so there is no
                        // `--codegen v1` escape hatch to point at.
                        if matches!(id.name.as_str(), "past" | "rose" | "fell" | "stable") {
                            return Err(LowerError::Invalid(format!(
                                "`{}(...)` is a temporal reading; it is only meaningful in \
                                 the CONDITION of a concurrent `assert`/`assume`/`cover` — \
                                 not nested inside another temporal reading, and not in \
                                 that check's `else fail(...)` message",
                                id.name
                            )));
                        }
                        format!("helper call `{}(...)`", id.name)
                    }
                    ExprKind::Field { target, name } => {
                        // Width-method intrinsics: `.trunc<N>()` /
                        // `.zext<N>()` / `.sext<N>()` / `.resize<N>()`.
                        if let Some(kind) = width_cast_kind(&name.name) {
                            return self.lower_width_method(kind, &name.name, target, args);
                        }
                        // Read-only queries on a dynamic record-list field.
                        // Resolve the whole-list receiver directly rather
                        // than passing it through ordinary expression
                        // lowering, where a bare list is intentionally not a
                        // scalar value.
                        if args.is_empty() && matches!(name.name.as_str(), "len" | "size" | "empty")
                        {
                            let query = if name.name == "empty" {
                                crate::ir::DynamicListQuery::Empty
                            } else {
                                crate::ir::DynamicListQuery::Size
                            };
                            if let Some(chain) = self.try_record_field_chain(target)? {
                                if matches!(chain.leaf_ty, IrType::Seq(_)) {
                                    return Ok(Expr::DynamicListQuery {
                                        target: Box::new(Expr::RecordField {
                                            local: chain.local,
                                            field: chain.field,
                                            path: chain.path,
                                            mid_indices: chain.mid_indices,
                                            index: None,
                                        }),
                                        query,
                                    });
                                }
                            }
                            if let Some(chain) = self.as_indexed_component_record_field(target)? {
                                if matches!(chain.leaf_ty, IrType::Seq(_)) {
                                    return Ok(Expr::DynamicListQuery {
                                        target: Box::new(Expr::ComponentVecElement {
                                            base: chain.base,
                                            field: chain.field,
                                            index_pos: chain.index_pos,
                                            index: Box::new(chain.index),
                                            inner_index: None,
                                        }),
                                        query,
                                    });
                                }
                            }
                            if let Some(rf) = self.as_component_record_field(target)? {
                                if matches!(rf.leaf_ty, IrType::Seq(_)) {
                                    return Ok(Expr::DynamicListQuery {
                                        target: Box::new(Expr::ComponentField {
                                            base: rf.base,
                                            field: rf.field,
                                        }),
                                        query,
                                    });
                                }
                            }
                            if let Some(chain) = self.as_transactor_state_record_field(target)? {
                                if matches!(chain.leaf_ty, IrType::Seq(_)) {
                                    return Ok(Expr::DynamicListQuery {
                                        target: Box::new(Expr::TransactorStateRecordField {
                                            instance: chain.instance,
                                            field: chain.field,
                                            path: chain.path,
                                            mid_indices: chain.mid_indices,
                                            index: chain.leaf_index.map(Box::new),
                                        }),
                                        query,
                                    });
                                }
                            }
                        }
                        // Component heartbeat-idle predicates:
                        // `agent.idle_in(N)`, `.idle_out(N)`, `.idle(N)`.
                        if let Some(predicate) =
                            self.as_component_builtin_predicate(callee, args)?
                        {
                            return Ok(predicate);
                        }
                        // Direct transactor heartbeat predicates use the
                        // transactor field/schema namespace rather than a
                        // composite `ComponentBase`.
                        if let Some(idle) = self.as_transactor_idle(callee, args)? {
                            return Ok(idle);
                        }
                        // Testbench-owned queue value-queries. `pop()`
                        // mutates and is accepted only as a standalone let
                        // RHS by the statement lowering path.
                        if let Some(q) = self.lower_tb_queue_query_call(callee, args)? {
                            return Ok(q);
                        }
                        // Scoreboard queue value-queries: `sb.q.size()`,
                        // `sb.q.empty()`. (`sb.q.pop()` mutates and is
                        // lowered only as a statement — reaching it here
                        // means it was used in a deeper expression
                        // position, which is rejected below.)
                        if let Some(q) = self.lower_scoreboard_query_call(callee, args)? {
                            return Ok(q);
                        }
                        // Composite-component queue value-queries:
                        // `checker.sb.errors.size()` / `.empty()`.
                        // (`.pop()` mutates → statement-only; rejected here.)
                        if let Some(q) = self.lower_component_queue_query(callee, args)? {
                            return Ok(q);
                        }
                        // Bound-to target-responder queue state-field
                        // value-queries: `pending.size()` / `.empty()`
                        // (bare field name inside a responder body).
                        // (`.pop()` mutates → statement-only; rejected here.)
                        self.reject_inactive_target_state_root(target)?;
                        if let Some(q) = self.lower_state_queue_query(callee, args)? {
                            return Ok(q);
                        }
                        // Test-scope target-responder queue state read:
                        // `target.pending.size()` / `.empty()` (fully
                        // resolved instance). (`.pop()` → statement-only.)
                        if let Some(q) = self.lower_test_state_queue_query(callee, args)? {
                            return Ok(q);
                        }
                        // Testbench helper method call (`_tb.reset()`),
                        // CFG-inlined like an impure helper.
                        if let Some(m) = self.tb_method_call_name(callee) {
                            return self.lower_tb_method_call(&m, args);
                        }
                        // Bus calls (tlm_method / send / recv) suspend,
                        // so they are statement-level only — `let x =
                        // bus.m(...)` and `x = bus.m(...)` lower via
                        // `try_lower_bus_call`; anything nested deeper
                        // gets this precise rejection.
                        if let Some(bind) = self.bus_call_root(callee) {
                            return Err(not_implemented(
                                "bus method calls in expression position",
                                format!(
                                    "only `let x = {bind}.{}(...)` and statement \
                                     position are lowered; v1 also rejects the nested form",
                                    name.name
                                ),
                                V1Status::Rejects,
                            ));
                        }
                        // Transactor method calls are call EDGES that may
                        // advance simulated time (v1 hookables run
                        // synchronously — their internal `wait`s `tick()`
                        // directly). In expression position the edge is
                        // value-bearing (`(helper.read(0) & 1) == 1`): we
                        // build the call edge here and let `hoist_ports`
                        // pull it into a `Stmt::TransactorCall { dest:
                        // Some(temp), .. }` in the SAME left-to-right pass
                        // as DUT-port reads, so the `tick()` lands in
                        // source order and the seam rule (a TransactorMethod
                        // edge only ever lives in a TransactorCall stmt or a
                        // top-level Assign RHS) is preserved. A void method
                        // used as a value is rejected by `lower_transactor_call`
                        // (`need_ret = true`), mirroring v1's C++ type error.
                        if self.as_transactor_call(callee)?.is_some() {
                            if self.in_fmt_args {
                                return Err(unsupported(
                                    &format!(
                                        "transactor method call `.{}(...)` inside a message",
                                        name.name
                                    ),
                                    "log/fail messages evaluate lazily; hoist the call into \
                                     a `let` first",
                                ));
                            }
                            if let Some(call) = self.lower_transactor_call(callee, args, true)? {
                                return Ok(call);
                            }
                        }
                        format!("transactor/method call `.{}(...)`", name.name)
                    }
                    _ => "a call expression".to_string(),
                };
                Err(unsupported(&what, ""))
            }
            ExprKind::ForkCall { .. } => Err(unsupported(
                "`fork` bus-method calls in expression position",
                "test-scope `let x = fork bus.m(...)` (initiator-side issue) IS lowered; a \
                 `fork` INSIDE a transactor responder body (target re-issuing a downstream \
                 TLM call — fork-forwarding) is a follow-up slice",
            )),
            // `let ok = randomize(t)` — the value-producing form. v1's
            // `emit_expr` has no arm for it (it only handles the
            // STATEMENT form), so v1 hits its "expression not supported
            // in v0 cpp_tb" fallback. The statement form IS lowered by
            // TB-IR (`Terminator::Randomize` + the constraint-IR seam).
            ExprKind::Randomize { .. } => Err(not_implemented(
                "`randomize` in expression position",
                "the statement form (`randomize(t)` / `randomize(t) with ... end randomize`) \
                 is lowered; a value-producing `let ok = randomize(t)` is not",
                V1Status::Rejects,
            )),
            ExprKind::Cast { expr, ty } => {
                // `e as uint<W>` / `as sint<W>` / `as bits<W>` (W ≤ 64)
                // is a width relabel: v1 emits `((uint64_t)(e))` (the
                // C type for every width ≤ 64 is the same 64-bit
                // integer), so the value is unchanged in the IR's
                // uint64 local model. The annotation still feeds the
                // width-method receiver inference (done on the AST at
                // the call site). Anything else stays rejected.
                if cast_relabel_width(ty).is_some() {
                    let width = cast_relabel_width(ty).expect("checked above");
                    let kind = match ty {
                        TypeExpr::Builtin {
                            name: BuiltinTy::SInt | BuiltinTy::SIntCap,
                            ..
                        } => WidthCastKind::Sext,
                        _ => WidthCastKind::Zext,
                    };
                    // An explicit `as sint<W>` is a signedness relabel, not
                    // a sign extension: it must preserve the 64-bit value
                    // even when the source expression has a narrower
                    // declared width. Keep the target width as the source
                    // width metadata so TBIR can select signed operators
                    // without applying a value-changing extension.
                    let src_width = if matches!(kind, WidthCastKind::Sext) {
                        Some(width)
                    } else {
                        // `Some(0)` (a `uint<0>` receiver) is not usable
                        // emission metadata — see `infer_expr_width`.
                        self.infer_expr_width(expr).filter(|w| *w > 0)
                    };
                    let inner = self.lower_expr(expr)?;
                    if matches!(self.expr_type(&inner), Some(IrType::FixedVec { .. })) {
                        return Err(not_implemented(
                            "a scalar cast applied to a fixed-vector local",
                            "select a scalar lane before casting",
                            V1Status::EmitsUncompilable,
                        ));
                    }
                    if matches!(self.expr_type(&inner), Some(IrType::Seq(_))) {
                        return Err(not_implemented(
                            "a scalar cast applied to a dynamic-list local",
                            "index the list before casting",
                            V1Status::EmitsUncompilable,
                        ));
                    }
                    return Ok(Expr::WidthCast {
                        kind,
                        width,
                        src_width,
                        inner: Box::new(inner),
                    });
                }
                // v1 has no emission for a non-scalar cast: it drops
                // the cast and emits the operand alone, so the value is
                // silently the un-cast one.
                Err(not_implemented(
                    "`as` casts outside scalar uint/sint/bits (≤ 64 bits)",
                    "",
                    V1Status::SilentlyMisLowers,
                ))
            }
            ExprKind::Index { target, index } => {
                // Fixed-vector locals and dynamic sequences share the same
                // emitted `<local>[index]` shape. The verifier recovers the
                // exact element type from the receiver local.
                if let ExprKind::Ident(id) = &*target.kind {
                    if let Some(local) = self.lookup(&id.name) {
                        if let IrType::FixedVec { len, .. } = self.local_type(local) {
                            let len = *len;
                            let index = self.lower_expr(index)?;
                            check_literal_vec_index_bounds(&id.name, &index, len)?;
                            return Ok(Expr::SeqIndex {
                                seq: local,
                                index: Box::new(index),
                            });
                        }
                    }
                }
                if let Some(selected) = self.as_transactor_state_fixed_vec_element(e)? {
                    return Ok(Expr::TransactorStateRecordField {
                        instance: selected.instance,
                        field: selected.field,
                        path: Vec::new(),
                        mid_indices: selected.mid_indices,
                        index: Some(Box::new(selected.index)),
                    });
                }
                if let Some(mut chain) = self.as_transactor_state_record_field(target)? {
                    if matches!(chain.leaf_ty, IrType::Seq(_)) {
                        let dotted = format!("{}.{}", chain.field, chain.path.join("."));
                        return Err(dynamic_record_list_index(&dotted));
                    }
                    if let Some(previous) = chain.leaf_index.take() {
                        let Some(len) = chain.leaf_vec_len else {
                            return Err(not_implemented(
                                "indexing past a scalar responder record-state field",
                                "only nested `Vec<T, N>` layers are indexable",
                                V1Status::EmitsUncompilable,
                            ));
                        };
                        check_literal_vec_index_bounds(&chain.field, &previous, len)?;
                        chain.mid_indices.push((chain.path.len() - 1, previous));
                        match chain.leaf_ty {
                            IrType::FixedVec { len, ref elem } => {
                                chain.leaf_vec_len = Some(len);
                                chain.leaf_ty = (**elem).clone();
                            }
                            _ => chain.leaf_vec_len = None,
                        }
                    }
                    if let Some(len) = chain.leaf_vec_len {
                        let idx = self.lower_expr(index)?;
                        check_literal_vec_index_bounds(
                            &format!("{}.{}", chain.field, chain.path.join(".")),
                            &idx,
                            len,
                        )?;
                        chain.leaf_index = Some(idx);
                        return Ok(Expr::TransactorStateRecordField {
                            instance: chain.instance,
                            field: chain.field,
                            path: chain.path,
                            mid_indices: chain.mid_indices,
                            index: chain.leaf_index.map(Box::new),
                        });
                    }
                }
                // `v[i][j]` — nested fixed-vector element read. The
                // outer `Index`'s target is itself `v[i]`; resolve `v`
                // to a vec whose element is a scalar-leaf `FixedVec` and
                // carry both indices. A deeper `FixedVec` leaf (triple
                // nesting) does not match — `v[i][j]` would still be a
                // vector, refused as a whole-inner-vec use downstream.
                if let ExprKind::Index {
                    target: inner_t,
                    index: outer_idx,
                } = &*target.kind
                {
                    if let Some((base, field, vec)) = self.as_component_vec_field(inner_t)? {
                        if let IrType::FixedVec {
                            len: inner_len,
                            elem: inner_elem,
                        } = &vec.elem
                        {
                            if !matches!(**inner_elem, IrType::FixedVec { .. }) {
                                let outer = self.lower_expr(outer_idx)?;
                                check_literal_component_vec_index_bounds(
                                    &base, &field, &outer, vec.len,
                                )?;
                                let inner = self.lower_expr(index)?;
                                let inner_len = *inner_len;
                                check_literal_component_vec_index_bounds(
                                    &base, &field, &inner, inner_len,
                                )?;
                                return Ok(Expr::ComponentVecElement {
                                    base,
                                    field,
                                    index_pos: 0,
                                    index: Box::new(outer),
                                    inner_index: Some(Box::new(inner)),
                                });
                            }
                        }
                    }
                }
                if let Some((base, field, vec)) = self.as_component_vec_field(target)? {
                    let index = self.lower_expr(index)?;
                    check_literal_component_vec_index_bounds(&base, &field, &index, vec.len)?;
                    return Ok(Expr::ComponentVecElement {
                        base,
                        field,
                        index_pos: 0,
                        index: Box::new(index),
                        inner_index: None,
                    });
                }
                // `mem[i][j]` — nested fixed-vector testbench-field element
                // read. The outer `Index`'s target is `mem[i]`; the base
                // `mem` must resolve to a `Vec` whose element is a
                // scalar-leaf `FixedVec` (a triple-nested leaf does not
                // match, matching the component lane).
                if let ExprKind::Index {
                    target: inner_t,
                    index: outer_idx,
                } = &*target.kind
                {
                    if let Some((field, IrType::FixedVec { elem, len })) =
                        self.as_tb_vec_field(inner_t)
                    {
                        if let IrType::FixedVec {
                            len: inner_len,
                            elem: inner_elem,
                        } = &*elem
                        {
                            if !matches!(**inner_elem, IrType::FixedVec { .. }) {
                                let outer = self.lower_expr(outer_idx)?;
                                check_literal_tb_vec_index_bounds(&field, &outer, len)?;
                                let inner = self.lower_expr(index)?;
                                check_literal_tb_vec_index_bounds(&field, &inner, *inner_len)?;
                                return Ok(Expr::TbFieldVecElement {
                                    field,
                                    index: Box::new(outer),
                                    inner_index: Some(Box::new(inner)),
                                });
                            }
                        }
                    }
                }
                // `mem[i]` — single fixed-vector testbench-field element
                // read (`_tb.mem[i]`).
                if let Some((field, IrType::FixedVec { len, .. })) = self.as_tb_vec_field(target) {
                    let index = self.lower_expr(index)?;
                    check_literal_tb_vec_index_bounds(&field, &index, len)?;
                    return Ok(Expr::TbFieldVecElement {
                        field,
                        index: Box::new(index),
                        inner_index: None,
                    });
                }
                // `a.data[i]` — element read of a `Vec<T, N>` LEAF
                // inside a component record field, the spelling every
                // whole-`Vec` diagnostic in this lane tells the user to
                // write. It has to lower, or the advice is a dead end:
                // v1 emits `int64_t z = self.a.data[0];` and g++ accepts
                // it, so refusing it also carried a false
                // `EmitsUncompilable`. Renders through the same
                // `ComponentVecElement` node a fixed-vector component
                // FIELD uses; the only difference is that `field` is a
                // dotted member suffix (`a.data`) rather than one name.
                if let Some(rf) = self.as_component_record_field(target)? {
                    if matches!(rf.leaf_ty, IrType::Seq(_)) {
                        return Err(dynamic_record_list_index(&rf.dotted));
                    }
                    if let Some((len, _)) = rf.leaf_vec {
                        let index = self.lower_expr(index)?;
                        check_literal_component_vec_index_bounds(
                            &rf.base, &rf.dotted, &index, len,
                        )?;
                        let index_pos = rf.field.split('.').count() - 1;
                        return Ok(Expr::ComponentVecElement {
                            base: rf.base,
                            field: rf.field,
                            index_pos,
                            index: Box::new(index),
                            inner_index: None,
                        });
                    }
                }
                // `rec.data[i]` — element read of a `Vec<T, N>` record
                // field. The target is a record-field access on a
                // record-typed local; lower it to an indexed
                // `Expr::RecordField`.
                if let Some(rf) = self.lower_record_vec_index(target, index)? {
                    return Ok(rf);
                }
                // DUT port lane access: `dut.<port>[i]` (constant or
                // runtime index).
                if let Some(port) = self.as_lane_port_ref(e)? {
                    return Ok(Expr::Port(port));
                }
                // Same shape as the field-access leftovers: v1 emits the
                // subscript verbatim, so `let b = a[0]` on a scalar
                // local becomes `int64_t b = a[0];` — subscripting an
                // integer, which the C++ compiler rejects.
                self.reject_scoreboard_list_index(e, "read")?;
                self.reject_indexed_component_record_path(e, "a read through")?;
                Err(not_implemented(
                    "index expressions",
                    "only `dut.<port>[i]` lane accesses and \
                     `<rec>.<vecfield>[i]` element reads are lowered",
                    V1Status::EmitsUncompilable,
                ))
            }
            ExprKind::BitSlice { target, hi, lo } => {
                // Constant scalar bit-slice `x[hi:lo]` with literal bounds
                // → IR `BitSlice` (right-shift + mask), mirroring v1's
                // scalar slice. Non-literal bounds (`x[i:0]`) keep the
                // width unknown, so they take the runtime-helper form
                // instead — the shape v1 emits for every slice.
                match (parse_int_literal_expr(hi), parse_int_literal_expr(lo)) {
                    (Some(h), Some(l)) if h >= l => match (u32::try_from(h), u32::try_from(l)) {
                        (Ok(hi), Ok(lo)) => {
                            let target = Box::new(self.lower_expr(target)?);
                            Ok(Expr::BitSlice { target, hi, lo })
                        }
                        // v1 casts the bound to `uint32_t` with no
                        // range check, so a bound past 2^32 silently
                        // wraps and slices the wrong bits.
                        _ => Err(not_implemented(
                            "bit-slice bounds above 2^32",
                            "",
                            V1Status::SilentlyMisLowers,
                        )),
                    },
                    // Both bounds literal, but reversed. This is not a
                    // missing feature in either backend: `x[0:3]` names
                    // no bits. v1 accepts it and emits
                    // `harc_bits(v, 0, 3)`, whose `hi < lo` guard
                    // returns 0 — a silent always-zero read. Reject it
                    // here as the malformed slice it is.
                    (Some(h), Some(l)) => Err(LowerError::Invalid(format!(
                        "bit slice `[{h}:{l}]` is reversed: a slice names bits high-to-low, \
                         so the first bound must be >= the second (write `[{l}:{h}]` to take \
                         those {} bits)",
                        l - h + 1
                    ))),
                    // At least one bound is a runtime value. The width
                    // is unknown at lowering, so this takes the runtime
                    // `harc_bits` helper — which is what v1 emits for
                    // every slice, constant bounds included.
                    _ => {
                        let target = Box::new(self.lower_expr(target)?);
                        let hi = Box::new(self.lower_expr(hi)?);
                        let lo = Box::new(self.lower_expr(lo)?);
                        Ok(Expr::BitSliceDyn { target, hi, lo })
                    }
                }
            }
            // A bare string literal in expression position has no
            // v1-supported landing surface: v1's `local_value_c_type` for a
            // `let s : String` routes through `record_field_c_type ->
            // txn_field_c_type`, which lacks a `BuiltinTy::String` case and
            // falls through to `uint64_t` — emitting `uint64_t s = "...";`,
            // a C++ compile error. (The `const char*` mapping in
            // `c_type_for` only applies to method *params*, never lets.)
            // And `${s}` interpolation always emits `%lld` +
            // `harc_printf_ll`, which also fails for a pointer. Since v1
            // cannot compile ANY string-valued local, lowering it in tbir
            // would diverge from v1 rather than mirror it — keep it out of
            // subset until v1 grows a real string-local surface (audit #425
            // deferral). String *interpolation* (`${...}`) and `log`/`logf`
            // format strings are separate statement-level paths that work.
            // HARC's value slot is a 64-bit integer; a string is only a
            // `log`/`fail` message operand. v1 emits the literal into an
            // integer slot (`int64_t s = "hello";`), which is a C++
            // compile error.
            ExprKind::String(_) => Err(not_implemented(
                "a string value in expression position",
                "strings are `log`/`fail`/`logf` message operands, not values",
                V1Status::EmitsUncompilable,
            )),
            // Same integer value slot: v1 emits `int64_t f = 1.5;`, which
            // COMPILES and silently truncates to 1.
            ExprKind::Float(_) => Err(not_implemented(
                "a float literal",
                "HARC values are integers; scale to a fixed-point integer instead",
                V1Status::SilentlyMisLowers,
            )),
            ExprKind::Time(s) => {
                // Bare `time` value in expression position (`let t : time =
                // 100ns`). v1's `emit_expr_with_arrow` emits the leading
                // numeric portion verbatim (no unit conversion) and types
                // the local `uint64_t`. We mirror that for the common case:
                // take the digit/underscore prefix, strip underscores, parse
                // as u64. (This is NOT the `wait <dur>` path, which converts
                // to ps via `time_literal_to_ps` — a different surface.)
                //
                // INTENTIONAL DIVERGENCE from v1 (authorized 2026-06-19, see
                // the "Time-literal digit separators" note in tbir-mvp.md):
                // for a digit-separated literal like `1_000ns`, v1 emits the
                // prefix verbatim — `uint64_t t = 1_000;` — which is a C++
                // compile error (no `operator""_000`). We strip the `_` and
                // lower `1000`, which is what the source plainly means. tbir
                // is the more-correct backend here; v1's behavior is a legacy
                // limitation, not a contract we preserve.
                let digits: String = s
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '_')
                    .filter(|c| *c != '_')
                    .collect();
                let value = digits
                    .parse::<u64>()
                    .map_err(|_| {
                        LowerError::Invalid(
                            "time literal has no leading numeric value".to_string(),
                        )
                    })?;
                Ok(Expr::Literal {
                    value,
                    ty: IrType::UInt(Some(64)),
                })
            }
            // `$past(x)` and friends in system-call syntax. Same rule as
            // the plain-call spelling above: only meaningful inside a
            // concurrent check body, which intercepts them by span.
            ExprKind::SystemCall { name, .. } => match name {
                // v1's `emit_expr` has no `SystemCall` arm at all, so
                // `clog2(x)` reaches its "expression not supported"
                // fallback there.
                crate::ast::SystemFn::Clog2 => Err(not_implemented(
                    "`clog2`",
                    "fold the value at the call site, or bind it to a `const`",
                    V1Status::Rejects,
                )),
                _ => Err(LowerError::Invalid(format!(
                    "`{}` is a temporal reading; it is only meaningful inside a concurrent \
                     `assert`/`assume`/`cover` body, and cannot be nested inside another \
                     temporal reading",
                    match name {
                        crate::ast::SystemFn::Past => "past",
                        crate::ast::SystemFn::Rose => "rose",
                        crate::ast::SystemFn::Fell => "fell",
                        _ => "stable",
                    }
                ))),
            },
            // Every arm below is a form that only means something in a
            // position this function is NOT: a constraint body (which the
            // typed constraint backend lowers, never `lower_expr`), a
            // temporal property body, or a cover-sequence pattern. Reaching
            // here means the construct was written in ordinary VALUE
            // position, where v1's `emit_expr` has no arm for it either —
            // hence `not_implemented`, not a v1 escape hatch.
            ExprKind::StructLit { .. } => Err(not_implemented(
                "a struct literal in value position",
                "assign the fields of a `let s : <Struct>` instead",
                V1Status::Rejects,
            )),
            ExprKind::SetLit(_) => Err(not_implemented(
                "a set literal in value position",
                "set literals are a constraint form (`keep x inside {…}`)",
                V1Status::Rejects,
            )),
            ExprKind::DistLit(_) | ExprKind::DistDirective { .. } => Err(not_implemented(
                "a `dist` literal in value position",
                "`dist` is a constraint form — it lowers inside `randomize ... with`",
                V1Status::Rejects,
            )),
            // v1 DOES have a `RangeLit` arm, but it emits
            // `/* range a..b */ 0` — the range silently becomes zero.
            ExprKind::RangeLit { .. } => Err(not_implemented(
                "a range expression in value position",
                "ranges are a `for`/`inside` form, not a value",
                V1Status::SilentlyMisLowers,
            )),
            ExprKind::Membership { .. } => Err(not_implemented(
                "an `in` membership test in value position",
                "membership is a constraint form — it lowers inside `randomize ... with`",
                V1Status::Rejects,
            )),
            // v1 emits the shorthand verbatim (`int64_t x = .a;`),
            // which is not valid C++.
            ExprKind::ImplicitSelf => Err(not_implemented(
                "`.field` shorthand",
                "name the receiver explicitly",
                V1Status::EmitsUncompilable,
            )),
            ExprKind::Send { .. } => Err(not_implemented(
                "a `<-` send in value position",
                "`<-` is a statement form (`target <- value`)",
                V1Status::Rejects,
            )),
            ExprKind::HashHash { .. } | ExprKind::SeqRepeat { .. } => Err(not_implemented(
                "a temporal sequence operator (`##N`, `[*N]`)",
                "sequence operators belong in a `pseq` / property body; no backend lowers \
                 them yet, in any position",
                V1Status::Rejects,
            )),
            ExprKind::NamedArg { .. } => Err(not_implemented(
                "a named argument",
                "pass arguments positionally",
                V1Status::Rejects,
            )),
            ExprKind::CoverArrow { .. } => Err(not_implemented(
                "a cover-sequence pattern (`a => b`)",
                "behavioral sequence coverage (spec §17.3) is parsed but not lowered by any \
                 backend",
                V1Status::Rejects,
            )),
            ExprKind::SolveOrder { .. } => Err(not_implemented(
                "`solve_order` in value position",
                "`solve_order` is a constraint form — it lowers inside `randomize ... with`",
                V1Status::Rejects,
            )),
            ExprKind::SoftConstraint(_) => Err(not_implemented(
                "`soft` in value position",
                "`soft` is a constraint form — it lowers inside `randomize ... with`",
                V1Status::Rejects,
            )),
            ExprKind::ForEachConstraint { .. } => Err(not_implemented(
                "a constraint `for` comprehension in value position",
                "`keep for x in ...` is a constraint form — it lowers inside a \
                 `transaction` body or `randomize ... with`",
                V1Status::Rejects,
            )),
        }
    }

    /// Resolve an AST field-access chain `ident.f1.f2...fn` rooted at a
    /// record-typed local into a [`RecordFieldChain`], descending through
    /// nested `IrType::Record` fields to reach the leaf. A NON-leaf
    /// segment may carry an element selection on a `Vec<Record, N>` field
    /// (`tbl.entries[i].tag`, at any depth) — collected into
    /// `mid_indices`; the descent then continues through the element
    /// record. Returns:
    ///   - `Ok(None)` when `e` is not a field access rooted at an `Ident`
    ///     bound to a record local (the caller falls through to the other
    ///     lanes: DUT signal, scoreboard, recv payload, …);
    ///   - `Err` when it IS such a chain but a component names no field, or
    ///     a non-leaf component is not a nested record (so it cannot be
    ///     descended into), or an element selection sits on a non-`Vec`
    ///     field / a `Vec` of records is traversed without one.
    pub(crate) fn try_record_field_chain(
        &mut self,
        e: &AstExpr,
    ) -> Result<Option<RecordFieldChain>, LowerError> {
        // Flatten `a.b.c` → root `a`, segments `[b, c]` (outer-to-inner
        // during the walk, reversed to declaration order after). An
        // `Index` node between segments records a pending element
        // selection that attaches to the NEXT (inner) `Field` segment:
        // in `tbl.entries[i].tag` the walk sees `.tag`, then `[i]`, then
        // `.entries` — so `[i]` belongs to `entries`.
        let mut segs: Vec<String> = Vec::new();
        // `(push-order seg position, index AST)` per element selection.
        let mut raw_indices: Vec<(usize, &AstExpr)> = Vec::new();
        let mut pending_indices: Vec<&AstExpr> = Vec::new();
        let mut cur = e;
        let root = loop {
            match &*cur.kind {
                ExprKind::Field { target, name } => {
                    for idx in pending_indices.drain(..).rev() {
                        raw_indices.push((segs.len(), idx));
                    }
                    segs.push(name.name.clone());
                    cur = target;
                }
                ExprKind::Index { target, index } => {
                    pending_indices.push(index);
                    cur = target;
                }
                ExprKind::Ident(root) => {
                    if !pending_indices.is_empty() {
                        // `ident[i].f` — the root local itself is indexed;
                        // not a record-field chain (lane ports and seq
                        // element reads route elsewhere).
                        return Ok(None);
                    }
                    break root;
                }
                // Innermost target is not a bare ident (`f().x`, …):
                // not a record-local chain this lane handles.
                _ => return Ok(None),
            }
        };
        if segs.is_empty() {
            return Ok(None); // bare ident, no field access
        }
        segs.reverse();
        let mut field_start = 0usize;
        let local = if let Some(local) = self.lookup(&root.name) {
            local
        } else if let Some(tb_field) = self.ctx.tb_field.as_deref() {
            if root.name == tb_field {
                let Some(tb_record) = segs.first() else {
                    return Ok(None);
                };
                let Some(local) = self.lookup_tb_record_field_in_capture_scope(tb_record) else {
                    return Ok(None);
                };
                field_start = 1;
                local
            } else {
                return Ok(None);
            }
        } else {
            return Ok(None);
        };
        let Some(mut cur_rid) = self.record_of_local(local) else {
            return Ok(None);
        };
        // Component record fields have mirror locals in method lowering,
        // but indexed accesses must retain their component base so mode and
        // per-instance semantics stay intact. The component-specific lane
        // already handles its nested fixed-vector indexes.
        if field_start == 0
            && !raw_indices.is_empty()
            && self.ctx.component_fields.contains_key(&root.name)
        {
            return Ok(None);
        }
        if field_start >= segs.len() {
            return Ok(None);
        }
        // Convert element selections from push-order to declaration-order
        // positions relative to `fields`. An index landing BELOW
        // `field_start` selects on the record local itself (`_tb.cur[i]…`)
        // — not this lane's chain. Checked for every entry before any
        // index lowers, so the fall-through leaves no hoisted temps.
        let total = segs.len();
        if raw_indices.iter().any(|(p, _)| total - 1 - p < field_start) {
            return Ok(None);
        }
        // Lower the index expressions left-to-right (chain order — the
        // walk collected them inner-to-outer), so hoisted statements keep
        // source order.
        let mut mid_indices: Vec<(usize, Expr)> = Vec::with_capacity(raw_indices.len());
        let mut ordered_indices: Vec<(usize, &AstExpr)> = raw_indices
            .into_iter()
            .map(|(raw_pos, idx)| ((total - 1 - raw_pos) - field_start, idx))
            .collect();
        ordered_indices.sort_by_key(|(position, _)| *position);
        for (pos, idx_ast) in ordered_indices {
            let idx = self.lower_record_path_index(idx_ast)?;
            mid_indices.push((pos, idx));
        }
        let mut dotted = self.ctx.records[cur_rid.index()].name.clone();
        // The user's own root: the local's name, or — under the `_tb`
        // testbench-field prefix, where `field_start` skipped a segment
        // — the bare field name they actually wrote.
        let mut spelled = if field_start == 0 {
            root.name.clone()
        } else {
            segs[field_start - 1].clone()
        };
        let fields = &segs[field_start..];
        let last = fields.len() - 1;
        let mut leaf_vec_len = None;
        let mut leaf_ty = IrType::Bool; // overwritten at the leaf
        for (i, seg) in fields.iter().enumerate() {
            let schema = &self.ctx.records[cur_rid.index()];
            let Some(fld) = schema.field(seg) else {
                return Err(LowerError::Invalid(format!(
                    "record `{}` has no field `{seg}`",
                    schema.name
                )));
            };
            dotted.push('.');
            dotted.push_str(seg);
            spelled.push('.');
            spelled.push_str(seg);
            // Mid-path element selections belong in the user's own
            // spelling: without them `r.tbl[0].data` came back as
            // `r.tbl.data[i]`, which resolves to a different field and
            // is refused by a different diagnostic. The index
            // EXPRESSION is not recoverable here (it has been lowered
            // to IR), so it renders as `[…]` — a placeholder that
            // cannot be mistaken for something to paste, rather than a
            // path that looks typeable and is not.
            if mid_indices.iter().any(|(p, _)| *p == i) {
                spelled.push_str("[…]");
            }
            if i == last {
                // An enclosing element lane peels the final index, but its
                // target can itself contain one or more selections
                // (`r.matrix[i]` in `r.matrix[i][j]`). Preserve those at
                // the leaf position and consume one fixed-vector layer per
                // selection so the caller sees the remaining inner shape.
                leaf_vec_len = fld.vec_len;
                leaf_ty = fld.ty.clone();
                for (_, idx) in mid_indices.iter().filter(|(p, _)| *p == i) {
                    let Some(len) = leaf_vec_len else {
                        return Err(not_implemented(
                            &format!("indexing the scalar record field `{dotted}`"),
                            "only nested `Vec<T, N>` layers are indexable",
                            V1Status::EmitsUncompilable,
                        ));
                    };
                    check_literal_vec_index_bounds(&dotted, idx, len)?;
                    match leaf_ty {
                        IrType::FixedVec { len, ref elem } => {
                            leaf_vec_len = Some(len);
                            leaf_ty = (**elem).clone();
                        }
                        _ => leaf_vec_len = None,
                    }
                }
                break;
            }
            let index_count = mid_indices.iter().filter(|(p, _)| *p == i).count();
            let indexed = index_count != 0;
            // A non-leaf component must reach a nested record to descend
            // into: either a plain nested-record field, or one element of
            // a `Vec<Record, N>` field selected by `[i]`.
            match fld.ty {
                IrType::Record(next) if fld.vec_len.is_none() && !indexed => cur_rid = next,
                IrType::Record(next) if fld.vec_len.is_some() && index_count == 1 => {
                    if let Some((_, idx)) = mid_indices.iter().find(|(p, _)| *p == i) {
                        check_literal_vec_index_bounds(&dotted, idx, fld.vec_len.unwrap_or(0))?;
                    }
                    cur_rid = next;
                }
                _ if indexed && fld.vec_len.is_none() => {
                    // The fourth arm of this loop, missed when its three
                    // siblings were swept. Same criterion, same answer:
                    // `r.inner[0].p` emits from v1 and g++ refuses ("no
                    // match for `operator[]` (`Kid` and `int`)"), and
                    // `r.n[0].p` gives "invalid types `uint64_t[int]` for
                    // array subscript". The precedent is the OUTERMOST-index
                    // spelling `r.n[0]`, which already carries this verdict.
                    return Err(not_implemented(
                        &format!("indexing the non-`Vec` record field `{dotted}`"),
                        "only `Vec<T, N>` record fields are indexable".to_string(),
                        V1Status::EmitsUncompilable,
                    ));
                }
                _ if index_count > 1 => {
                    return Err(not_implemented(
                        &format!("indexing past the record element of `{dotted}`"),
                        "select fields on the record element after its single `Vec` index",
                        V1Status::EmitsUncompilable,
                    ));
                }
                _ if indexed => {
                    return Err(not_implemented(
                        &format!(
                            "field access `.{}` on an element of `{dotted}`, \
                             whose elements are scalars",
                            fields[i + 1]
                        ),
                        "only `Vec` fields with struct/transaction elements can be \
                         traversed further"
                            .to_string(),
                        V1Status::EmitsUncompilable,
                    ));
                }
                IrType::Record(_) if fld.vec_len.is_some() => {
                    return Err(not_implemented(
                        &format!(
                            "traversing the `Vec` record field `{dotted}` without an \
                             element index; cannot access `.{}`",
                            fields[i + 1]
                        ),
                        format!("select one element first (`{seg}[i].{}`)", fields[i + 1]),
                        V1Status::EmitsUncompilable,
                    ));
                }
                _ => {
                    return Err(not_implemented(
                        &format!(
                            "field access `.{}` on `{}.{seg}`, which is not a nested record",
                            fields[i + 1],
                            schema.name
                        ),
                        "only nested struct/transaction fields can be traversed further"
                            .to_string(),
                        V1Status::EmitsUncompilable,
                    ));
                }
            }
        }
        let field = fields[0].clone();
        let path = fields[1..].to_vec();
        Ok(Some(RecordFieldChain {
            local,
            field,
            path,
            mid_indices,
            leaf_vec_len,
            leaf_ty,
            dotted,
            spelled,
        }))
    }

    /// `Some(rid)` when `e` is a *whole* record value (a record-typed local,
    /// a whole nested-record field read, or one `Vec<Record, N>` element —
    /// `tbl.entries[i]`). Used to validate a whole-record field assignment
    /// (`o.a = d`) and record-typed `let`/copy RHS shapes.
    pub(crate) fn record_id_of_expr(&self, e: &Expr) -> Option<RecordId> {
        match e {
            Expr::Local(l) => self.record_of_local(*l),
            Expr::RecordField {
                local,
                field,
                path,
                mid_indices,
                index,
            } => {
                let mut cur = self.record_of_local(*local)?;
                let segs: Vec<&String> = std::iter::once(field).chain(path.iter()).collect();
                let last = segs.len() - 1;
                for (i, seg) in segs.iter().enumerate() {
                    let fld = self.ctx.records.get(cur.index())?.field(seg)?;
                    let selection_count = mid_indices.iter().filter(|(p, _)| *p == i).count()
                        + usize::from(i == last && index.is_some());
                    if i == last {
                        return match selected_record_leaf_type(
                            fld.vec_len,
                            &fld.ty,
                            selection_count,
                        ) {
                            Some(IrType::Record(record)) => Some(record),
                            _ => None,
                        };
                    }
                    let indexed = selection_count != 0;
                    // A record value at each step: a plain nested-record
                    // field, or one indexed `Vec<Record, N>` element. A
                    // whole (unindexed) `Vec` leaf is an array, not a
                    // record value.
                    match fld.ty {
                        IrType::Record(r)
                            if (fld.vec_len.is_none() && !indexed)
                                || (fld.vec_len.is_some() && selection_count == 1) =>
                        {
                            cur = r
                        }
                        _ => return None,
                    }
                }
                None
            }
            Expr::ScoreboardQuery {
                sb,
                query: crate::ir::ScoreboardQuery::Scalar { scalar },
                ..
            } => self
                .ctx
                .scoreboards
                .get(sb.index())?
                .field(scalar)
                .and_then(|field| match field.kind {
                    crate::ir::ScoreboardFieldKind::Record { record } => Some(record),
                    _ => None,
                }),
            // One element of a component-record `Vec<Record, N>` leaf
            // (`a.kids[i]`) is a whole record value, exactly as
            // `tbl.entries[i]` is on a record local. Answered through
            // `expr_type` so the two walks cannot disagree — without
            // this arm `a.small[0] = b.kids[0]` passed
            // `reject_record_into_scalar` and emitted
            // `self.a.small[0] = self.b.kids[0];`, which g++ refuses.
            Expr::ComponentVecElement { .. } => match self.expr_type(e) {
                Some(IrType::Record(r)) => Some(r),
                _ => None,
            },
            // A whole-record read of a target-transactor state field
            // (`responder.last` / bare `last`) — resolve via the instance's
            // (or the responder body's) state-field table.
            Expr::TransactorState { instance, field } => {
                let kind = if instance.is_empty() {
                    self.target_state_fields.get(field)
                } else {
                    self.ctx.target_state.get(instance)?.get(field)
                };
                match kind {
                    Some(crate::ir::StateFieldKind::Record { record }) => Some(*record),
                    _ => None,
                }
            }
            // A nested whole-record subfield read of a state record
            // (`responder.last.inner`, where `inner` is itself a record).
            Expr::TransactorStateRecordField {
                instance,
                field,
                path,
                mid_indices,
                index,
            } => {
                // A direct fixed-vector state element uses this same IR with
                // an empty record path. `expr_type` already walks every
                // nested selection against the FixedVec schema; ask it so
                // record assignment/equality lanes see the declared leaf.
                if path.is_empty() {
                    return match self.expr_type(e) {
                        Some(IrType::Record(record)) => Some(record),
                        _ => None,
                    };
                }
                let kind = if instance.is_empty() {
                    self.target_state_fields.get(field)
                } else {
                    self.ctx.target_state.get(instance)?.get(field)
                };
                let Some(crate::ir::StateFieldKind::Record { record }) = kind else {
                    return None;
                };
                let mut cur = *record;
                let last = path.len().checked_sub(1)?;
                for (i, seg) in path.iter().enumerate() {
                    let fld = self.ctx.records.get(cur.index())?.field(seg)?;
                    let selection_count = mid_indices.iter().filter(|(p, _)| *p == i).count()
                        + usize::from(i == last && index.is_some());
                    if i == last {
                        return match selected_record_leaf_type(
                            fld.vec_len,
                            &fld.ty,
                            selection_count,
                        ) {
                            Some(IrType::Record(record)) => Some(record),
                            _ => None,
                        };
                    }
                    let indexed = selection_count != 0;
                    match fld.ty {
                        IrType::Record(r)
                            if (fld.vec_len.is_none() && !indexed)
                                || (fld.vec_len.is_some() && selection_count == 1) =>
                        {
                            cur = r
                        }
                        _ => return None,
                    }
                }
                None
            }
            // A ternary over records (`b = c ? x : y`). `expr_type`
            // already types this shape one line-for-line the same way
            // (`self.expr_type(t).or_else(|| self.expr_type(e))`);
            // leaving it out here made a whole-record ternary — which
            // v1 emits as `b = (c ? x : y);` and g++ accepts — look
            // like a type error to the assignment guards. Both arms
            // must agree, or the value has no single record type.
            Expr::Ternary(_, t, e) => {
                let a = self.record_id_of_expr(t)?;
                (a == self.record_id_of_expr(e)?).then_some(a)
            }
            // A record-typed COMPOSITE-COMPONENT field (`src.cur`,
            // where `cur : Beat` on an agent). These are first-class —
            // `ComponentFieldKind::Record` — and leaving them untyped
            // here is what made a whole-record write of one look like a
            // type error to the assignment guards.
            Expr::ComponentField { base, field } => self.component_field_record(base, field),
            _ => None,
        }
    }

    /// Boolean-producing operators hide their operand type from `expr_type`.
    /// Same-record equality/inequality is supported by the generated record
    /// operators; every other record operand in a scalar boolean expression is
    /// invalid and must be diagnosed before TB-IR verification.
    pub(crate) fn bool_expr_has_invalid_record_operand(&self, e: &Expr) -> bool {
        match e {
            Expr::Unary(_, inner) => {
                self.record_id_of_expr(inner).is_some()
                    || self.bool_expr_has_invalid_record_operand(inner)
            }
            Expr::Binary(op, lhs, rhs) if matches!(op, BinOp::Eq | BinOp::Ne) => {
                match (self.record_id_of_expr(lhs), self.record_id_of_expr(rhs)) {
                    (Some(lhs), Some(rhs)) => lhs != rhs,
                    (Some(_), None) | (None, Some(_)) => true,
                    (None, None) => {
                        self.bool_expr_has_invalid_record_operand(lhs)
                            || self.bool_expr_has_invalid_record_operand(rhs)
                    }
                }
            }
            Expr::Binary(_, lhs, rhs) => {
                self.record_id_of_expr(lhs).is_some()
                    || self.record_id_of_expr(rhs).is_some()
                    || self.bool_expr_has_invalid_record_operand(lhs)
                    || self.bool_expr_has_invalid_record_operand(rhs)
            }
            Expr::Ternary(cond, then_expr, else_expr) => {
                self.record_id_of_expr(cond).is_some()
                    || self.record_id_of_expr(then_expr).is_some()
                    || self.record_id_of_expr(else_expr).is_some()
                    || self.bool_expr_has_invalid_record_operand(cond)
                    || self.bool_expr_has_invalid_record_operand(then_expr)
                    || self.bool_expr_has_invalid_record_operand(else_expr)
            }
            Expr::WidthCast { inner, .. } | Expr::BitSlice { target: inner, .. } => {
                self.record_id_of_expr(inner).is_some()
                    || self.bool_expr_has_invalid_record_operand(inner)
            }
            Expr::BitSliceDyn { target, hi, lo } => [target.as_ref(), hi.as_ref(), lo.as_ref()]
                .iter()
                .any(|inner| {
                    self.record_id_of_expr(inner).is_some()
                        || self.bool_expr_has_invalid_record_operand(inner)
                }),
            _ => false,
        }
    }

    pub(crate) fn validate_truth_expr(&self, e: &Expr, context: &str) -> Result<(), LowerError> {
        if matches!(self.expr_type(e), Some(IrType::FixedVec { .. })) {
            return Err(LowerError::Invalid(format!(
                "{context} must be a scalar value, not a fixed vector"
            )));
        }
        if matches!(self.expr_type(e), Some(IrType::Seq(_))) {
            return Err(LowerError::Invalid(format!(
                "{context} must be a scalar value, not a dynamic list"
            )));
        }
        if let Some(record) = self.record_id_of_expr(e) {
            let name = &self.ctx.records[record.index()].name;
            return Err(LowerError::Invalid(format!(
                "{context} must be a scalar value, not record `{name}`"
            )));
        }
        if self.bool_expr_has_invalid_record_operand(e) {
            return Err(LowerError::Invalid(format!(
                "{context} applies a scalar operator to a record value"
            )));
        }
        Ok(())
    }

    /// The record type of a component field access, or `None` if the
    /// access is not a record.
    ///
    /// `field` is not always a single name. `as_component_record_field`
    /// returns a DOTTED one for a nested subfield read
    /// (`ComponentField { base: SelfField, field: "cur.v" }`), so the
    /// walk mirrors that function's own `validate` closure: the head
    /// names a component field, each further segment a record field,
    /// and anything that is not a traversable record ends the walk.
    /// Answering `None` for every dotted shape made `b = cur.inn` — a
    /// nested RECORD subfield both backends copy as a struct — a type
    /// error, and answering the HEAD's record for one made `b = cur.v`
    /// — a scalar — lower into `b = self.cur.v;`, which g++ refuses.
    ///
    /// The answer is definitive in both directions. Every `field` that
    /// reaches here was already resolved and validated by the site that
    /// built the `ComponentField`, so a segment this walk cannot find
    /// does not occur.
    pub(crate) fn component_field_record(
        &self,
        base: &crate::ir::ComponentBase,
        field: &str,
    ) -> Option<crate::ir::RecordId> {
        let cid = self.component_base_id(base)?;
        let mut segs = field.split('.');
        let schema = self.ctx.components.get(cid.index())?.field(segs.next()?)?;
        let mut rid = match schema.kind {
            crate::ir::ComponentFieldKind::Record { record } => record,
            // A scalar / queue / sub-component field is not a record,
            // and neither is any path through one.
            _ => return None,
        };
        for seg in segs {
            let f = self.ctx.records[rid.index()].field(seg)?;
            match f.ty {
                // `vec_len.is_none()` mirrors `validate`, which refuses
                // a `Vec` MID-segment upstream ("traversing the `Vec`
                // record field … without an element index") — copied so
                // the two walks cannot drift. A `Vec` LEAF does reach
                // here, since the `==`/`!=` landing is permitted, and
                // falls to `None`: `Vec<T, N>` is not a record type, so
                // "no record" is the answer, not a missed rejection.
                crate::ir::IrType::Record(next) if f.vec_len.is_none() => rid = next,
                _ => return None,
            }
        }
        Some(rid)
    }

    /// The component a `ComponentBase` names, when it can be resolved
    /// without a diagnostic. `None` for a receiver this context cannot
    /// resolve — callers must treat that as "cannot tell", never as
    /// "not a record". (This paragraph had been stacked on top of
    /// `component_field_record`'s doc, describing a function two
    /// definitions away.)
    pub(crate) fn component_base_id(
        &self,
        base: &crate::ir::ComponentBase,
    ) -> Option<crate::ir::ComponentId> {
        match base {
            crate::ir::ComponentBase::SelfField => self.self_component,
            crate::ir::ComponentBase::Path(path) => {
                // The INVERSE of `component_path_head`, which builds
                // this base — and it has two head forms, not one. A
                // test-scope instance heads the path by its own name; a
                // sub-component of the METHOD'S OWN component heads it
                // with the literal `"self"`. Resolving only the first
                // made every `src.cur` inside a method body untypable,
                // and `component_path_head` is checked in this same
                // order, `component_fields` first.
                let (head, rest) = path.split_first()?;
                let head_cid = match self.ctx.component_fields.get(head) {
                    Some(cid) => *cid,
                    None if head == "self" => {
                        let cid = self.self_component?;
                        let comp = self.ctx.components.get(cid.index())?;
                        match comp.field(rest.first()?)?.kind {
                            crate::ir::ComponentFieldKind::Sub { component, .. } => {
                                return self.resolve_component_recv(component, &rest[1..]).ok()
                            }
                            _ => return None,
                        }
                    }
                    None => return None,
                };
                self.resolve_component_recv(head_cid, rest).ok()
            }
            // `ComponentBase::Local` (a component-typed method
            // parameter) is built only by `as_component_method_call`,
            // for a CALL receiver. No `Expr::ComponentField` carries
            // one, so there is nothing here to resolve.
            crate::ir::ComponentBase::Local(_) => None,
        }
    }

    /// `rec.data[i]` — element read of a `Vec<T, N>` record field, at any
    /// nesting depth (`s.a.b[i]`). Returns `Some(Expr::RecordField { index })`
    /// when `target` is a field-access chain on a record-typed local whose
    /// leaf is a `Vec`; `None` if `target` is not such a chain (the caller
    /// then tries the DUT-lane and rejection paths). A scalar leaf indexed
    /// like an array is a hard error (a scalar has no elements).
    pub(crate) fn lower_record_vec_index(
        &mut self,
        target: &AstExpr,
        index: &AstExpr,
    ) -> Result<Option<Expr>, LowerError> {
        let Some(chain) = self.try_record_field_chain(target)? else {
            return Ok(None);
        };
        if matches!(chain.leaf_ty, IrType::Seq(_)) {
            return Err(dynamic_record_list_index(&chain.spelled));
        }
        if chain.leaf_vec_len.is_none() {
            // v1 emits the subscript verbatim (`_tb.cur.v[1]`), which
            // subscripts a `uint64_t`.
            return Err(not_implemented(
                &format!("indexing the scalar record field `{}`", chain.dotted),
                "only `Vec<T, N>` record fields are indexable",
                V1Status::EmitsUncompilable,
            ));
        }
        let idx = self.lower_expr(index)?;
        check_literal_vec_index_bounds(&chain.dotted, &idx, chain.leaf_vec_len.unwrap_or(0))?;
        Ok(Some(Expr::RecordField {
            local: chain.local,
            field: chain.field,
            path: chain.path,
            mid_indices: chain.mid_indices,
            index: Some(Box::new(idx)),
        }))
    }

    /// Lower and hoist every surviving `Expr::Port` into a `DutRead`
    /// temp in the current block.
    pub(crate) fn lower_expr_no_ports(&mut self, e: &AstExpr) -> Result<Expr, LowerError> {
        let ir = self.lower_expr(e)?;
        Ok(self.hoist_ports(ir))
    }

    pub(crate) fn hoist_ports(&mut self, e: Expr) -> Expr {
        self.hoist_ports_with_hint(e, None, false)
    }

    /// Ordered interpolation materialization needs an exact host carrier for
    /// a port whose source width is only known later from `--sv` metadata.
    pub(crate) fn hoist_fmt_ports(&mut self, e: Expr) -> Expr {
        self.hoist_ports_with_hint(e, None, true)
    }

    /// Lower an expression with a width hint so a bare DUT-port read in
    /// value position (e.g. a wide TLM method argument `wide.send(dut.payload)`)
    /// hoists into a temp typed at the hint's width instead of the default
    /// u64. Without the hint a `>64-bit` port read would silently truncate.
    pub(crate) fn lower_expr_no_ports_hinted(
        &mut self,
        e: &AstExpr,
        hint: Option<IrType>,
    ) -> Result<Expr, LowerError> {
        let ir = self.lower_expr(e)?;
        Ok(self.hoist_ports_with_hint(ir, hint, false))
    }

    fn hoist_ports_with_hint(
        &mut self,
        e: Expr,
        hint: Option<IrType>,
        exact_untyped_ports: bool,
    ) -> Expr {
        match e {
            Expr::Port(p) => {
                let t = self.fresh_temp();
                if let Some(ty) = port_temp_type(&p, hint.as_ref()) {
                    self.set_local_type(t, ty);
                } else if exact_untyped_ports {
                    self.set_local_type(t, IrType::PortSnapshot);
                }
                self.push(Stmt::DutRead(t, p));
                Expr::Local(t)
            }
            Expr::TbQueueQuery { field, query } => Expr::TbQueueQuery { field, query },
            Expr::DynamicListQuery { target, query } => Expr::DynamicListQuery {
                target: Box::new(self.hoist_ports_with_hint(
                    *target,
                    None,
                    exact_untyped_ports,
                )),
                query,
            },
            Expr::Binary(op, a, b) => {
                let a_hint = if matches!(op, BinOp::Eq | BinOp::Ne) {
                    self.expr_type(&b)
                } else {
                    None
                };
                let b_hint = if matches!(op, BinOp::Eq | BinOp::Ne) {
                    self.expr_type(&a)
                } else {
                    None
                };
                let a = self.hoist_ports_with_hint(*a, a_hint, exact_untyped_ports);
                let b = self.hoist_ports_with_hint(*b, b_hint, exact_untyped_ports);
                Expr::Binary(op, Box::new(a), Box::new(b))
            }
            Expr::Unary(op, a) => {
                let a = self.hoist_ports_with_hint(*a, None, exact_untyped_ports);
                Expr::Unary(op, Box::new(a))
            }
            Expr::Ternary(c, t, e) => {
                let c = self.hoist_ports_with_hint(*c, None, exact_untyped_ports);
                let t = self.hoist_ports_with_hint(*t, None, exact_untyped_ports);
                let e = self.hoist_ports_with_hint(*e, None, exact_untyped_ports);
                Expr::Ternary(Box::new(c), Box::new(t), Box::new(e))
            }
            Expr::BitSlice { target, hi, lo } => {
                let target = self.hoist_ports_with_hint(*target, None, exact_untyped_ports);
                Expr::BitSlice {
                    target: Box::new(target),
                    hi,
                    lo,
                }
            }
            Expr::BitSliceDyn { target, hi, lo } => {
                let target = self.hoist_ports_with_hint(*target, None, exact_untyped_ports);
                let hi = self.hoist_ports_with_hint(*hi, None, exact_untyped_ports);
                let lo = self.hoist_ports_with_hint(*lo, None, exact_untyped_ports);
                Expr::BitSliceDyn {
                    target: Box::new(target),
                    hi: Box::new(hi),
                    lo: Box::new(lo),
                }
            }
            Expr::PortSnapshotLane {
                snapshot,
                port,
                index,
            } => Expr::PortSnapshotLane {
                snapshot,
                port,
                index: Box::new(self.hoist_ports_with_hint(*index, None, exact_untyped_ports)),
            },
            Expr::WidthCast {
                kind,
                width,
                src_width,
                inner,
            } => {
                let inner = self.hoist_ports_with_hint(*inner, None, exact_untyped_ports);
                Expr::WidthCast {
                    kind,
                    width,
                    src_width,
                    inner: Box::new(inner),
                }
            }
            Expr::Call(t, args) => {
                let args = args
                    .into_iter()
                    .map(|a| self.hoist_ports_with_hint(a, None, exact_untyped_ports))
                    .collect();
                // A value-bearing transactor-method call in expression
                // position: pull the call edge into its own
                // `Stmt::TransactorCall { dest: Some(temp), .. }` (the
                // seam rule's sanctioned home) and substitute the result
                // temp. Args (and sibling ports, since this is the same
                // left-to-right pass as `Expr::Port` hoisting) are already
                // lifted above, so the `tick()` inside the call lands in
                // source order. Helper/Builtin/Tseq targets are ordinary
                // inline values.
                self.hoist_transactor_edge(Expr::Call(t, args))
            }
            Expr::ComponentIdle {
                base,
                subpath,
                kind,
                n,
            } => {
                let n = self.hoist_ports_with_hint(*n, None, exact_untyped_ports);
                Expr::ComponentIdle {
                    base,
                    subpath,
                    kind,
                    n: Box::new(n),
                }
            }
            Expr::TransactorIdle {
                field,
                transactor,
                storage,
                kind,
                n,
            } => {
                let n = self.hoist_ports_with_hint(*n, None, exact_untyped_ports);
                Expr::TransactorIdle {
                    field,
                    transactor,
                    storage,
                    kind,
                    n: Box::new(n),
                }
            }
            Expr::SeqIndex { seq, index } => {
                let index = self.hoist_ports_with_hint(*index, None, exact_untyped_ports);
                Expr::SeqIndex {
                    seq,
                    index: Box::new(index),
                }
            }
            Expr::ComponentVecElement { base, field, index_pos, index, inner_index } => {
                let index = self.hoist_ports_with_hint(*index, None, exact_untyped_ports);
                let inner_index = inner_index.map(|j| {
                    Box::new(self.hoist_ports_with_hint(*j, None, exact_untyped_ports))
                });
                Expr::ComponentVecElement {
                    base,
                    field,
                    index_pos,
                    index: Box::new(index),
                    inner_index,
                }
            }
            // Fixed-vector testbench-field / test-local element reads carry
            // index sub-exprs that may hold DUT ports (`mem[dut.sel]`) —
            // hoist into both dimensions, mirroring `ComponentVecElement`.
            Expr::TbFieldVecElement { field, index, inner_index } => {
                let index = self.hoist_ports_with_hint(*index, None, exact_untyped_ports);
                let inner_index = inner_index.map(|j| {
                    Box::new(self.hoist_ports_with_hint(*j, None, exact_untyped_ports))
                });
                Expr::TbFieldVecElement {
                    field,
                    index: Box::new(index),
                    inner_index,
                }
            }
            Expr::TransactorStateRecordField {
                instance,
                field,
                path,
                mid_indices,
                index,
            } if index.is_some() || !mid_indices.is_empty() => {
                let mid_indices = mid_indices
                    .into_iter()
                    .map(|(p, idx)| (p, self.hoist_ports_with_hint(idx, None, exact_untyped_ports)))
                    .collect();
                let index = index.map(|idx| Box::new(self.hoist_ports_with_hint(*idx, None, exact_untyped_ports)));
                Expr::TransactorStateRecordField {
                    instance,
                    field,
                    path,
                    mid_indices,
                    index,
                }
            }
            // An indexed `Vec`-field read carries index sub-exprs (the
            // leaf `[i]` and any mid-chain `entries[i].…` selections),
            // which may hold DUT ports; hoist into each. A plain scalar
            // RecordField (no indices) is the no-op host-state value.
            Expr::RecordField {
                local,
                field,
                path,
                mid_indices,
                index,
            } if index.is_some() || !mid_indices.is_empty() => {
                let mid_indices = mid_indices
                    .into_iter()
                    .map(|(p, idx)| (p, self.hoist_ports_with_hint(idx, None, exact_untyped_ports)))
                    .collect();
                let index =
                    index.map(|idx| Box::new(self.hoist_ports_with_hint(*idx, None, exact_untyped_ports)));
                Expr::RecordField {
                    local,
                    field,
                    path,
                    mid_indices,
                    index,
                }
            }
            Expr::CovHookParam { param, field, index: Some(index) } => {
                let index = self.hoist_ports_with_hint(*index, None, exact_untyped_ports);
                Expr::CovHookParam { param, field, index: Some(Box::new(index)) }
            }
            other @ (Expr::Literal { .. }
            | Expr::WideLiteral(_)
            | Expr::Local(_)
            // The global cycle counter / error counter — framework
            // values, no DUT port.
            | Expr::CycleCount
            | Expr::ErrorCount
            // Index-free RecordFields only — the guarded arm above
            // consumed every index-carrying shape.
            | Expr::RecordField { .. }
            | Expr::CovHookParam { index: None, .. }
            | Expr::CovHookArg { .. }
            | Expr::TbField(_)
            // A temporal latch reading is resolved inside the check closure,
            // never a DUT port read at this position.
            | Expr::TemporalSlot { .. }
            // Transactor-instance state is host state — no DUT port inside.
            | Expr::TransactorState { .. }
            // A record-state subfield read is host state — no DUT port inside.
            | Expr::TransactorStateRecordField { .. }
            // Target-state queue size/empty reads are host state — no port.
            | Expr::TransactorStateQueueQuery { .. }
            // Scoreboard reads are host state — no DUT port inside.
            | Expr::ScoreboardQuery { .. }
            // Component fields are host state — no DUT port inside.
            | Expr::ComponentField { .. }
            // A by-value component arg is host state — no DUT port inside.
            | Expr::ComponentValue { .. }
            // Component-queue size/empty reads are host state — no port.
            | Expr::ComponentQueueQuery { .. }
            // Sequence length is host state — no DUT port inside.
            | Expr::SeqLen(_)
            // A register-level frontdoor read carries no DUT *port*
            // subtree — its bus read routes through the helper lambda,
            // emitted inline. Nothing to hoist.
            | Expr::RegRead { .. }
            | Expr::CovBin { .. }) => other,
        }
    }

    pub(crate) fn expr_type(&self, e: &Expr) -> Option<IrType> {
        match e {
            Expr::Literal { ty, .. } => Some(ty.clone()),
            Expr::WideLiteral(words) => Some(IrType::UInt(Some(wide_literal_bits(words)))),
            Expr::Local(l) => Some(self.local_type(*l).clone()),
            Expr::Port(p) => p.width.map(|w| IrType::UInt(Some(w))),
            Expr::Binary(op, a, b) => match op {
                BinOp::Eq
                | BinOp::Ne
                | BinOp::Lt
                | BinOp::Le
                | BinOp::Gt
                | BinOp::Ge
                | BinOp::And
                | BinOp::Or => Some(IrType::Bool),
                BinOp::Shl | BinOp::Shr => self.expr_type(a),
                _ => common_expr_type(self.expr_type(a), self.expr_type(b)),
            },
            Expr::Unary(crate::ir::UnOp::Not, _) => Some(IrType::Bool),
            Expr::Unary(_, inner) => self.expr_type(inner),
            Expr::Ternary(_, t, e) => common_expr_type(self.expr_type(t), self.expr_type(e)),
            Expr::BitSlice { hi, lo, .. } => Some(IrType::UInt(Some(hi - lo + 1))),
            // Runtime bounds: the width is not known here. The helper
            // returns `uint64_t`, so the value is unsigned of unknown
            // width — not `None`, which would let a signed context
            // silently claim it.
            Expr::BitSliceDyn { .. } => Some(IrType::UInt(None)),
            Expr::WidthCast { kind, width, .. } => Some(match kind {
                WidthCastKind::Sext => IrType::SInt(Some(*width)),
                _ => IrType::UInt(Some(*width)),
            }),
            Expr::Call(
                crate::ir::CallTarget::Helper { ret, .. }
                | crate::ir::CallTarget::ExternFn { ret, .. },
                _,
            ) => Some(ret.clone()),
            Expr::Call(
                crate::ir::CallTarget::TransactorMethod { .. }
                | crate::ir::CallTarget::TransactorSelfMethod { .. },
                _,
            ) => self.transactor_call_ret_ty(e),
            Expr::ComponentIdle { .. } | Expr::TransactorIdle { .. } => Some(IrType::Bool),
            Expr::DynamicListQuery {
                query: crate::ir::DynamicListQuery::Size,
                ..
            } => Some(IrType::UInt(None)),
            Expr::DynamicListQuery {
                query: crate::ir::DynamicListQuery::Empty,
                ..
            } => Some(IrType::Bool),
            Expr::TbField(field) => self.ctx.tb_scalar_fields.get(field).cloned(),
            Expr::ComponentField { base, field } => self.component_field_value_type(base, field),
            Expr::TransactorState { instance, field } => {
                let kind = if instance.is_empty() {
                    self.target_state_fields.get(field)
                } else {
                    self.ctx.target_state.get(instance)?.get(field)
                };
                match kind? {
                    crate::ir::StateFieldKind::Scalar { ty, .. } => Some(ty.clone()),
                    crate::ir::StateFieldKind::Record { record } => Some(IrType::Record(*record)),
                    crate::ir::StateFieldKind::FixedVec { ty } => Some(ty.clone()),
                    crate::ir::StateFieldKind::Queue { .. } => None,
                }
            }
            Expr::TransactorStateRecordField {
                instance,
                field,
                path,
                mid_indices,
                index,
            } => {
                let kind = if instance.is_empty() {
                    self.target_state_fields.get(field)
                } else {
                    self.ctx.target_state.get(instance)?.get(field)
                };
                match kind? {
                    crate::ir::StateFieldKind::Record { record } => {
                        self.record_path_value_type(*record, path, mid_indices, index.is_some())
                    }
                    crate::ir::StateFieldKind::FixedVec { ty }
                        if path.is_empty() && index.is_some() => {
                        let mut selected = ty;
                        for _ in 0..=mid_indices.len() {
                            let IrType::FixedVec { elem, .. } = selected else {
                                return None;
                            };
                            selected = elem;
                        }
                        Some(selected.clone())
                    }
                    _ => None,
                }
            }
            Expr::ScoreboardQuery { sb, query, .. } => match query {
                crate::ir::ScoreboardQuery::Scalar { scalar } => self
                    .ctx
                    .scoreboards
                    .get(sb.index())?
                    .field(scalar)
                    .and_then(|field| match &field.kind {
                        crate::ir::ScoreboardFieldKind::Scalar { ty, .. } => Some(ty.clone()),
                        crate::ir::ScoreboardFieldKind::Record { record } => {
                            Some(IrType::Record(*record))
                        }
                        _ => None,
                    }),
                crate::ir::ScoreboardQuery::QueueSize { .. } => Some(IrType::UInt(None)),
                crate::ir::ScoreboardQuery::QueueEmpty { .. } => Some(IrType::Bool),
            },
            // A record-field chain types as its leaf: the leaf field's own
            // scalar/record type, or the element type when the leaf `Vec`
            // is indexed. A whole (unindexed) `Vec` leaf is an array — it
            // has no expression-value type here. This is what types an
            // untyped `let e = tbl.entries[i]` as the element record.
            Expr::RecordField {
                local,
                field,
                path,
                mid_indices,
                index,
            } => {
                let mut cur = self.record_of_local(*local)?;
                let segs: Vec<&String> = std::iter::once(field).chain(path.iter()).collect();
                let last = segs.len() - 1;
                for (i, seg) in segs.iter().enumerate() {
                    let fld = self.ctx.records.get(cur.index())?.field(seg)?;
                    if i == last {
                        let selections = mid_indices.iter().filter(|(p, _)| *p == i).count()
                            + usize::from(index.is_some());
                        return selected_record_leaf_type(fld.vec_len, &fld.ty, selections);
                    }
                    let indexed = mid_indices.iter().any(|(p, _)| *p == i);
                    match fld.ty {
                        IrType::Record(r) if fld.vec_len.is_none() == !indexed => cur = r,
                        _ => return None,
                    }
                }
                None
            }
            Expr::ComponentVecElement {
                base,
                field,
                inner_index,
                ..
            } => {
                // `field` is a member SUFFIX and is not always one name:
                // a `Vec<T, N>` LEAF inside a component RECORD field
                // spells it dotted (`a.data`). Looking it up by whole
                // name can never match that, and "no type" here is what
                // let a record-element selection through the guards
                // below.
                let cid = self.component_base_id(base)?;
                let mut segs = field.split('.');
                let root = segs.next()?;
                match &self.ctx.components.get(cid.index())?.field(root)?.kind {
                    crate::ir::ComponentFieldKind::FixedVec(vec) if field.find('.').is_none() => {
                        // A nested read `v[i][j]` descends one `FixedVec`
                        // to the scalar leaf; a single `v[i]` yields the
                        // element as declared (a scalar, or the inner
                        // vector for a nested field).
                        match (inner_index, &vec.elem) {
                            (Some(_), IrType::FixedVec { elem, .. }) => Some((**elem).clone()),
                            _ => Some(vec.elem.clone()),
                        }
                    }
                    crate::ir::ComponentFieldKind::Record { record } => {
                        let mut cur = *record;
                        let mut last = None;
                        for seg in segs {
                            let fld = self.ctx.records.get(cur.index())?.field(seg)?;
                            last = Some(fld.ty.clone());
                            if let IrType::Record(r) = fld.ty {
                                cur = r;
                            }
                        }
                        last
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Exact scalar/record value type of a component field expression.
    /// Whole fixed vectors deliberately return `None`: they are arrays, not
    /// scalar format/temporary values. Dotted record paths retain the leaf's
    /// declared width and record identity.
    pub(crate) fn component_field_value_type(
        &self,
        base: &crate::ir::ComponentBase,
        field: &str,
    ) -> Option<IrType> {
        let cid = self.component_base_id(base)?;
        let mut segs = field.split('.');
        let root = self.ctx.components.get(cid.index())?.field(segs.next()?)?;
        let mut record = match &root.kind {
            crate::ir::ComponentFieldKind::Scalar { ty, .. } => {
                return segs.next().is_none().then(|| ty.clone())
            }
            crate::ir::ComponentFieldKind::Record { record } => *record,
            _ => return None,
        };
        let rest: Vec<&str> = segs.collect();
        if rest.is_empty() {
            return Some(IrType::Record(record));
        }
        for (i, seg) in rest.iter().enumerate() {
            let member = self.ctx.records.get(record.index())?.field(seg)?;
            if i + 1 == rest.len() {
                return member.vec_len.is_none().then(|| member.ty.clone());
            }
            match member.ty {
                IrType::Record(next) if member.vec_len.is_none() => record = next,
                _ => return None,
            }
        }
        None
    }

    fn record_path_value_type(
        &self,
        mut record: RecordId,
        path: &[String],
        mid_indices: &[(usize, Expr)],
        leaf_indexed: bool,
    ) -> Option<IrType> {
        for (i, seg) in path.iter().enumerate() {
            let member = self.ctx.records.get(record.index())?.field(seg)?;
            let selection_count = mid_indices.iter().filter(|(pos, _)| *pos == i).count()
                + usize::from(i + 1 == path.len() && leaf_indexed);
            let indexed = selection_count != 0;
            if i + 1 == path.len() {
                return selected_record_leaf_type(
                    member.vec_len,
                    &member.ty,
                    selection_count,
                );
            }
            match member.ty {
                IrType::Record(next)
                    if (member.vec_len.is_none() && !indexed)
                        || (member.vec_len.is_some() && selection_count == 1) =>
                {
                    record = next
                }
                _ => return None,
            }
        }
        Some(IrType::Record(record))
    }

    /// Conservative scalar result type for assignment compatibility. Unlike
    /// ordinary expression typing, an `&` literal mask provides a real upper
    /// bound, and shifts retain that bounded LHS width. This admits
    /// `(wide & 0xFF) >> 4` into uint<8> without exempting unrelated binary
    /// expressions such as `wide + 1` from narrowing checks.
    pub(crate) fn scalar_assignment_type(&self, e: &Expr) -> Option<IrType> {
        match e {
            Expr::Binary(BinOp::BitAnd, lhs, rhs) => {
                let bounded = |e: &Expr| {
                    if let Expr::Literal { value, ty } = e {
                        if matches!(ty, IrType::Unknown) {
                            return Some(IrType::UInt(Some((64 - value.leading_zeros()).max(1))));
                        }
                    }
                    self.scalar_assignment_type(e)
                };
                narrowest_scalar_type(bounded(lhs), bounded(rhs))
            }
            Expr::Binary(BinOp::Shl | BinOp::Shr, lhs, _) => self.scalar_assignment_type(lhs),
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
            ) => Some(IrType::Bool),
            Expr::Binary(_, lhs, rhs) => common_expr_type(
                self.scalar_assignment_type(lhs),
                self.scalar_assignment_type(rhs),
            ),
            Expr::Ternary(_, then_expr, else_expr) => common_expr_type(
                self.scalar_assignment_type(then_expr),
                self.scalar_assignment_type(else_expr),
            ),
            Expr::Unary(crate::ir::UnOp::Not, _) => Some(IrType::Bool),
            Expr::Unary(crate::ir::UnOp::BitNotHost, _) => Some(IrType::SInt(None)),
            // Unary minus normally retains its operand's scalar type: this is
            // load-bearing for existing unsigned wide values. A sized-literal
            // expression is widthless here, and the AST-provenance fallback in
            // let lowering turns only that newly admitted form into SInt(None).
            Expr::Unary(crate::ir::UnOp::Neg, inner) => self.scalar_assignment_type(inner),
            Expr::Unary(_, inner) => self.scalar_assignment_type(inner),
            // Not `expr_type`: it answers `None` for every host-state
            // read — a testbench/scoreboard/component field, a
            // transactor state field — and those are exactly the leaves
            // a declared WIDE field made reachable. See
            // `host_state_scalar_type`.
            _ => self.host_state_scalar_type(e),
        }
    }

    /// Does the composed width of `e` rest on a genuinely-WIDTHLESS
    /// scalar leaf — a leaf whose declared width is unknown, so its
    /// contribution to the result width is the 64-bit host-ABI
    /// PLACEHOLDER `common_expr_type` manufactures, not a width anyone
    /// declared?
    ///
    /// The directional narrowing check must not fire on a manufactured
    /// width. `seen : uint<32>` written `seen + N` for a file-scope
    /// `const N = 9` is not a narrowing: `N` substitutes as a widthless
    /// `UInt(None)` (it carries signedness but no width, by #525's
    /// design — a const's width is not its value's minimum), so
    /// `scalar_assignment_type` reports the sum as 64-bit, and flagging
    /// that against a 32-bit field is a false positive with no honest
    /// source fix. A widthless DUT-port read or dynamic bit-slice is
    /// the same. The check is skipped for width when this is true; a
    /// leaf with a DECLARED width (`v[0][1] : uint<128>`) is not
    /// widthless, so `n = n + v[0][1]` is still judged and refused.
    ///
    /// An ordinary integer literal (`Literal { ty: Unknown }`) is NOT
    /// widthless here: `common_expr_type` sizes it to the other operand
    /// rather than to 64, so it never manufactures a width. Only the
    /// `UInt(None)`/`SInt(None)`-typed leaves do.
    ///
    /// Mirrors `scalar_assignment_type`'s arithmetic decomposition so
    /// the two agree about which leaves a width is built from.
    pub(super) fn rhs_width_manufactured(&self, e: &Expr) -> bool {
        match e {
            // A comparison/logical result is `bool` (width 1, known);
            // its operands do not contribute to the result width.
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
            ) => false,
            Expr::Unary(crate::ir::UnOp::Not, _) => false,
            // A shift's width is the shifted value's; the count does not
            // contribute. `&` narrows to the narrower operand, and a
            // sized-literal mask bounds it — but a widthless leaf on
            // either side still manufactures, so recurse both.
            Expr::Binary(BinOp::Shl | BinOp::Shr, lhs, _) => self.rhs_width_manufactured(lhs),
            Expr::Binary(_, lhs, rhs) => {
                self.rhs_width_manufactured(lhs) || self.rhs_width_manufactured(rhs)
            }
            Expr::Ternary(_, t, f) => {
                self.rhs_width_manufactured(t) || self.rhs_width_manufactured(f)
            }
            Expr::Unary(_, inner) => self.rhs_width_manufactured(inner),
            // Leaf: widthless iff its resolved scalar type has no width.
            // A `Literal { Unknown }` resolves to `Unknown`, not
            // `UInt(None)`, so it is not caught here.
            _ => matches!(
                self.scalar_assignment_type(e),
                Some(IrType::UInt(None) | IrType::SInt(None))
            ),
        }
    }

    /// If `e` is a value-bearing `CallTarget::TransactorMethod` edge,
    /// pull it into a fresh `Stmt::TransactorCall { dest: Some(temp), .. }`
    /// and return `Expr::Local(temp)`; otherwise return `e` unchanged.
    /// This is the seam rule's sanctioned home for a transactor edge that
    /// surfaced in expression position (e.g. `(helper.read(0) & 1) == 1`):
    /// the edge never lives nested in another expression, and the call's
    /// internal `tick()` runs at the hoist point (source order, because the
    /// callers traverse left-to-right).
    fn hoist_transactor_edge(&mut self, e: Expr) -> Expr {
        match &e {
            Expr::Call(crate::ir::CallTarget::TransactorMethod { .. }, _) => {
                let temp = self.fresh_temp();
                if let Some(ty) = self.transactor_call_ret_ty(&e) {
                    self.set_local_type(temp, ty);
                }
                self.push(Stmt::TransactorCall {
                    dest: Some(temp),
                    call: e,
                });
                Expr::Local(temp)
            }
            Expr::Call(crate::ir::CallTarget::TransactorSelfMethod { .. }, _) => {
                let temp = self.fresh_temp();
                if let Some(ty) = self.transactor_call_ret_ty(&e) {
                    self.set_local_type(temp, ty);
                }
                self.push(Stmt::TransactorSelfCall {
                    dest: Some(temp),
                    call: e,
                });
                Expr::Local(temp)
            }
            _ => e,
        }
    }

    /// Hoist every value-bearing transactor-method call edge out of `e`
    /// into preceding `Stmt::TransactorCall` statements, leaving DUT
    /// `Expr::Port` leaves INLINE (unlike `hoist_ports`). Used where ports
    /// are intentionally left lazy — assert conditions — but a transactor
    /// edge still cannot stay nested (the seam rule, and the call may
    /// advance simulated time). Traverses left-to-right so the synthesized
    /// `TransactorCall`s land in source order.
    ///
    /// Subset note: an expression that mixes a hoisted port read with a
    /// transactor call is not exercised by any fixture; here ports stay
    /// inline (lazy assert eval) so there is no port/tick reordering.
    pub(crate) fn hoist_transactor_calls(&mut self, e: Expr) -> Expr {
        match e {
            Expr::DynamicListQuery { target, query } => Expr::DynamicListQuery {
                target: Box::new(self.hoist_transactor_calls(*target)),
                query,
            },
            Expr::Binary(op, a, b) => {
                let a = self.hoist_transactor_calls(*a);
                let b = self.hoist_transactor_calls(*b);
                Expr::Binary(op, Box::new(a), Box::new(b))
            }
            Expr::Unary(op, a) => {
                let a = self.hoist_transactor_calls(*a);
                Expr::Unary(op, Box::new(a))
            }
            Expr::Ternary(c, t, f) => {
                let c = self.hoist_transactor_calls(*c);
                let t = self.hoist_transactor_calls(*t);
                let f = self.hoist_transactor_calls(*f);
                Expr::Ternary(Box::new(c), Box::new(t), Box::new(f))
            }
            Expr::BitSlice { target, hi, lo } => {
                let target = self.hoist_transactor_calls(*target);
                Expr::BitSlice {
                    target: Box::new(target),
                    hi,
                    lo,
                }
            }
            Expr::BitSliceDyn { target, hi, lo } => {
                let target = self.hoist_transactor_calls(*target);
                let hi = self.hoist_transactor_calls(*hi);
                let lo = self.hoist_transactor_calls(*lo);
                Expr::BitSliceDyn {
                    target: Box::new(target),
                    hi: Box::new(hi),
                    lo: Box::new(lo),
                }
            }
            Expr::WidthCast {
                kind,
                width,
                src_width,
                inner,
            } => {
                let inner = self.hoist_transactor_calls(*inner);
                Expr::WidthCast {
                    kind,
                    width,
                    src_width,
                    inner: Box::new(inner),
                }
            }
            Expr::ComponentIdle {
                base,
                subpath,
                kind,
                n,
            } => {
                let n = self.hoist_transactor_calls(*n);
                Expr::ComponentIdle {
                    base,
                    subpath,
                    kind,
                    n: Box::new(n),
                }
            }
            Expr::TransactorIdle {
                field,
                transactor,
                storage,
                kind,
                n,
            } => {
                let n = self.hoist_transactor_calls(*n);
                Expr::TransactorIdle {
                    field,
                    transactor,
                    storage,
                    kind,
                    n: Box::new(n),
                }
            }
            Expr::SeqIndex { seq, index } => {
                let index = self.hoist_transactor_calls(*index);
                Expr::SeqIndex {
                    seq,
                    index: Box::new(index),
                }
            }
            Expr::ComponentVecElement {
                base,
                field,
                index_pos,
                index,
                inner_index,
            } => {
                let index = self.hoist_transactor_calls(*index);
                let inner_index = inner_index.map(|j| Box::new(self.hoist_transactor_calls(*j)));
                Expr::ComponentVecElement {
                    base,
                    field,
                    index_pos,
                    index: Box::new(index),
                    inner_index,
                }
            }
            // A value-returning transactor-method call in a fixed-vector
            // testbench-field READ index (`mem[xt.idx()]`) must hoist into a
            // `Stmt::TransactorCall` like any other call-in-expression — the
            // write path already does (its index goes through this same
            // helper), so without this arm the read is asymmetric and the
            // verifier rejects the un-hoisted call edge. Mirrors
            // `ComponentVecElement`.
            Expr::TbFieldVecElement {
                field,
                index,
                inner_index,
            } => {
                let index = self.hoist_transactor_calls(*index);
                let inner_index = inner_index.map(|j| Box::new(self.hoist_transactor_calls(*j)));
                Expr::TbFieldVecElement {
                    field,
                    index: Box::new(index),
                    inner_index,
                }
            }
            Expr::TransactorStateRecordField {
                instance,
                field,
                path,
                mid_indices,
                index,
            } => {
                let mid_indices = mid_indices
                    .into_iter()
                    .map(|(p, idx)| (p, self.hoist_transactor_calls(idx)))
                    .collect();
                let index = index.map(|idx| Box::new(self.hoist_transactor_calls(*idx)));
                Expr::TransactorStateRecordField {
                    instance,
                    field,
                    path,
                    mid_indices,
                    index,
                }
            }
            Expr::Call(t, args) => {
                let args = args
                    .into_iter()
                    .map(|a| self.hoist_transactor_calls(a))
                    .collect();
                self.hoist_transactor_edge(Expr::Call(t, args))
            }
            other => other,
        }
    }

    /// Lower `sb.<queue>.size()` / `sb.<queue>.empty()` into an
    /// `Expr::ScoreboardQuery`, or `None` when `callee` is not a
    /// scoreboard queue method access. A `pop()` reaching here (deeper
    /// than a `let`/assign RHS) is rejected — it mutates and must be a
    /// statement.
    fn lower_scoreboard_query_call(
        &self,
        callee: &AstExpr,
        args: &[crate::ast::CallArg],
    ) -> Result<Option<Expr>, LowerError> {
        let Some((sb, field, queue, method, nested_path)) = self.as_scoreboard_queue_call(callee)
        else {
            return Ok(None);
        };
        let query = match method.as_str() {
            "size" => crate::ir::ScoreboardQuery::QueueSize {
                queue: queue.clone(),
            },
            "empty" => crate::ir::ScoreboardQuery::QueueEmpty {
                queue: queue.clone(),
            },
            "pop" => {
                // Validate the receiver before issuing the queue-specific
                // expression-position advice. A scoreboard list has no
                // `.pop()` member in v1's std::vector storage, so telling a
                // list user to bind the result and retry v1 is a dead end.
                self.scoreboard_queue_field(sb, &queue)?;
                return Err(queue_pop_in_expression_position(&format!(
                    "scoreboard `{field}.{queue}.pop()`"
                )));
            }
            other => {
                return Err(queue_method_in_expression_position(
                    &format!("scoreboard queue method `{field}.{queue}.{other}(...)`"),
                    other,
                ));
            }
        };
        if !args.is_empty() {
            return Err(LowerError::Invalid(format!(
                "scoreboard `{field}.{queue}.{method}()` takes no arguments"
            )));
        }
        self.scoreboard_container_field(sb, &queue)?;
        Ok(Some(Expr::ScoreboardQuery {
            sb,
            field,
            query,
            nested_path,
        }))
    }

    /// Lower `_tb.pending.size()` / `.empty()` on a testbench-owned queue.
    /// The queue's field identity is explicit in the IR so backend and
    /// verifier ownership cannot be confused with scoreboard state.
    fn lower_tb_queue_query_call(
        &self,
        callee: &AstExpr,
        args: &[crate::ast::CallArg],
    ) -> Result<Option<Expr>, LowerError> {
        let Some((field, method)) = self.as_tb_queue_call(callee) else {
            return Ok(None);
        };
        if !args.is_empty() {
            return Err(LowerError::Invalid(format!(
                "testbench queue `{field}.{method}()` takes no arguments"
            )));
        }
        let query = match method.as_str() {
            "size" => crate::ir::ScoreboardQuery::QueueSize {
                queue: field.clone(),
            },
            "empty" => crate::ir::ScoreboardQuery::QueueEmpty {
                queue: field.clone(),
            },
            "pop" => {
                return Err(queue_pop_in_expression_position(&format!(
                    "testbench queue `{field}.pop()`"
                )));
            }
            other => {
                return Err(queue_method_in_expression_position(
                    &format!("testbench queue method `{field}.{other}(...)`"),
                    other,
                ));
            }
        };
        Ok(Some(Expr::TbQueueQuery { field, query }))
    }

    /// Lower `<recv>.<queue>.size()` / `.empty()` on a composite-component
    /// `queue<T>` field into an `Expr::ComponentQueueQuery`, or `None` when
    /// `callee` is not a component-queue method access. A `pop()` reaching
    /// here (deeper than a `let`/assign RHS) is rejected — it mutates and
    /// must be a statement. Mirrors `lower_scoreboard_query_call`.
    fn lower_component_queue_query(
        &self,
        callee: &AstExpr,
        args: &[crate::ast::CallArg],
    ) -> Result<Option<Expr>, LowerError> {
        let Some((base, queue, method)) = self.as_component_queue_call(callee)? else {
            return Ok(None);
        };
        let query = match method.as_str() {
            "size" => crate::ir::ScoreboardQuery::QueueSize {
                queue: queue.clone(),
            },
            "empty" => crate::ir::ScoreboardQuery::QueueEmpty {
                queue: queue.clone(),
            },
            "pop" => {
                return Err(queue_pop_in_expression_position(&format!(
                    "component `{queue}.pop()`"
                )));
            }
            other => {
                return Err(queue_method_in_expression_position(
                    &format!("component queue method `{queue}.{other}(...)`"),
                    other,
                ));
            }
        };
        if !args.is_empty() {
            return Err(LowerError::Invalid(format!(
                "component `{queue}.{method}()` takes no arguments"
            )));
        }
        Ok(Some(Expr::ComponentQueueQuery { base, query }))
    }

    /// Lower `<field>.size()` / `<field>.empty()` on a bound-to target
    /// transactor's persistent `queue<T>` state field (a bare field name
    /// inside a responder body) into an `Expr::TransactorStateQueueQuery`,
    /// or `None` when `callee` is not a state-queue method access. The
    /// `instance` is a placeholder filled at test-binding. A `pop()`
    /// reaching here (deeper than a `let`/assign RHS) is rejected — it
    /// mutates and must be a statement. Mirrors `lower_component_queue_query`.
    fn lower_state_queue_query(
        &self,
        callee: &AstExpr,
        args: &[crate::ast::CallArg],
    ) -> Result<Option<Expr>, LowerError> {
        let ExprKind::Field { target, name } = &*callee.kind else {
            return Ok(None);
        };
        let ExprKind::Ident(id) = &*target.kind else {
            return Ok(None);
        };
        // A local of the same name shadows the state field (matches the
        // bare-read resolution order).
        if self.lookup(&id.name).is_some() {
            return Ok(None);
        }
        if !matches!(
            self.target_state_fields.get(&id.name),
            Some(crate::ir::StateFieldKind::Queue { .. })
        ) {
            return Ok(None);
        }
        let field = id.name.clone();
        let method = name.name.clone();
        let query = match method.as_str() {
            "size" => crate::ir::ScoreboardQuery::QueueSize {
                queue: field.clone(),
            },
            "empty" => crate::ir::ScoreboardQuery::QueueEmpty {
                queue: field.clone(),
            },
            "pop" => {
                return Err(queue_pop_in_expression_position(&format!(
                    "target-state `{field}.pop()`"
                )));
            }
            other => {
                return Err(queue_method_in_expression_position(
                    &format!("target-state queue method `{field}.{other}(...)`"),
                    other,
                ));
            }
        };
        if !args.is_empty() {
            return Err(LowerError::Invalid(format!(
                "target-state `{field}.{method}()` takes no arguments"
            )));
        }
        Ok(Some(Expr::TransactorStateQueueQuery {
            instance: String::new(),
            field,
            query,
        }))
    }

    /// `Some(PortRef)` when the expression is a dotted access rooted at
    /// the DUT field (`dut.count_out`, `dut.bus.req`). `Err` when it is
    /// rooted at the testbench instance (`_tb.<field>` — post-MVP).
    pub(crate) fn as_port_ref(&self, e: &AstExpr) -> Result<Option<PortRef>, LowerError> {
        let mut segments: Vec<String> = Vec::new();
        let mut cur = e;
        loop {
            match &*cur.kind {
                ExprKind::Field { target, name } => {
                    segments.push(name.name.clone());
                    cur = target;
                }
                ExprKind::Ident(root) => {
                    // The DUT field itself, or — inside an inlined
                    // helper — a parameter bound to the DUT. Either way
                    // the `PortRef` is rooted at the caller's DUT field.
                    // A declared local SHADOWS the DUT name (a method
                    // param or `let` named like the DUT field is host
                    // state, not the DUT — v1 surfaces such shadowing
                    // as a C++ compile error; without this guard the
                    // access would silently mis-lower to a DutWrite/
                    // DutRead). DUT-bound inline-helper params are not
                    // declared as locals, so they pass through.
                    if self.lookup(&root.name).is_none() && self.is_dut_name(&root.name) {
                        if segments.is_empty() {
                            return Ok(None);
                        }
                        segments.reverse();
                        // A single-segment `dut.<name>` whose name was
                        // declared as a `probe` on `let dut` is a DUT-
                        // internal access, not a top-level port: it lowers
                        // to a `Probe` (read-only) or `Force` (force-
                        // capable) `PortRef` so the tbir backend routes it
                        // through the SV bind-stub accessor. Ordinary ports
                        // keep `Port`. See docs/probe-signals.md.
                        let (access, width) = match self.ctx.probes.get(&segments[0]) {
                            Some(meta) => {
                                let access = if meta.force {
                                    PortAccess::Force
                                } else {
                                    PortAccess::Probe
                                };
                                (access, meta.width)
                            }
                            None => (PortAccess::Port, None),
                        };
                        return Ok(Some(PortRef {
                            testbench_field: self.ctx.dut_field.clone(),
                            port_path: segments,
                            aggregate_path: true,
                            deferred_bus_binding: None,
                            direction: None,
                            width,
                            access,
                            lane: None,
                        }));
                    }
                    if Some(root.name.as_str()) == self.ctx.tb_field.as_deref()
                        && !segments.is_empty()
                    {
                        // Covergroup-field paths (`_tb.cov...`) and
                        // scalar-field paths (`_tb.expected`) are not
                        // ports — `lower_expr` resolves them as
                        // `Expr::CovBin` via `as_cov_bin`; `Expr::TbField`
                        // via the testbench-field path. Transactor-
                        // field paths (`_tb.xact...`) are call/bind
                        // surfaces handled by their statement forms.
                        if self.ctx.cov_fields.contains_key(segments.last().unwrap())
                            || self
                                .ctx
                                .transactor_fields
                                .contains_key(segments.last().unwrap())
                        {
                            return Ok(None);
                        }
                        // Scoreboard-field paths (`_tb.sb`, `_tb.sb.q`,
                        // `_tb.sb.q.push`) are host state, not ports —
                        // `lower_expr` / `lower_assign` resolve them via
                        // the scoreboard op/query forms. The root field
                        // (the segment after `_tb`) is the scoreboard
                        // instance name.
                        if self
                            .ctx
                            .scoreboard_fields
                            .contains_key(segments.last().unwrap())
                        {
                            return Ok(None);
                        }
                        // Composite-component field paths (`_tb.prod`,
                        // `_tb.prod.seen`, `_tb.top.prod`) are host
                        // instances, not ports — `lower_expr` /
                        // `lower_assign` resolve them via the component
                        // field/method/idle/emit forms. `segments` is in
                        // reverse path order (innermost first), so the
                        // segment right after `_tb` — the component
                        // instance name — is `segments.last()`.
                        if self
                            .ctx
                            .component_fields
                            .contains_key(segments.last().unwrap())
                        {
                            return Ok(None);
                        }
                        if segments.len() == 1
                            && self.ctx.tb_scalar_fields.contains_key(&segments[0])
                        {
                            return Ok(None);
                        }
                        if self
                            .ctx
                            .tb_record_fields
                            .iter()
                            .any(|(field, _)| field == segments.last().unwrap())
                        {
                            return Ok(None);
                        }
                        if segments.len() == 1
                            && self
                                .ctx
                                .tb_record_fields
                                .iter()
                                .any(|(field, _)| field == &segments[0])
                        {
                            return Ok(None);
                        }
                        return Err(unsupported(
                            &format!("testbench field access `_tb.{}`", segments.last().unwrap()),
                            "",
                        ));
                    }
                    return Ok(None);
                }
                ExprKind::Paren(inner) => cur = inner,
                _ => return Ok(None),
            }
        }
    }

    /// Lower a built-in heartbeat predicate on a direct transactor field.
    /// A user-declared method with the same name wins, matching v1 and the
    /// component predicate path. Both testbench fields (`_tb.d`) and bare
    /// test-scope instances (`d`) resolve through `transactor_fields`.
    pub(crate) fn as_transactor_idle(
        &mut self,
        callee: &AstExpr,
        args: &[CallArg],
    ) -> Result<Option<Expr>, LowerError> {
        let ExprKind::Field {
            target,
            name: method,
        } = &*callee.kind
        else {
            return Ok(None);
        };
        let kind = match method.name.as_str() {
            "idle" | "quiesced" => crate::ir::IdleKind::Both,
            "idle_in" => crate::ir::IdleKind::In,
            "idle_out" => crate::ir::IdleKind::Out,
            _ => return Ok(None),
        };
        let field = match &*target.kind {
            ExprKind::Field {
                target: root_expr,
                name: field,
            } => {
                let ExprKind::Ident(root) = &*root_expr.kind else {
                    return Ok(None);
                };
                if Some(root.name.as_str()) != self.ctx.tb_field.as_deref() {
                    return Ok(None);
                }
                field.name.clone()
            }
            ExprKind::Ident(id)
                if self.lookup(&id.name).is_none()
                    && (self.ctx.bare_transactor_fields.contains(&id.name)
                        || self.ctx.transactor_fields.contains_key(&id.name)
                        || self.ctx.target_transactor_fields.contains_key(&id.name)) =>
            {
                id.name.clone()
            }
            _ => return Ok(None),
        };
        let Some(transactor) = self
            .ctx
            .transactor_fields
            .get(&field)
            .or_else(|| self.ctx.target_transactor_fields.get(&field))
            .copied()
        else {
            return Ok(None);
        };
        let schema = &self.ctx.transactors[transactor.index()];
        if schema.method(&method.name).is_some() {
            return Ok(None);
        }
        if args.len() != 1 {
            let (detail, v1) = if method.name == "quiesced" {
                (
                    "the quiesce predicate takes exactly one cycle-count argument",
                    V1Status::EmitsUncompilable,
                )
            } else {
                (
                    "idle predicates take exactly one cycle-count argument",
                    V1Status::Rejects,
                )
            };
            return Err(not_implemented(
                &format!("`{}(...)` with {} arguments", method.name, args.len()),
                detail.to_string(),
                v1,
            ));
        }
        // The sole argument binds by position. v1 ignores an optional
        // source name here, as it does for component predicates.
        let (CallArg::Expr(n_expr) | CallArg::Named { value: n_expr, .. }) = &args[0];
        let n = self.lower_expr_no_ports(n_expr)?;
        let storage = if self.ctx.transactor_fields.contains_key(&field) {
            self.ctx
                .heartbeat_transactor_fields
                .borrow_mut()
                .insert(field.clone());
            self.ctx.heartbeat_transactor_storage[&field].clone()
        } else {
            // Bound target responders already have a per-instance state
            // object whose body includes the heartbeat stamps.
            field.clone()
        };
        Ok(Some(Expr::TransactorIdle {
            field,
            transactor,
            storage,
            kind,
            n: Box::new(n),
        }))
    }

    /// `Some((tb_field, transactor, method))` when `callee` is a
    /// method access on a transactor-typed testbench field:
    /// `_tb.xact.write1` (the impl-for desugaring already rewrote
    /// `xact.` → `_tb.xact.`). An access to a method the transactor
    /// does not declare is a hard error — v1 would surface it as a
    /// C++ compile failure; the IR rejects it at lowering.
    pub(crate) fn as_transactor_call(
        &self,
        callee: &AstExpr,
    ) -> Result<Option<(String, crate::ir::TransactorId, String)>, LowerError> {
        let ExprKind::Field {
            target,
            name: method,
        } = &*callee.kind
        else {
            return Ok(None);
        };
        // Two access shapes resolve to a transactor field:
        //   `_tb.<field>.<method>` — testbench-field instance (the
        //     impl-for desugaring rewrote `xact.` → `_tb.xact.`).
        //   `<field>.<method>`     — test-scope-let instance, accessed
        //     by its bare name (left unqualified by the desugaring).
        let field_name = match &*target.kind {
            ExprKind::Field {
                target: root_expr,
                name: field,
            } => {
                let ExprKind::Ident(root) = &*root_expr.kind else {
                    return Ok(None);
                };
                if Some(root.name.as_str()) != self.ctx.tb_field.as_deref() {
                    return Ok(None);
                }
                field.name.clone()
            }
            ExprKind::Ident(id)
                if self.lookup(&id.name).is_none()
                    && (self.ctx.bare_transactor_fields.contains(&id.name)
                        || self.ctx.transactor_fields.contains_key(&id.name)) =>
            {
                id.name.clone()
            }
            _ => return Ok(None),
        };
        let Some(&xid) = self.ctx.transactor_fields.get(&field_name) else {
            return Ok(None);
        };
        let schema = &self.ctx.transactors[xid.index()];
        if schema.method(&method.name).is_none() {
            // Built-in heartbeat predicates are pure expressions, not
            // transactor method edges. Returning `None` lets statement,
            // let, and assignment callers fall through to ordinary
            // expression lowering (`as_transactor_idle`) instead of
            // claiming the call here and rejecting a missing method.
            if matches!(
                method.name.as_str(),
                "idle" | "idle_in" | "idle_out" | "quiesced"
            ) {
                return Ok(None);
            }
            return Err(LowerError::Invalid(format!(
                "transactor `{}` has no method `{}`",
                schema.name, method.name
            )));
        }
        Ok(Some((field_name, xid, method.name.clone())))
    }

    /// `Some(Expr::CovBin)` when the expression is a check-phase bin
    /// read on a covergroup-typed testbench field: `_tb.cov.cp_x.yes`
    /// (the impl-for desugaring already rewrote `cov.` → `_tb.cov.`).
    /// Unknown point/bin names are hard errors — v1 would surface them
    /// as C++ compile failures; the IR rejects them at lowering.
    pub(crate) fn as_cov_bin(&self, e: &AstExpr) -> Result<Option<Expr>, LowerError> {
        let Some((field, rest)) = self.as_cov_field_path(e) else {
            return Ok(None);
        };
        let covgroup = self.ctx.cov_fields[&field];
        let schema = &self.ctx.covgroups[covgroup.index()];
        let [point, bin] = rest.as_slice() else {
            return Err(unsupported(
                &format!(
                    "covergroup field access `{field}.{}` (expected `{field}.<point>.<bin>`)",
                    rest.join(".")
                ),
                "",
            ));
        };
        let Some(p) = schema.points.iter().find(|p| p.name == *point) else {
            return Err(LowerError::Invalid(format!(
                "covergroup `{}` has no coverpoint `{point}`",
                schema.name
            )));
        };
        if !p.bins.iter().any(|b| b.name == *bin) {
            return Err(LowerError::Invalid(format!(
                "coverpoint `{}.{point}` has no bin `{bin}`",
                schema.name
            )));
        }
        Ok(Some(Expr::CovBin {
            inst: crate::ir::CovgroupInstance {
                tb_field: field,
                covgroup,
            },
            point: point.clone(),
            bin: bin.clone(),
        }))
    }

    /// Decompose a dotted path rooted at a covergroup-typed testbench
    /// field: `_tb.cov.a.b` → `Some(("cov", ["a", "b"]))`.
    pub(crate) fn as_cov_field_path(&self, e: &AstExpr) -> Option<(String, Vec<String>)> {
        let tb_field = self.ctx.tb_field.as_deref()?;
        let mut segments: Vec<String> = Vec::new();
        let mut cur = e;
        loop {
            match &*cur.kind {
                ExprKind::Field { target, name } => {
                    segments.push(name.name.clone());
                    cur = target;
                }
                ExprKind::Paren(inner) => cur = inner,
                ExprKind::Ident(root) => {
                    if root.name != tb_field {
                        return None;
                    }
                    segments.reverse();
                    let field = segments.first()?.clone();
                    if !self.ctx.cov_fields.contains_key(&field) {
                        return None;
                    }
                    return Some((field, segments[1..].to_vec()));
                }
                _ => return None,
            }
        }
    }
    /// `Some(field)` when the expression is a one-segment access to a
    /// scalar testbench field: `_tb.expected`.
    ///
    /// A fixed-vector host field (`_tb.mem`) is deliberately EXCLUDED: it
    /// is scalar-shaped storage in the same `tb_scalar_fields` table but a
    /// whole-`Vec` value, not a scalar. Its element access lowers through
    /// `as_tb_vec_field` (the indexed lane); letting it answer here would
    /// make a bare `_tb.mem` a scalar `TbField` and a subscript on it fall
    /// through to the undeclared-name path.
    pub(crate) fn as_tb_scalar_field(&self, e: &AstExpr) -> Option<String> {
        let tb_field = self.ctx.tb_field.as_deref()?;
        let ExprKind::Field { target, name } = &*e.kind else {
            return None;
        };
        let ExprKind::Ident(root) = &*target.kind else {
            return None;
        };
        if root.name != tb_field {
            return None;
        }
        match self.ctx.tb_scalar_fields.get(&name.name) {
            Some(IrType::FixedVec { .. }) | None => None,
            Some(_) => Some(name.name.clone()),
        }
    }

    /// `Some((field, FixedVec type))` when `e` names a fixed-vector
    /// testbench host field — either `_tb.mem` or a bare `mem` in the
    /// test/check/hook capture scope. Drives the element read/write lanes
    /// (`Expr::TbFieldVecElement` / `Stmt::TbFieldVecElementWrite`); the
    /// scalar resolvers above skip these so the two lanes never collide.
    pub(crate) fn as_tb_vec_field(&self, e: &AstExpr) -> Option<(String, IrType)> {
        let name = match &*e.kind {
            // `_tb.mem`
            ExprKind::Field { target, name } => {
                let ExprKind::Ident(root) = &*target.kind else {
                    return None;
                };
                if root.name != self.ctx.tb_field.as_deref()? {
                    return None;
                }
                name.name.clone()
            }
            // bare `mem` in capture scope
            ExprKind::Ident(id) => self.tb_scalar_field_in_capture_scope(&id.name)?,
            _ => return None,
        };
        match self.ctx.tb_scalar_fields.get(&name) {
            Some(ty @ IrType::FixedVec { .. }) => Some((name, ty.clone())),
            _ => None,
        }
    }

    /// The record a `_tb.<field>` / bare `<field>` target names, when
    /// that field is a testbench RECORD field.
    ///
    /// The scalar sibling above resolves only `ctx.tb_scalar_fields`;
    /// a record testbench field lives in `tb_record_fields` and had no
    /// resolver, so a whole-record write to one fell through every
    /// lane to `lower_assign`'s catch-all.
    pub(crate) fn tb_record_field_target(&self, e: &AstExpr) -> Option<crate::ir::RecordId> {
        // Only the `_tb.<field>` spelling. A bare `<field>` cannot
        // reach here: `declare_tb_record_fields` declares every
        // testbench record field as a LOCAL of the run function, so
        // `lookup` claims the name first — which is also why reading
        // and subfield-writing one already worked.
        let ExprKind::Field { target, name } = &*e.kind else {
            return None;
        };
        let ExprKind::Ident(root) = &*target.kind else {
            return None;
        };
        if root.name != self.ctx.tb_field.as_deref()? {
            return None;
        }
        let name = name.name.clone();
        self.ctx
            .tb_record_fields
            .iter()
            .find(|(f, _)| *f == name)
            .map(|(_, rid)| *rid)
    }

    /// Resolve a record-field target root. Supports both bare record
    /// locals (`cur.value`) and desugared testbench record fields
    /// (`_tb.cur.value`), where `cur` is a synthetic local declared at
    /// function entry but emitted as shared test-scope state.
    pub(crate) fn record_target_local(&self, target: &AstExpr) -> Option<crate::ir::LocalId> {
        match &*target.kind {
            ExprKind::Ident(root) => self
                .lookup(&root.name)
                .or_else(|| self.lookup_tb_record_field_in_capture_scope(&root.name)),
            ExprKind::Field { target, name } => {
                let tb_field = self.ctx.tb_field.as_deref()?;
                let ExprKind::Ident(root) = &*target.kind else {
                    return None;
                };
                if root.name != tb_field {
                    return None;
                }
                if !self
                    .ctx
                    .tb_record_fields
                    .iter()
                    .any(|(field, _)| field == &name.name)
                {
                    return None;
                }
                self.lookup_tb_record_field_in_capture_scope(&name.name)
            }
            ExprKind::Paren(inner) => self.record_target_local(inner),
            _ => None,
        }
    }

    /// The record type of a bound-to responder's whole-record state
    /// field, or `None` when the field is a scalar (or absent).
    ///
    /// Reads the same `ctx.target_state` map `as_transactor_state`
    /// resolves against, so the test-scope write lane can make the
    /// record-type check the bare-name lane already made.
    pub(crate) fn target_state_record(
        &self,
        instance: &str,
        field: &str,
    ) -> Option<crate::ir::RecordId> {
        match self.ctx.target_state.get(instance)?.get(field)? {
            crate::ir::StateFieldKind::Record { record } => Some(*record),
            _ => None,
        }
    }

    /// `Some((instance, field))` when the expression is a test-scope
    /// access to a bound-to target responder's persistent state field:
    /// `target.read_count`. The instance is a passive responder bound
    /// in this test; the field must be one of its declared state fields
    /// (an unknown field is a hard error, surfaced precisely). Returns
    /// `None` for any non-matching shape so the caller falls through.
    pub(crate) fn as_transactor_state(&self, e: &AstExpr) -> Option<(String, String)> {
        let ExprKind::Field { target, name } = &*e.kind else {
            return None;
        };
        // Two access shapes carry transactor state:
        //   * `target.read_count` — a test-scope `let` bound-to responder
        //     (not `_tb`-prefixed by the impl-for desugaring), so the
        //     root is the instance name directly;
        //   * `_tb.xact.last_read` — a testbench transactor FIELD (the
        //     impl-for desugaring prepends `_tb`), so the instance name
        //     is the middle segment.
        let instance = match &*target.kind {
            ExprKind::Ident(root) => root.name.clone(),
            ExprKind::Field {
                target: inner,
                name: mid,
            } => {
                let ExprKind::Ident(root) = &*inner.kind else {
                    return None;
                };
                if Some(root.name.as_str()) != self.ctx.tb_field.as_deref() {
                    return None;
                }
                mid.name.clone()
            }
            _ => return None,
        };
        let fields = self.ctx.target_state.get(&instance)?;
        // A SCALAR field is a bare `target.<field>` read; a whole-record
        // field is a `target.<field>` value read (by-value struct copy).
        // A queue field is read via `.size()`/`.empty()`/`.pop()`, and a
        // record SUBFIELD (`target.last.addr`) is handled by the earlier
        // `as_transactor_state_record_field` lane, so both are excluded.
        matches!(
            fields.get(&name.name),
            Some(
                crate::ir::StateFieldKind::Scalar { .. }
                    | crate::ir::StateFieldKind::Record { .. }
                    | crate::ir::StateFieldKind::FixedVec { .. }
            )
        )
        .then(|| (instance, name.name.clone()))
    }

    /// Recognize a test-scope `target.<queue>.size()` / `.empty()` read
    /// on a bound-to responder's persistent `queue<T>` state field (fully
    /// resolved: `instance` is the bound test field). Returns the built
    /// `Expr::TransactorStateQueueQuery`, or `None` for a non-matching
    /// shape. A `.pop()` reaching here (nested deeper than a `let`/assign
    /// RHS) is rejected — it mutates and must be a statement. Mirrors
    /// `lower_scoreboard_query_call` for the test-scope target-state path.
    pub(crate) fn lower_test_state_queue_query(
        &self,
        callee: &AstExpr,
        args: &[crate::ast::CallArg],
    ) -> Result<Option<Expr>, LowerError> {
        let ExprKind::Field { target, name } = &*callee.kind else {
            return Ok(None);
        };
        // `target.<queue>` (or `_tb.xact.<queue>`): reuse the state-root
        // resolution — treat the receiver `target` as the state access `e`.
        let Some((instance, field, kind)) = self.as_transactor_state_any(target) else {
            return Ok(None);
        };
        if !matches!(kind, crate::ir::StateFieldKind::Queue { .. }) {
            return Ok(None);
        }
        let method = name.name.clone();
        let query = match method.as_str() {
            "size" => crate::ir::ScoreboardQuery::QueueSize {
                queue: field.clone(),
            },
            "empty" => crate::ir::ScoreboardQuery::QueueEmpty {
                queue: field.clone(),
            },
            "pop" => {
                return Err(queue_pop_in_expression_position(&format!(
                    "target-state `{instance}.{field}.pop()`"
                )));
            }
            other => {
                return Err(queue_method_in_expression_position(
                    &format!("target-state queue method `{instance}.{field}.{other}(...)`"),
                    other,
                ));
            }
        };
        if !args.is_empty() {
            return Err(LowerError::Invalid(format!(
                "target-state `{instance}.{field}.{method}()` takes no arguments"
            )));
        }
        Ok(Some(Expr::TransactorStateQueueQuery {
            instance,
            field,
            query,
        }))
    }

    /// Like `as_transactor_state` but returns the field KIND too and does
    /// not filter on scalar-ness — the callers (`lower_test_state_queue_
    /// query`, the statement-level test-scope push/pop) select the kind.
    pub(crate) fn as_transactor_state_any(
        &self,
        e: &AstExpr,
    ) -> Option<(String, String, crate::ir::StateFieldKind)> {
        let ExprKind::Field { target, name } = &*e.kind else {
            return None;
        };
        let instance = match &*target.kind {
            ExprKind::Ident(root) => root.name.clone(),
            ExprKind::Field {
                target: inner,
                name: mid,
            } => {
                let ExprKind::Ident(root) = &*inner.kind else {
                    return None;
                };
                if Some(root.name.as_str()) != self.ctx.tb_field.as_deref() {
                    return None;
                }
                mid.name.clone()
            }
            _ => return None,
        };
        let fields = self.ctx.target_state.get(&instance)?;
        fields
            .get(&name.name)
            .map(|kind| (instance, name.name.clone(), kind.clone()))
    }

    /// Resolve a whole fixed-vector state receiver in either a responder
    /// body (`lanes`) or test scope (`target.lanes`).
    pub(crate) fn as_transactor_state_fixed_vec(
        &self,
        e: &AstExpr,
    ) -> Option<(String, String, IrType)> {
        if let ExprKind::Ident(id) = &*e.kind {
            if self.lookup(&id.name).is_none() {
                if let Some(crate::ir::StateFieldKind::FixedVec { ty }) =
                    self.target_state_fields.get(&id.name)
                {
                    return Some((String::new(), id.name.clone(), ty.clone()));
                }
            }
        }
        let (instance, field, kind) = self.as_transactor_state_any(e)?;
        match kind {
            crate::ir::StateFieldKind::FixedVec { ty } => Some((instance, field, ty)),
            _ => None,
        }
    }

    /// Resolve a direct state-vector access through every declared fixed
    /// dimension. Partial selections remain aggregate values and stay fenced;
    /// this path produces only scalar or record elements.
    pub(crate) fn as_transactor_state_fixed_vec_element(
        &mut self,
        e: &AstExpr,
    ) -> Result<Option<TransactorStateVecElement>, LowerError> {
        let mut raw_indices = Vec::new();
        let mut base = e;
        while let ExprKind::Index { target, index } = &*base.kind {
            raw_indices.push(index);
            base = target;
        }
        if raw_indices.is_empty() {
            return Ok(None);
        }
        raw_indices.reverse();
        let Some((instance, field, mut selected_ty)) =
            self.as_transactor_state_fixed_vec(base)
        else {
            return Ok(None);
        };
        let mut indices = Vec::with_capacity(raw_indices.len());
        for raw in raw_indices {
            let IrType::FixedVec { elem, len } = selected_ty else {
                return Err(not_implemented(
                    &format!("indexing past fixed-vector state field `{field}`"),
                    "select no more than the field's declared fixed-vector dimensions",
                    V1Status::EmitsUncompilable,
                ));
            };
            let index = self.lower_record_path_index(raw)?;
            check_literal_vec_index_bounds(&field, &index, len)?;
            indices.push(index);
            selected_ty = *elem;
        }
        if matches!(selected_ty, IrType::FixedVec { .. }) {
            return Err(not_implemented(
                &format!("partial nested fixed-vector state field `{field}` element value"),
                "select through every fixed-vector dimension",
                V1Status::EmitsUncompilable,
            ));
        }
        let index = indices.pop().expect("at least one state-vector index");
        Ok(Some(TransactorStateVecElement {
            instance,
            field,
            mid_indices: indices.into_iter().map(|index| (0, index)).collect(),
            index,
            elem_ty: selected_ty,
        }))
    }

    /// Resolve an AST field-access chain onto a SUB-FIELD of a bound-to
    /// target responder's whole-record state field. Handles all three
    /// access shapes uniformly:
    ///   * `last.addr` — a bare responder-body chain (the record field
    ///     `last` is in `self.target_state_fields`; the instance is a
    ///     placeholder filled at test-binding);
    ///   * `responder.last.addr` — a test-scope `let`-bound responder;
    ///   * `_tb.xact.last.addr` — an impl-form testbench transactor field.
    /// Returns `Ok(None)` when the chain is not a record-state subfield
    /// access (the caller falls through), or `Err` when it IS one but a
    /// segment names no record field / a non-leaf is not a nested record.
    /// The returned `path` is length ≥ 1 (a whole-record access, path
    /// empty, is handled by the scalar `TransactorState` lane).
    pub(crate) fn as_transactor_state_record_field(
        &mut self,
        e: &AstExpr,
    ) -> Result<Option<TransactorStateRecordChain>, LowerError> {
        let Some((root, segs, raw_indices)) = indexed_path_parts(e) else {
            return Ok(None);
        };
        // A local shadows a same-named state field / instance (the
        // established convention throughout this lowerer). Fall through
        // to the record-local field-chain lane in that case.
        if self.lookup(&root.name).is_some() {
            return Ok(None);
        }
        // Resolve (instance, state-field-name, remaining subfield segs).
        // Bare responder-body form: root IS the record state field, so
        // the instance is a placeholder. Otherwise root/`_tb`-prefix
        // names the bound test-scope instance and its state field.
        let (instance, state_field, sub, sub_start) =
            if self.target_state_fields.contains_key(&root.name) {
                // `last.addr` — root is the state field, segs are the subfields.
                (String::new(), root.name.clone(), segs, 0usize)
            } else {
                // Test-scope: `responder.last.addr` (instance=root) or
                // `_tb.xact.last.addr` (instance=segs[0]).
                let (instance, rest_start) =
                    if Some(root.name.as_str()) == self.ctx.tb_field.as_deref() {
                        match segs.first() {
                            Some(mid) => (mid.clone(), 1usize),
                            None => return Ok(None),
                        }
                    } else {
                        (root.name.clone(), 0usize)
                    };
                if !self.ctx.target_state.contains_key(&instance) {
                    return Ok(None);
                }
                let rest = &segs[rest_start..];
                let Some(state_field) = rest.first() else {
                    return Ok(None);
                };
                (
                    instance,
                    state_field.clone(),
                    rest[1..].to_vec(),
                    rest_start + 1,
                )
            };
        // The named state field must exist and be a whole-record field.
        let kind = if instance.is_empty() {
            self.target_state_fields.get(&state_field)
        } else {
            match self.ctx.target_state.get(&instance) {
                Some(fields) => fields.get(&state_field),
                None => return Ok(None),
            }
        };
        let Some(crate::ir::StateFieldKind::Record { record }) = kind else {
            return Ok(None);
        };
        let record = *record;
        // A bare whole-record access (no subfield) is the scalar
        // `TransactorState` lane's job, not this one.
        if sub.is_empty() {
            return Ok(None);
        }
        // Type-check the subfield chain against the record schema,
        // descending through nested records to the leaf.
        if raw_indices.iter().any(|(p, _)| *p < sub_start) {
            return Ok(None);
        }
        let mut indices = Vec::with_capacity(raw_indices.len());
        for (pos, idx) in raw_indices {
            indices.push((pos - sub_start, self.lower_record_path_index(idx)?));
        }
        let mut cur_rid = record;
        let last = sub.len() - 1;
        let mut leaf_vec_len = None;
        let mut leaf_ty = IrType::Unknown;
        for (i, seg) in sub.iter().enumerate() {
            let schema = &self.ctx.records[cur_rid.index()];
            let Some(fld) = schema.field(seg) else {
                return Err(LowerError::Invalid(format!(
                    "record `{}` has no field `{seg}`",
                    schema.name
                )));
            };
            let indexed = indices.iter().any(|(p, _)| *p == i);
            if i == last {
                leaf_vec_len = fld.vec_len;
                leaf_ty = fld.ty.clone();
                break;
            }
            match fld.ty {
                IrType::Record(next) if fld.vec_len.is_none() && !indexed => cur_rid = next,
                IrType::Record(next) if fld.vec_len.is_some() && indexed => {
                    if let Some((_, idx)) = indices.iter().find(|(p, _)| *p == i) {
                        check_literal_vec_index_bounds(seg, idx, fld.vec_len.unwrap_or(0))?;
                    }
                    cur_rid = next;
                }
                // The THIRD lane's copy of the mid-segment split. The
                // record-LOCAL walk (`try_record_field_chain`) and the
                // COMPONENT walk (`as_component_record_field`) already
                // separate these two shapes and word them this way;
                // this one was named in the same commit and left alone.
                // Measured in a bound responder's thread: v1 emits
                // `target.ba.lng.p` / `target.ba.n.p` and g++ refuses
                // each, so the `--codegen v1` this promised was a dead
                // end for every landing it covered.
                IrType::Record(_) if fld.vec_len.is_some() => {
                    return Err(not_implemented(
                        &format!(
                            "traversing the `Vec` record field `{}.{seg}` without an \
                             element index; cannot access `.{}`",
                            schema.name,
                            sub[i + 1]
                        ),
                        format!("select one element first (`{seg}[i].{}`)", sub[i + 1]),
                        V1Status::EmitsUncompilable,
                    ));
                }
                _ => {
                    return Err(not_implemented(
                        &format!(
                            "field access `.{}` on `{}.{seg}`, which is not a nested record",
                            sub[i + 1],
                            schema.name
                        ),
                        "only nested struct/transaction fields can be traversed further"
                            .to_string(),
                        V1Status::EmitsUncompilable,
                    ));
                }
            }
        }
        let mut leaf_indices: Vec<Expr> = indices
            .iter()
            .filter(|(p, _)| *p == last)
            .map(|(_, idx)| idx.clone())
            .collect();
        let leaf_index = leaf_indices.pop();
        let mut mid_indices: Vec<(usize, Expr)> =
            indices.into_iter().filter(|(p, _)| *p < last).collect();
        // All but the final leaf selection belong to the access path; the
        // final selection retains the established `index` slot. This maps
        // `state.grid[i][j]` to one leaf-position mid index plus the final
        // index without changing the shared record access IR.
        for idx in leaf_indices {
            let Some(len) = leaf_vec_len else {
                return Err(not_implemented(
                    &format!("indexing the scalar record state field `{state_field}`"),
                    "only nested `Vec<T, N>` layers are indexable",
                    V1Status::EmitsUncompilable,
                ));
            };
            check_literal_vec_index_bounds(&state_field, &idx, len)?;
            mid_indices.push((last, idx));
            match leaf_ty {
                IrType::FixedVec { len, ref elem } => {
                    leaf_vec_len = Some(len);
                    leaf_ty = (**elem).clone();
                }
                _ => leaf_vec_len = None,
            }
        }
        if let Some(idx) = leaf_index.as_ref() {
            let Some(len) = leaf_vec_len else {
                return Err(not_implemented(
                    &format!("indexing the scalar record state field `{state_field}`"),
                    "only nested `Vec<T, N>` layers are indexable",
                    V1Status::EmitsUncompilable,
                ));
            };
            check_literal_vec_index_bounds(&state_field, idx, len)?;
        }
        Ok(Some(TransactorStateRecordChain {
            instance,
            field: state_field,
            path: sub,
            mid_indices,
            leaf_index,
            leaf_vec_len,
            leaf_ty,
        }))
    }

    /// `Some(PortRef)` (with `lane`) when the expression is a lane
    /// access on a direct DUT port: `dut.<port>[i]`. A constant index
    /// (integer literal, through parens, or via a `const`/enum name)
    /// folds to `LaneIndex::Const`; any other index expression is
    /// lowered as a runtime value into `LaneIndex::Var`, mirroring v1's
    /// `dut_packed_lane`, which re-renders an arbitrary `&Expr`.
    pub(crate) fn as_lane_port_ref(&mut self, e: &AstExpr) -> Result<Option<PortRef>, LowerError> {
        let ExprKind::Index { target, index } = &*e.kind else {
            return Ok(None);
        };
        let Some(mut port) = self.as_port_ref(target)? else {
            return Ok(None);
        };
        port.lane = Some(match self.const_eval_index(index) {
            Some(lane) => crate::ir::LaneIndex::Const(lane),
            None => crate::ir::LaneIndex::Var(Box::new(self.lower_expr(index)?)),
        });
        Ok(Some(port))
    }

    /// Constant-evaluate a lane index: integer literal, parenthesized
    /// literal, or a `const`/enum-variant name.
    pub(crate) fn const_eval_index(&self, e: &AstExpr) -> Option<u64> {
        match &*e.kind {
            ExprKind::Int(s) => parse_int_literal(s),
            ExprKind::Paren(inner) => self.const_eval_index(inner),
            ExprKind::Ident(id) if self.lookup(&id.name).is_none() => {
                self.ctx.consts.get(&id.name).copied()
            }
            _ => None,
        }
    }

    /// Lower a width-method intrinsic call (`recv.trunc<N>()`, ...).
    /// Mirrors v1's `try_emit_width_method`: constant width required,
    /// zero-width rejected, direction checked against the best-effort
    /// receiver width. Destinations through the language's 1024-bit
    /// width-method limit lower to the same `WidthCast` node; storage
    /// selection is a backend concern.
    fn lower_width_method(
        &mut self,
        kind: WidthCastKind,
        kind_name: &str,
        target: &AstExpr,
        args: &[CallArg],
    ) -> Result<Expr, LowerError> {
        let width_expr = match args.first() {
            Some(CallArg::Expr(e)) if args.len() == 1 => e,
            _ => {
                return Err(LowerError::Invalid(format!(
                    "`.{kind_name}<N>()` requires a constant width argument"
                )));
            }
        };
        let Some(width) = const_eval_width(width_expr) else {
            return Err(LowerError::Invalid(format!(
                "`.{kind_name}<N>()` requires a constant integer width"
            )));
        };
        // Best-effort receiver-width inference (v1's
        // `infer_expr_width_best_effort`) for the direction check and
        // the sext shift-fill shape.
        let src_width = self.infer_expr_width(target);
        if let Some(why) = width_method_violation(kind_name, width, src_width) {
            return Err(LowerError::Invalid(why));
        }
        // The direction check above wants the raw inferred width (a
        // zero-width receiver is still a wrong-direction `.trunc<N>()`),
        // but a zero width is not usable *emission* metadata: it would
        // reach the sext shift-fill as `64 - 0` (UB). Record it as
        // unknown, which selects the plain-cast shape instead.
        let src_width = src_width.filter(|w| *w > 0);
        let inner = self.lower_expr(target)?;
        if matches!(self.expr_type(&inner), Some(IrType::FixedVec { .. })) {
            return Err(not_implemented(
                "a scalar width method applied to a fixed-vector local",
                "select a scalar lane before resizing",
                V1Status::EmitsUncompilable,
            ));
        }
        if matches!(self.expr_type(&inner), Some(IrType::Seq(_))) {
            return Err(not_implemented(
                "a scalar width method applied to a dynamic-list local",
                "index the list before resizing",
                V1Status::EmitsUncompilable,
            ));
        }
        Ok(Expr::WidthCast {
            kind,
            width,
            src_width,
            inner: Box::new(inner),
        })
    }

    /// Is this operand held as `harc_rt::HarcWide<N>` rather than a
    /// builtin integer?
    ///
    /// `expr_type` answers for locals, literals, casts and slices, but
    /// returns `None` for every HOST-STATE read — a testbench field, a
    /// component/scoreboard field, a transactor state field. Those are
    /// precisely the reads a wide declared field made reachable, so
    /// they are resolved here against their schemas rather than by
    /// widening `expr_type`, which decides hint and guard questions
    /// throughout lowering and would change all of them at once.
    ///
    /// Deliberately narrow: a shape it cannot resolve answers `false`
    /// and keeps the behaviour that shipped before — the expression
    /// lowers, and if the emitted C++ turns out not to build that is
    /// the pre-existing defect, not a new one. `differential.rs`'s
    /// wide-operator spaces are what say which shapes must be here.
    pub(super) fn is_wide_scalar(&self, e: &Expr) -> bool {
        self.wide_scalar_words(e).is_some()
    }

    /// `HarcWide` word count of a wide operand, or `None` when the
    /// operand is not held as `HarcWide` (or its width is unknown).
    /// Mirrors `wide_scalar_words` in the emitter: the storage class and
    /// its size are one decision, and a guard that disagreed with the
    /// emitter about either would fire on the wrong expressions.
    pub(super) fn wide_scalar_words(&self, e: &Expr) -> Option<u32> {
        match self.scalar_assignment_type(e) {
            Some(IrType::UInt(Some(w)) | IrType::SInt(Some(w)))
                if w > super::BUILTIN_SCALAR_BITS =>
            {
                Some(w.div_ceil(32))
            }
            _ => None,
        }
    }

    /// The scalar type of a HOST-STATE read — a testbench field, a
    /// scoreboard field, a component field (whose member name may be a
    /// DOTTED record path), a transactor state field or one of its
    /// record leaves.
    ///
    /// `expr_type` answers `None` for every one of them, which is why
    /// this exists; `scalar_assignment_type` calls it as its
    /// fallthrough instead of `expr_type`, so the two halves compose:
    /// that function knows the EXPRESSION shapes (bitand narrowing,
    /// shifts, and that a comparison or logical operator yields `Bool`
    /// whatever its operands are), and this one knows the LEAVES it
    /// bottoms out on. Written separately and merged; an earlier
    /// version of this file recursed through `Expr::Binary` here too
    /// and dropped the `Bool` rule in the copy.
    pub(super) fn host_state_scalar_type(&self, e: &Expr) -> Option<IrType> {
        let wide = |ty: Option<&IrType>| -> Option<IrType> { ty.cloned() };
        match e {
            Expr::TbField(name) => wide(self.ctx.tb_scalar_fields.get(name)),
            // `sb.<scalar>`, on a scoreboard-typed testbench field or a
            // scoreboard held inside an env.
            Expr::ScoreboardQuery {
                sb,
                query: crate::ir::ScoreboardQuery::Scalar { scalar },
                ..
            } => {
                let f = self
                    .ctx
                    .scoreboards
                    .get(sb.index())
                    .and_then(|s| s.field(scalar))?;
                match &f.kind {
                    crate::ir::ScoreboardFieldKind::Scalar { ty, .. } => wide(Some(ty)),
                    _ => None,
                }
            }
            // A bound-to transactor's persistent scalar state, read
            // inside a responder body or through the test-scope
            // instance.
            Expr::TransactorState { instance, field } => {
                match self.state_field_kind(instance, field) {
                    Some(crate::ir::StateFieldKind::Scalar { ty, .. }) => wide(Some(ty)),
                    _ => None,
                }
            }
            // A leaf of a whole-record state field. The record schema
            // carries the leaf's declared type, same as any other
            // record field.
            Expr::TransactorStateRecordField {
                instance,
                field,
                path,
                index,
                ..
            } => {
                let Some(kind) = self.state_field_kind(instance, field) else {
                    return None;
                };
                if let crate::ir::StateFieldKind::FixedVec {
                    ty: IrType::FixedVec { elem, .. },
                } = kind
                {
                    return (path.is_empty() && index.is_some())
                        .then(|| (**elem).clone());
                }
                let crate::ir::StateFieldKind::Record { record } = kind else {
                    return None;
                };
                // `index` came from `main`'s indexed record-state
                // support. An INDEXED `Vec` leaf reads as one ELEMENT,
                // so its type is the element type; an unindexed one is
                // a `std::array` and has no scalar type at all. Same
                // `(vec_len, index.is_some())` pairing `expr_type`'s
                // record walk makes.
                wide(
                    self.record_leaf_type(*record, path, index.is_some())
                        .as_ref(),
                )
            }
            Expr::ComponentField { base, field } => {
                let cid = self.component_base_id(base)?;
                // `field` is a member SUFFIX, and for a record leaf it
                // is DOTTED (`cur.w`) — the same thing
                // `ComponentVecElement` says out loud a few hundred
                // lines up. Looking it up whole never matched, so every
                // wide record leaf on a component answered "not wide".
                let mut segs = field.split('.');
                let root = segs.next()?;
                let f = self
                    .ctx
                    .components
                    .get(cid.index())
                    .and_then(|c| c.field(root))?;
                let rest: Vec<String> = segs.map(str::to_string).collect();
                match (&f.kind, rest.is_empty()) {
                    (crate::ir::ComponentFieldKind::Scalar { ty, .. }, true) => wide(Some(ty)),
                    (crate::ir::ComponentFieldKind::Record { record }, false) => {
                        self.record_leaf_type(*record, &rest, false)
                    }
                    _ => None,
                }
            }
            // Expression shapes belong to `scalar_assignment_type`,
            // which calls this only after it has decomposed them.
            _ => self.expr_type(e),
        }
    }

    /// Refuse `/ % < > <= >=` when either operand is wider than a
    /// builtin integer type.
    ///
    /// `HarcWide<N>` defines all six for two operands of the SAME
    /// width, and all six are UNSIGNED. Against an integer (`w < 8`)
    /// they are not defined at all: `HarcWide` converts to `uint64_t`
    /// and to `_harc_u128` equally well, so the call is ambiguous and
    /// g++ rejects it — in both backends, which is how these landed
    /// here. `+ - * & | ^` are defined for the mixed shapes instead,
    /// because those six give the same N-word answer read signed or
    /// unsigned and one implementation is correct for both.
    ///
    /// Defining the other six in the header would be worse than this
    /// refusal, not better: there is no signed-wide compare outside
    /// `harc_wide_slt` (which only covergroup lowering reaches), so
    /// `w < 0` on a negative `sint<1024>` would quietly answer false
    /// rather than fail to build.
    ///
    /// The label is `EmitsUncompilable` because that is measured: v1
    /// emits the same `HarcWide<32> < int` and g++ refuses it too, so
    /// `--codegen v1` is not a way out and must not be offered.
    /// Zero-extend the narrower operand of a mixed-width UNSIGNED
    /// comparison or division so the two reach `HarcWide`'s homogeneous
    /// operators at one width.
    ///
    /// Implicit widening is how every other width pair in the language
    /// already compares: `uint<16> > uint<8>` and `uint<128> > uint<64>`
    /// both lower. Only a pair that crosses into `HarcWide<N>` was
    /// refused, because `HarcWide`'s six ordered/division operators are
    /// homogeneous and nothing widened the narrow side for them. That
    /// made `uint<256> > uint<128>` a hole in an otherwise uniform rule,
    /// not a rule of its own — and it is a REGRESSION: harc#647 shipped
    /// a fixture asserting that comparison and harc#653 started refusing
    /// it (harc#662).
    ///
    /// UNSIGNED both sides, and both widths statically known. That is
    /// the whole precondition, and it is exactly what the refusal's own
    /// text says is missing: "widening the narrower side needs its
    /// signedness, which the C++ type does not carry (zero-extending a
    /// negative `sint` turns -1 into 2^W-1)". The C++ type does not
    /// carry it, but the IR does — for an unsigned operand
    /// zero-extension is the value-preserving widening, so the compare
    /// is exact. A signed operand keeps the refusal, because the wide
    /// carriers are unsigned and the answer would be by magnitude;
    /// that is harc#657's to fix, not this one's.
    ///
    /// `Expr::WidthCast` with `Zext` is the same node `.zext<N>()`
    /// lowers to, and the same one the emitter already builds
    /// `HarcWide<N>` from — measured before writing this: the explicit
    /// spelling `sb.wide > sb.mid.zext<256>()` compiles and runs
    /// correctly today. This inserts what the user would otherwise
    /// have to write.
    fn zext_mixed_width_unsigned_operands(&self, ir_op: BinOp, l: Expr, r: Expr) -> (Expr, Expr) {
        // The homogeneous six, and only those. `== !=` already carry a
        // cross-width `<A, B>` form; `+ - * & | ^` are defined for the
        // mixed shapes; `<< >>` take an integral count and are judged
        // by their own arm.
        if !matches!(
            ir_op,
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Div | BinOp::Mod
        ) {
            return (l, r);
        }
        // At least one side must be `HarcWide`. Below that boundary the
        // pair already lowers, and inserting a cast would churn output
        // for no reason.
        if !self.is_wide_scalar(&l) && !self.is_wide_scalar(&r) {
            return (l, r);
        }
        let unsigned_width = |e: &Expr| match self.scalar_assignment_type(e) {
            Some(IrType::UInt(Some(w))) => Some(w),
            _ => None,
        };
        let (Some(lw), Some(rw)) = (unsigned_width(&l), unsigned_width(&r)) else {
            return (l, r);
        };
        if lw == rw {
            return (l, r);
        }
        let zext = |inner: Expr, from: u32, to: u32| Expr::WidthCast {
            kind: WidthCastKind::Zext,
            width: to,
            src_width: Some(from),
            inner: Box::new(inner),
        };
        if lw < rw {
            (zext(l, lw, rw), r)
        } else {
            (l, zext(r, rw, lw))
        }
    }

    /// Whether a wide `< <= > >= / %` pair is a SIGNED one the emitter
    /// can now build (harc#657), and so must not be refused here.
    ///
    /// Matches the emitter's gate (`expr_is_signed` on both operands): a
    /// strictly-signed operand, or a widthless integer literal — which
    /// sign-extends the same either way and is how `a >> 1 < 0` and
    /// `x < 5` are spelled. At least one side must be strictly signed,
    /// so a plain unsigned wide pair keeps the unsigned zero-extension
    /// path; a signed-vs-unsigned-TYPED mix, which the carriers still
    /// cannot answer, keeps the refusal.
    fn signed_wide_pair_ok(&self, l: &Expr, r: &Expr) -> bool {
        let signed = |e: &Expr| {
            matches!(
                self.scalar_assignment_type(e),
                Some(IrType::SInt(Some(_)) | IrType::SInt(None))
            )
        };
        let signed_ok = |e: &Expr| {
            signed(e)
                || matches!(
                    e,
                    Expr::Literal {
                        ty: IrType::Unknown | IrType::UInt(None),
                        ..
                    }
                )
        };
        (signed(l) || signed(r)) && signed_ok(l) && signed_ok(r)
    }

    fn reject_unbuildable_wide_operator(
        &self,
        op: BinaryOp,
        ir_op: BinOp,
        l: &Expr,
        r: &Expr,
    ) -> Result<(), LowerError> {
        // `+% -% *%` reach `wrap_to_operand_width` first; everything
        // else is judged on the lowered operands.
        let _ = op;
        // Two wide operands of DIFFERENT widths deduce no `N` for the
        // homogeneous `HarcWide` operators, so the shape is unbuildable
        // for every operator EXCEPT `==`/`!=`, which carry their own
        // `<A, B>` form comparing at the wider of the two. Widening for
        // the rest would need the narrower side's signedness, which the
        // C++ type does not carry; see the header note beside
        // `HARC_WIDE_MIXED_OP`.
        //
        // A first version refused this shape for every operator, and
        // claimed `EmitsUncompilable` while v1 built `b == a` perfectly
        // well — a refusal of something that worked, under a label the
        // measurement contradicts.
        //
        // Different-width SIGNED pairs lower now (harc#657): the emitter
        // sign-extends the narrower operand to the wider width before
        // `harc_wide_slt`/`sdiv`/`smod`, and the width-aware `==`/`!=`
        // path does the same — which also fixes the raw-word equality
        // bug this block's comment used to call "NAMED and not fixed"
        // (`-1` as `sint<160>` vs `sint<256>` compared unequal). `==`/
        // `!=` were already let through for the unsigned case.
        let cross_width_ok =
            matches!(ir_op, BinOp::Eq | BinOp::Ne) || self.signed_wide_pair_ok(l, r);
        if let (Some(a), Some(b)) = (self.wide_scalar_words(l), self.wide_scalar_words(r)) {
            if a != b && !cross_width_ok {
                return Err(not_implemented(
                    &format!(
                        "the operator `{}` between scalars of {} and {} words",
                        crate::ir::display::bin_op_str(ir_op),
                        a,
                        b
                    ),
                    "a scalar wider than 128 bits is held as `harc_rt::HarcWide<N>`, whose \
                     operators take two operands of the SAME width; widening the narrower \
                     side needs its signedness, which the C++ type does not carry (zero- \
                     extending a negative `sint` turns -1 into 2^W-1). Give both operands \
                     the same declared width, or narrow one with `.trunc<N>()`. v1 emits \
                     the same expression and its C++ does not compile either, so \
                     `--codegen v1` is not a way out"
                        .to_string(),
                    V1Status::EmitsUncompilable,
                ));
            }
        }
        // A wide SHIFT COUNT lowers. The emitter narrows it to the
        // integral count `HarcWide`'s `<<`/`>>` take, and the result is
        // the one C++ gives for an out-of-range shift, which is what
        // harc#647's fixture asserts (`1 << sb.wide` with `wide = 3` is
        // 8; a count past the value's width is 0).
        //
        // This USED to be refused, on the grounds that a `HarcWide` on
        // the right SFINAEs out of the shift operators and converts to
        // both `uint64_t` and `_harc_u128` — ambiguous. That is a
        // statement about v1's emission, and TB-IR does not emit the
        // ambiguous shape: measured, its C++ compiles and every
        // assertion in that fixture passes. Refusing it made TB-IR
        // lower LESS than v1, which is the wrong direction — TB-IR may
        // exceed v1, never trail it (harc#662).
        if matches!(ir_op, BinOp::Shl | BinOp::Shr) {
            return Ok(());
        }
        let sym = match ir_op {
            BinOp::Div => "/",
            BinOp::Mod => "%",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            _ => return Ok(()),
        };
        // Fire only when EXACTLY ONE side is wide. `HarcWide` defines
        // all six for two operands of the same width, so `a / a` and
        // `a < a + 1` build and run in both backends; it is the
        // HarcWide-against-integer pair that is ambiguous. A first
        // version fired whenever either side was wide and refused
        // those, and no row covered same-width wide-vs-wide for these
        // six, so two review rounds passed over it.
        //
        if self.is_wide_scalar(l) == self.is_wide_scalar(r) {
            return Ok(());
        }
        // A SIGNED wide comparison lowers now (harc#657): the emitter
        // routes `< <= > >=` to `harc_wide_slt` and the `_u128` twin,
        // sign-extending the narrower operand to the wide side's width.
        if self.signed_wide_pair_ok(l, r) {
            return Ok(());
        }
        Err(not_implemented(
            &format!(
                "the operator `{sym}` on a scalar wider than {} bits",
                super::BUILTIN_SCALAR_BITS
            ),
            format!(
                "a scalar wider than {} bits is held as `harc_rt::HarcWide<N>`, which \
                 defines `{sym}` only between two operands of the same width, and only \
                 as an UNSIGNED operation; `+ - * & | ^ == !=` are lowered at any width, as \
                 are `<<`/`>>` with an ordinary integer count. v1 emits the same expression and its C++ does not compile either, \
                 so `--codegen v1` is not a way out",
                super::BUILTIN_SCALAR_BITS
            ),
            V1Status::EmitsUncompilable,
        ))
    }

    /// The `StateFieldKind` of `<instance>.<field>`.
    ///
    /// Inside a responder body `instance` is the EMPTY placeholder that
    /// test-binding fills later, and the builder already carries the
    /// owning transactor's fields in `target_state_fields` — the same
    /// table the bare-ident read above resolves against. From the test
    /// scope `instance` is a real testbench field, in either of the two
    /// instance maps.
    fn state_field_kind(&self, instance: &str, field: &str) -> Option<&crate::ir::StateFieldKind> {
        if instance.is_empty() {
            return self.target_state_fields.get(field);
        }
        let id = self
            .ctx
            .transactor_fields
            .get(instance)
            .or_else(|| self.ctx.target_transactor_fields.get(instance))?;
        self.ctx
            .transactors
            .get(id.index())?
            .state_fields
            .iter()
            .find(|f| f.name == field)
            .map(|f| &f.kind)
    }

    /// The declared type of a record leaf reached by `path` from
    /// `record`. `None` when any step is not a record or the leaf does
    /// not exist.
    fn record_leaf_type(
        &self,
        record: crate::ir::RecordId,
        path: &[String],
        indexed: bool,
    ) -> Option<IrType> {
        let mut cur = record;
        for (i, seg) in path.iter().enumerate() {
            let fld = self.ctx.records.get(cur.index())?.field(seg)?;
            if i + 1 == path.len() {
                // An unindexed `Vec` leaf is a `std::array`, not a
                // scalar of the element type — the same distinction
                // `expr_type`'s record walk makes with
                // `match (fld.vec_len, index.is_some())`. Answering the
                // element type there would call a whole `Vec` field a
                // wide scalar; answering `None` for an INDEXED one
                // would miss a wide element.
                return match (fld.vec_len, indexed) {
                    (Some(_), true) | (None, false) => Some(fld.ty.clone()),
                    _ => None,
                };
            }
            match fld.ty {
                IrType::Record(r) => cur = r,
                _ => return None,
            }
        }
        None
    }

    /// Wrap a lowered `+% / -% / *%` result to `max(W(lhs), W(rhs))` bits
    /// (harc#473). ARCH's wrapping operators take the wider operand's width
    /// as the result width with no widening; the mask is emitted as a
    /// `WidthCast::Trunc`, so codegen produces `(a OP b) & ((1<<W)-1)` for
    /// `W < 64` (and a no-op cast at `W == 64`, since 64 b fills the slot).
    ///
    /// Both operand widths must be statically determinable — literals are
    /// self-sized, typed locals / DUT ports / casts carry their width. If
    /// either operand's width is unknown, lowering fails loudly rather than
    /// silently degrading to the un-wrapped value (the exact hazard the
    /// operator exists to prevent): a scoreboard mirroring a wrapping
    /// datapath would otherwise compute values the DUT can never emit.
    fn wrap_to_operand_width(
        &self,
        op: BinaryOp,
        lhs: &AstExpr,
        rhs: &AstExpr,
        inner: Expr,
    ) -> Result<Expr, LowerError> {
        let sym = match op {
            BinaryOp::AddWrap => "+%",
            BinaryOp::SubWrap => "-%",
            BinaryOp::MulWrap => "*%",
            _ => "+%",
        };
        let wl = self.infer_wrap_operand_width(lhs);
        let wr = self.infer_wrap_operand_width(rhs);
        let (Some(wl), Some(wr)) = (wl, wr) else {
            return Err(LowerError::Invalid(format!(
                "wrapping operator `{sym}` needs both operands to have a statically \
                 known bit-width so the wrap width `max(W(lhs), W(rhs))` is defined \
                 (left is {}, right is {}). Give the operand(s) a scalar type \
                 (`let x : uint<N>`), a cast (`x as uint<N>`), or a width method.",
                if wl.is_some() { "known" } else { "unknown" },
                if wr.is_some() { "known" } else { "unknown" },
            )));
        };
        let width = wl.max(wr);
        if width > 64 {
            // Not a `--codegen v1` escape: v1 has its OWN identical gate
            // (`wrap_mask_width` in `cpp_tb.rs`) that returns an error and
            // emits no C++ for a wrapping op past 64 bits (measured — v1
            // refuses `+%`/`-%`/`*%` at width 128). Both backends reject it.
            return Err(not_implemented(
                &format!("wrapping operator `{sym}` at width {width} (> 64 bits)"),
                "wrapping arithmetic is lowered for operand widths up to 64 bits; \
                 wider datapaths need the `HarcWide<N>` model, which is not wired \
                 through the wrapping mask yet",
                V1Status::Rejects,
            ));
        }
        // `width == 0` can't occur: every determinable width is >= 1.
        Ok(Expr::WidthCast {
            kind: WidthCastKind::Trunc,
            width,
            src_width: None,
            inner: Box::new(inner),
        })
    }

    /// Operand bit-width for a wrapping-op mask. Like `infer_expr_width`
    /// but also resolves DUT/bus port reads (their declared width) and
    /// composes through nested wrapping ops (`(a +% b) *% c`), so a wrap
    /// chain masks at each step's own operand width. Kept separate from
    /// `infer_expr_width` so the `.trunc<N>()` direction check it feeds is
    /// unaffected.
    fn infer_wrap_operand_width(&self, e: &AstExpr) -> Option<u32> {
        match &*e.kind {
            ExprKind::Paren(inner) => self.infer_wrap_operand_width(inner),
            ExprKind::Binary { op, lhs, rhs }
                if matches!(
                    op,
                    BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap
                ) =>
            {
                let l = self.infer_wrap_operand_width(lhs)?;
                let r = self.infer_wrap_operand_width(rhs)?;
                Some(l.max(r))
            }
            // DUT port / bus-bound signal read carries its declared width.
            ExprKind::Field { .. } => self.as_port_ref(e).ok().flatten().and_then(|p| p.width),
            // Everything else shares the receiver-width inference (parens,
            // casts, width methods, literals, typed locals).
            _ => self.infer_expr_width(e),
        }
    }

    /// Best-effort receiver bit-width: parens recurse, `as uint<W>`
    /// casts give W, constant-bounded bit slices give `hi - lo + 1`,
    /// nested width methods give their target width, bare literals give
    /// their minimum unsigned width, and locals resolve through the
    /// typed-`let` width table.
    ///
    /// Kept in step with v1's `infer_expr_width_best_effort`
    /// (`src/codegen/cpp_tb.rs`): the two feed the same direction checks
    /// and the same `sext` shift-fill, so a shape either backend infers
    /// and the other does not is a value divergence, not just a missed
    /// optimization.
    ///
    /// The result can be `Some(0)` for a `uint<0>` receiver. Callers
    /// that store it as `WidthCast::src_width` must filter that out (it
    /// would reach the sext shift-fill as `64 - 0`, UB); callers doing
    /// direction or operand-width checks want the raw value.
    fn infer_expr_width(&self, e: &AstExpr) -> Option<u32> {
        match &*e.kind {
            ExprKind::Paren(inner) => self.infer_expr_width(inner),
            ExprKind::Cast { ty, .. } => cast_relabel_width(ty),
            // A constant-bounded slice is exactly `hi - lo + 1` bits.
            // Without this arm `p[7:0].sext<64>()` looked like an
            // unknown-width receiver and skipped the sign fill, silently
            // yielding `0x00..AB` where v1 produced `0xFF..AB`.
            ExprKind::BitSlice { hi, lo, .. } => {
                let hi = const_eval_width(hi)?;
                let lo = const_eval_width(lo)?;
                (hi >= lo).then_some(hi - lo + 1)
            }
            ExprKind::Call { callee, args } => {
                if let ExprKind::Field { name, .. } = &*callee.kind {
                    if width_cast_kind(&name.name).is_some() {
                        if let Some(CallArg::Expr(w)) = args.first() {
                            return const_eval_width(w);
                        }
                    }
                }
                None
            }
            ExprKind::Int(s) => {
                let v = parse_int_literal(s)?;
                Some(if v == 0 { 1 } else { 64 - v.leading_zeros() })
            }
            ExprKind::Ident(id) => {
                let local = self.lookup(&id.name)?;
                self.let_widths.get(&local).copied()
            }
            _ => None,
        }
    }
}

pub(crate) fn unparen_expr(mut e: &crate::ast::Expr) -> &crate::ast::Expr {
    while let ExprKind::Paren(inner) = &*e.kind {
        e = inner;
    }
    e
}

/// Width-method name → `WidthCastKind`.
pub(crate) fn width_cast_kind(name: &str) -> Option<WidthCastKind> {
    match name {
        "trunc" => Some(WidthCastKind::Trunc),
        "zext" => Some(WidthCastKind::Zext),
        "sext" => Some(WidthCastKind::Sext),
        "resize" => Some(WidthCastKind::Resize),
        _ => None,
    }
}

/// `Some(W)` when the cast target is a scalar `uint<W>`/`sint<W>`/
/// `bits<W>` relabel with W ≤ 128. The 65..128 form is accepted for
/// wide-local/port construction where the surrounding expression or
/// destination carries `_harc_u128` storage; it remains a relabel in the
/// IR expression tree. Width-less
/// scalar casts give 64.
pub(crate) fn cast_relabel_width(ty: &TypeExpr) -> Option<u32> {
    let TypeExpr::Builtin { name, args, .. } = ty else {
        return None;
    };
    if !matches!(
        name,
        BuiltinTy::UInt
            | BuiltinTy::UIntCap
            | BuiltinTy::SInt
            | BuiltinTy::SIntCap
            | BuiltinTy::Bits
    ) {
        return None;
    }
    let width = match args.first() {
        Some(TypeArg::Expr(e)) => match &*e.kind {
            ExprKind::Int(s) => s.replace('_', "").parse::<u32>().ok()?,
            _ => return None,
        },
        Some(_) => return None,
        None => 64,
    };
    (width > 0 && width <= super::BUILTIN_SCALAR_BITS).then_some(width)
}

/// Constant width argument of a width method (v1's `eval_const_width`:
/// integer literal, possibly parenthesized).
fn const_eval_width(e: &AstExpr) -> Option<u32> {
    match &*e.kind {
        ExprKind::Paren(inner) => const_eval_width(inner),
        ExprKind::Int(s) => parse_int_literal(s).and_then(|v| u32::try_from(v).ok()),
        _ => None,
    }
}

pub(crate) fn lower_bin_op(op: BinaryOp) -> Result<BinOp, LowerError> {
    Ok(match op {
        BinaryOp::Add | BinaryOp::AddWrap => BinOp::Add,
        BinaryOp::Sub | BinaryOp::SubWrap => BinOp::Sub,
        BinaryOp::Mul | BinaryOp::MulWrap => BinOp::Mul,
        BinaryOp::Div => BinOp::Div,
        BinaryOp::Mod => BinOp::Mod,
        BinaryOp::Eq => BinOp::Eq,
        BinaryOp::Ne => BinOp::Ne,
        BinaryOp::Lt => BinOp::Lt,
        BinaryOp::Le => BinOp::Le,
        BinaryOp::Gt => BinOp::Gt,
        BinaryOp::Ge => BinOp::Ge,
        BinaryOp::AndAnd | BinaryOp::AndKw => BinOp::And,
        BinaryOp::OrOr | BinaryOp::OrKw => BinOp::Or,
        BinaryOp::BitAnd => BinOp::BitAnd,
        BinaryOp::BitOr => BinOp::BitOr,
        BinaryOp::BitXor => BinOp::BitXor,
        BinaryOp::Shl => BinOp::Shl,
        BinaryOp::Shr => BinOp::Shr,
        // `|->` / `|=>` shape a concurrent check, and
        // `lower_property_check` destructures the TOP-LEVEL one into a
        // `PropertyShape` before any operand reaches here. Reaching this
        // arm means the operator sat somewhere else: a value position
        // (`let x = a |-> b`), or nested inside another implication
        // (`a |-> (b |-> c)`, which is legal property syntax this
        // subset does not lower). v1 accepts both and emits the C++
        // comma operator (`a /* unsupported-op */ , b`), which compiles
        // and evaluates to the right operand alone — the antecedent is
        // silently dropped, so the check runs on half the expression.
        BinaryOp::PipeImplies | BinaryOp::PipeImpliesNext => {
            let sym = if matches!(op, BinaryOp::PipeImpliesNext) {
                "|=>"
            } else {
                "|->"
            };
            return Err(not_implemented(
                &format!("`{sym}` outside the top level of an `assert` / `assume`"),
                "only one implication per check is lowered, and it must be the \
                 outermost operator",
                V1Status::SilentlyMisLowers,
            ));
        }
        // The SVA sequence operators (spec §5). No backend implements
        // them: v1 emits the C++ comma operator, so
        // `a throughout b` compiles into `b` with `a` discarded — the
        // check passes or fails on the wrong expression entirely.
        BinaryOp::Throughout | BinaryOp::Within | BinaryOp::Intersect => {
            let sym = match op {
                BinaryOp::Throughout => "throughout",
                BinaryOp::Within => "within",
                _ => "intersect",
            };
            return Err(not_implemented(
                &format!("the `{sym}` sequence operator"),
                "only `|->` / `|=>` implications and the `past`/`rose`/`fell`/`stable` \
                 readings are lowered",
                V1Status::SilentlyMisLowers,
            ));
        }
        BinaryOp::In | BinaryOp::Inside => {
            return Err(unsupported("`in`/`inside` membership operators", ""));
        }
    })
}

/// Parse a hex literal wider than 64 bits (> 16 hex digits) into
/// LSB-first 32-bit words — v1's `c_wide_lit_words` decomposition,
/// extended down to the 65..=128-bit range (v1 covers that range with
/// a `_harc_u128` composite; the tbir emitter reconstructs the same
/// composite from the words). Returns `None` for non-hex or ≤ 64-bit
/// literals (those take the plain `Expr::Literal` path).
pub(crate) fn parse_wide_hex_literal(s: &str) -> Option<Vec<u32>> {
    let t = s.replace('_', "");
    let hex = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X"))?;
    if hex.len() <= 16 || hex.chars().any(|c| !c.is_ascii_hexdigit()) {
        return None;
    }
    let mut words = Vec::with_capacity(hex.len().div_ceil(8));
    let mut remaining = hex.len();
    while remaining > 0 {
        let start = remaining.saturating_sub(8);
        words.push(u32::from_str_radix(&hex[start..remaining], 16).ok()?);
        remaining = start;
    }
    Some(words)
}

/// Parse the VALUE of a validated hexadecimal sized literal when it needs
/// more than the native 64-bit scalar carrier. The declared width is a
/// representability bound, not zero-padding for general expressions: this
/// mirrors v1's normalized literal value and keeps `128'h1` on the scalar
/// path while `128'h1_0000_0000_0000_0000` becomes three LSB-first words.
fn parse_wide_sized_hex_literal(s: &str) -> Option<Vec<u32>> {
    let t = s.replace('_', "");
    let tick = t.find('\'')?;
    let width = t[..tick].parse::<u32>().ok()?;
    let rest = &t[tick + 1..];
    let digits = rest.strip_prefix('h').or_else(|| rest.strip_prefix('H'))?;
    if width == 0 || digits.is_empty() || digits.chars().any(|c| !c.is_ascii_hexdigit()) {
        return None;
    }

    let significant = digits.trim_start_matches('0');
    if significant.len() <= 16 {
        return None;
    }
    let first_bits = 32 - significant.chars().next()?.to_digit(16)?.leading_zeros();
    let value_bits = (significant.len() as u64 - 1) * 4 + u64::from(first_bits);
    if value_bits > u64::from(width) {
        return None;
    }

    let mut words = Vec::with_capacity(significant.len().div_ceil(8));
    let mut remaining = significant.len();
    while remaining > 0 {
        let start = remaining.saturating_sub(8);
        words.push(u32::from_str_radix(&significant[start..remaining], 16).ok()?);
        remaining = start;
    }
    Some(words)
}

/// Fold an AST expression to a `u64` when it is an integer literal
/// (optionally parenthesized). Used by the target-side `out_of_order
/// tags N` responder lowering to range-check the literal tag count;
/// mirrors v1's `fold_int_literal` over the same surface.
/// The width-method rules — zero width, the language limit, and the
/// direction check — stated ONCE, because they were previously stated
/// three times and the third copy drifted from the other two.
///
/// `cpp_tb.rs` has v1's copy and `lower_width_method` below had TB-IR's;
/// the covergroup path grew a third from intuition and got `resize`
/// wrong (it is direction-agnostic — spec.md says so, and so do the
/// other two copies), omitted the 1024-bit limit entirely, and inferred
/// receiver widths through a constant fold where v1 uses literals only.
/// Each of those was a separate review finding. A rule this codebase
/// states more than once has drifted every time.
///
/// Returns the canonical sentence when a rule is violated, so both
/// callers render identical text and only prepend their own context.
/// `None` means the width is admissible — NOT that it lowers; a width
/// above 64 is still outside the covergroup value model, which is that
/// caller's own concern.
pub(crate) fn width_method_violation(
    kind_name: &str,
    width: u32,
    src_width: Option<u32>,
) -> Option<String> {
    if width == 0 {
        return Some(format!(
            "`.{kind_name}<{width}>()`: width must be greater than zero"
        ));
    }
    if width > crate::MAX_WIDTH_METHOD_BITS {
        return Some(format!(
            "`.{kind_name}<{width}>()`: destination width exceeds the {}-bit language limit",
            crate::MAX_WIDTH_METHOD_BITS
        ));
    }
    // `resize` is deliberately absent from the direction check: it
    // narrows or widens as asked. Stated here so the exception lives in
    // one place too.
    let sw = src_width?;
    match kind_name {
        "trunc" if width >= sw => Some(format!(
            "`.trunc<{width}>()` on a {sw}-bit value: width must be strictly less than \
             the source width (otherwise it's a no-op or wrong-direction). Use \
             `.zext<{width}>()` to widen, or remove the cast if you meant a no-op."
        )),
        "zext" | "sext" if width < sw => Some(format!(
            "`.{kind_name}<{width}>()` on a {sw}-bit value: width must be ≥ the source \
             width (otherwise it narrows, wrong direction). Use `.trunc<{width}>()` to \
             narrow."
        )),
        _ => None,
    }
}

pub(crate) fn parse_int_literal_expr(e: &crate::ast::Expr) -> Option<u64> {
    match &*e.kind {
        crate::ast::ExprKind::Int(s) => parse_int_literal(s),
        crate::ast::ExprKind::Paren(inner) => parse_int_literal_expr(inner),
        _ => None,
    }
}

/// Reject a LITERAL `Vec` element index that is statically out of range
/// (`tbl.entries[9]` on a `Vec<T, 4>` field). Without this, the access
/// lowers cleanly and both backends emit `std::array` UB at runtime (v1's
/// textual emission has the same hole), so this is `Invalid` — a
/// statically wrong program, NOT a subset gap — and must NOT suggest
/// `--codegen v1`. A non-literal index passes through unchanged (runtime
/// range behavior is the backends', as before).
pub(crate) fn check_literal_vec_index_bounds(
    dotted: &str,
    idx: &Expr,
    len: usize,
) -> Result<(), LowerError> {
    if len == 0 {
        return Err(LowerError::Invalid(format!(
            "cannot index `Vec` record field `{dotted}` of length 0 (an empty vector has no valid indices)"
        )));
    }
    let Expr::Literal { value, .. } = idx else {
        return Ok(());
    };
    if (*value as u128) < len as u128 {
        return Ok(());
    }
    Err(LowerError::Invalid(format!(
        "element index {value} is out of range for `Vec` record field \
         `{dotted}` of length {len} (valid indices are 0..={})",
        len.saturating_sub(1)
    )))
}

pub(crate) fn check_literal_tb_vec_index_bounds(
    field: &str,
    idx: &Expr,
    len: usize,
) -> Result<(), LowerError> {
    if len == 0 {
        return Err(LowerError::Invalid(format!(
            "cannot index testbench `Vec` field `{field}` of length 0 (an empty vector has no valid indices)"
        )));
    }
    let Expr::Literal { value, .. } = idx else {
        return Ok(());
    };
    if (*value as u128) < len as u128 {
        return Ok(());
    }
    Err(LowerError::Invalid(format!(
        "element index {value} is out of range for testbench `Vec` field \
         `{field}` of length {len} (valid indices are 0..={})",
        len.saturating_sub(1)
    )))
}

pub(crate) fn check_literal_component_vec_index_bounds(
    base: &crate::ir::ComponentBase,
    field: &str,
    idx: &Expr,
    len: usize,
) -> Result<(), LowerError> {
    let access = match base {
        crate::ir::ComponentBase::SelfField => field.to_string(),
        crate::ir::ComponentBase::Path(path) => {
            format!("{}.{field}", path.join("."))
        }
        crate::ir::ComponentBase::Local(_) => field.to_string(),
    };
    if len == 0 {
        return Err(LowerError::Invalid(format!(
            "cannot index component `Vec` field `{access}` of length 0 (an empty vector has no valid indices)"
        )));
    }
    let Expr::Literal { value, .. } = idx else {
        return Ok(());
    };
    if (*value as u128) < len as u128 {
        return Ok(());
    }
    Err(LowerError::Invalid(format!(
        "element index {value} is out of range for component `Vec` field \
         `{access}` of length {len} (valid indices are 0..={})",
        len.saturating_sub(1)
    )))
}

fn port_temp_type(p: &PortRef, hint: Option<&IrType>) -> Option<IrType> {
    if let Some(w) = p.width {
        return Some(IrType::UInt(Some(w)));
    }
    match hint {
        Some(IrType::UInt(Some(w)) | IrType::SInt(Some(w))) if *w > 64 => {
            Some(IrType::UInt(Some(*w)))
        }
        Some(IrType::Bool) => Some(IrType::Bool),
        _ => None,
    }
}

pub(crate) fn wide_literal_bits(words: &[u32]) -> u32 {
    let Some((idx, word)) = words.iter().enumerate().rev().find(|(_, w)| **w != 0) else {
        return 1;
    };
    (idx as u32) * 32 + (32 - word.leading_zeros())
}

/// Parse a plain integer literal (decimal / 0x / 0b / 0o, `_`
/// separators). Verilog-style sized literals use
/// [`parse_sized_int_literal_with_width`] so callers cannot accidentally
/// discard their declared width.
pub(crate) fn parse_int_literal(s: &str) -> Option<u64> {
    parse_int_literal_checked(s).ok()
}

/// Why an integer literal did not become a `u64`.
///
/// `parse_int_literal` answers `None` for both, and a caller that has to
/// tell them apart was reading the digits itself to decide — which got
/// the hex spelling wrong, grading `0x1_0000_0000_0000_0000` as "a
/// non-integer initializer" while the decimal spelling of the same value
/// was graded correctly. One parse, two answers.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum IntLiteralErr {
    /// A well-formed integer literal whose value exceeds `u64`.
    Overflows,
    /// Not an integer literal in any supported base.
    NotAnInteger,
}

pub(crate) fn parse_int_literal_checked(s: &str) -> Result<u64, IntLiteralErr> {
    let t = s.replace('_', "");
    let (digits, radix) = match () {
        _ if t.starts_with("0x") || t.starts_with("0X") => (&t[2..], 16),
        _ if t.starts_with("0b") || t.starts_with("0B") => (&t[2..], 2),
        _ if t.starts_with("0o") || t.starts_with("0O") => (&t[2..], 8),
        _ => (&t[..], 10),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return Err(IntLiteralErr::NotAnInteger);
    }
    u64::from_str_radix(digits, radix).map_err(|_| IntLiteralErr::Overflows)
}

/// Parse the VALUE of a validated HARC/Verilog-style sized literal such
/// as `8'hAB`, `8'd42`, or `4'b1010` when it fits the scalar IR domain.
///
/// This deliberately stays separate from [`parse_int_literal`]: callers that
/// only need an index, sampled value, metadata value, or v1-compatible host
/// scalar may discard the width explicitly; width-aware paths call
/// [`parse_sized_int_literal_with_width`].
pub(crate) fn parse_sized_int_literal(s: &str) -> Option<u64> {
    parse_sized_int_literal_with_width(s).map(|(_, value)| value)
}

/// Whether a general AST expression contains a HARC/Verilog-sized literal.
/// The value IR deliberately erases this source provenance when matching v1's
/// host-scalar semantics; host-width unary bit-not still needs the distinction.
pub(crate) fn ast_expr_contains_sized_literal(e: &AstExpr) -> bool {
    match &*e.kind {
        ExprKind::Int(text) => text.contains('\''),
        ExprKind::Paren(inner) | ExprKind::Unary { expr: inner, .. } => {
            ast_expr_contains_sized_literal(inner)
        }
        ExprKind::Cast { expr, .. } => ast_expr_contains_sized_literal(expr),
        ExprKind::Field { target, .. } => ast_expr_contains_sized_literal(target),
        ExprKind::Index { target, index } => {
            ast_expr_contains_sized_literal(target) || ast_expr_contains_sized_literal(index)
        }
        ExprKind::BitSlice { target, hi, lo } => {
            ast_expr_contains_sized_literal(target)
                || ast_expr_contains_sized_literal(hi)
                || ast_expr_contains_sized_literal(lo)
        }
        ExprKind::Call { callee, args } => {
            ast_expr_contains_sized_literal(callee)
                || args.iter().any(|arg| match arg {
                    CallArg::Expr(value) | CallArg::Named { value, .. } => {
                        ast_expr_contains_sized_literal(value)
                    }
                })
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            ast_expr_contains_sized_literal(lhs) || ast_expr_contains_sized_literal(rhs)
        }
        ExprKind::Ternary {
            cond,
            then_branch,
            else_branch,
        } => {
            ast_expr_contains_sized_literal(cond)
                || ast_expr_contains_sized_literal(then_branch)
                || ast_expr_contains_sized_literal(else_branch)
        }
        ExprKind::SystemCall { args, .. } | ExprKind::SolveOrder { args } => {
            args.iter().any(ast_expr_contains_sized_literal)
        }
        ExprKind::NamedArg { value, .. } => ast_expr_contains_sized_literal(value),
        ExprKind::Membership { expr, set } => {
            ast_expr_contains_sized_literal(expr) || ast_expr_contains_sized_literal(set)
        }
        _ => false,
    }
}

/// A syntactically valid sized h/d/b literal whose value cannot fit the
/// scalar index carrier. Such a value is necessarily outside every host
/// `Vec` length, so record-path selection diagnoses it instead of suggesting
/// v1's unchecked `std::array::operator[]` emission.
fn sized_int_literal_overflows_u64(s: &str) -> bool {
    let t = s.replace('_', "");
    let Some(tick) = t.find('\'') else {
        return false;
    };
    if t[..tick].parse::<u32>().is_err() {
        return false;
    }
    let rest = &t[tick + 1..];
    let Some(radix_ch) = rest.chars().next() else {
        return false;
    };
    let radix = match radix_ch {
        'h' | 'H' => 16,
        'd' | 'D' => 10,
        'b' | 'B' => 2,
        _ => return false,
    };
    let digits = &rest[radix_ch.len_utf8()..];
    !digits.is_empty()
        && digits.chars().all(|ch| ch.is_digit(radix))
        && u64::from_str_radix(digits, radix).is_err()
}

/// Parse both the declared width and value of a sized literal. Coverpoint
/// lowering retains the width on `Expr::Literal` so later width methods do
/// not mistake `4'd15` for an untyped 64-bit value.
pub(crate) fn parse_sized_int_literal_with_width(s: &str) -> Option<(u32, u64)> {
    let t = s.replace('_', "");
    let tick = t.find('\'')?;
    let width = t[..tick].parse::<u32>().ok()?;
    let rest = &t[tick + 1..];
    let (radix_ch, digits) = rest.split_at(rest.chars().next()?.len_utf8());
    let radix = match radix_ch {
        "h" | "H" => 16,
        "d" | "D" => 10,
        "b" | "B" => 2,
        _ => return None,
    };
    let value = u64::from_str_radix(digits, radix).ok()?;
    Some((width, value))
}

/// True when `e` contains a transactor call edge selected by `is_bound`.
/// Re-evaluated predicates pass the bus-binding table here: synchronous
/// sibling and testbench-instance methods can remain inline, while a bus/TLM
/// handshake still needs the statement-level call seam.
pub(crate) fn expr_has_bound_transactor_edge<F>(e: &Expr, is_bound: &F) -> bool
where
    F: Fn(&str) -> bool,
{
    match e {
        Expr::Call(
            crate::ir::CallTarget::TransactorMethod { bus_field, .. },
            _,
        ) => is_bound(bus_field),
        Expr::Call(crate::ir::CallTarget::TransactorSelfMethod { .. }, _) => false,
        Expr::Call(_, args) => args
            .iter()
            .any(|arg| expr_has_bound_transactor_edge(arg, is_bound)),
        Expr::Binary(_, a, b) => {
            expr_has_bound_transactor_edge(a, is_bound)
                || expr_has_bound_transactor_edge(b, is_bound)
        }
        Expr::Unary(_, a) => expr_has_bound_transactor_edge(a, is_bound),
        Expr::Ternary(c, t, f) => {
            expr_has_bound_transactor_edge(c, is_bound)
                || expr_has_bound_transactor_edge(t, is_bound)
                || expr_has_bound_transactor_edge(f, is_bound)
        }
        Expr::WidthCast { inner, .. } => expr_has_bound_transactor_edge(inner, is_bound),
        Expr::ComponentIdle { n, .. } | Expr::TransactorIdle { n, .. } => {
            expr_has_bound_transactor_edge(n, is_bound)
        }
        Expr::SeqIndex { index, .. } => expr_has_bound_transactor_edge(index, is_bound),
        // A transactor call nested in a fixed-vector element INDEX
        // (`mem[xt.idx()]` / `sb.v[xt.idx()]`) reaches a `wait until`
        // predicate wrapped in a vec node, not bare. Without recursing
        // here the predicate scanner misses a bound-instance call: its
        // honest refusal is skipped and the un-hoistable call surfaces
        // later as a verifier `BadTransactorCall` instead. A sibling call
        // is deliberately admitted and re-evaluated.
        Expr::TbFieldVecElement {
            index, inner_index, ..
        }
        | Expr::ComponentVecElement {
            index, inner_index, ..
        } => {
            expr_has_bound_transactor_edge(index, is_bound)
                || inner_index
                    .as_deref()
                    .is_some_and(|inner| expr_has_bound_transactor_edge(inner, is_bound))
        }
        Expr::BitSlice { target, .. } => expr_has_bound_transactor_edge(target, is_bound),
        Expr::BitSliceDyn { target, hi, lo } => {
            expr_has_bound_transactor_edge(target, is_bound)
                || expr_has_bound_transactor_edge(hi, is_bound)
                || expr_has_bound_transactor_edge(lo, is_bound)
        }
        Expr::CovHookParam {
            index: Some(index), ..
        } => expr_has_bound_transactor_edge(index, is_bound),
        _ => false,
    }
}
